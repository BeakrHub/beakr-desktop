//! The JavaScript "gather script" injected into the benchling.com webview.
//!
//! This script runs in the page context of https://benchling.com after the user
//! has logged in with their own Benchling session. It uses the logged-in session
//! cookie (sent automatically by the browser because requests use
//! `credentials: "include"` on the benchling.com origin) to call Benchling's
//! INTERNAL REST API at `/1/api/*`.
//!
//! The full contract below was confirmed against a live, populated session on
//! 2026-06-18. This is the ONE place where Benchling's internal API shapes live.
//!
//! CONFIRMED ENDPOINTS:
//!   GET /1/api/users/me                                   -> { handle, email, ... }
//!   GET /1/api/folders                                    -> [ { api_identifier:"lib_..", id, name, source } ]
//!   GET /1/api/folders/actions/get-nested-files?ids[]=<lib_..>
//!        -> { files: [ { id, name } ] }    (NOTE: param is ids[]=, NOT folderId=)
//!   GET /1/api/entries/<etr_..>?view=true   -> entry with entryDays[].note.noteParts[]
//!   GET /1/api/protocols/<prt_..>           -> protocol with description + materials
//!   GET /1/api/sequences/<seq_..>           -> sequence with bases + annotations[]
//!   GET /1/api/files/<file_..>              -> uploaded file (name + link only)
//!
//! Item type is the Benchling id prefix: etr_=entry, prt_=protocol, seq_=sequence,
//! file_=uploaded file. A files[] item only carries { id, name }; per-item content
//! is a follow-up GET to the type-specific endpoint above.
//!
//! Idle/expired sessions return 401 with `sessionIdle:true` -> surfaced as
//! needs_login so the user can re-authenticate.
//!
//! Data bridge: the script POSTs the gathered JSON to a localhost HTTP endpoint
//! served by the Rust side (`http://127.0.0.1:<port>/benchling/ingest`). We do NOT
//! expose Tauri IPC to the remote benchling.com origin; the localhost listener
//! responds with `Access-Control-Allow-Origin: *` so the page-context fetch
//! succeeds. The `__BEAKR_BRIDGE_PORT__` token is substituted by Rust before
//! injection (see `benchling::commands`).

/// Placeholder token replaced by Rust with the localhost bridge port.
pub const PORT_PLACEHOLDER: &str = "__BEAKR_BRIDGE_PORT__";

/// The raw gather script. Rust substitutes `PORT_PLACEHOLDER` before `webview.eval`.
pub const BENCHLING_GATHER_SCRIPT: &str = r#####"
(async function beakrBenchlingGather() {
  const BRIDGE_PORT = "__BEAKR_BRIDGE_PORT__";
  const BRIDGE_URL = "http://127.0.0.1:" + BRIDGE_PORT + "/benchling/ingest";
  const API = "/1/api";
  // Cap very long sequence base strings so a genome-sized record can't bloat the
  // payload. The backend stores content to S3, but keep the bridge POST sane.
  const MAX_BASES = 200000;
  // get-nested-files returns content recursively, so the same item can appear
  // under multiple projects. Dedup by Benchling id to avoid redundant detail
  // fetches and duplicate payload entries (the backend also dedups by id).
  const seenIds = new Set();

  // ---- bridge helpers -------------------------------------------------------
  async function postBridge(payload) {
    try {
      await fetch(BRIDGE_URL, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(payload),
      });
    } catch (e) {
      console.error("[beakr] bridge post failed", e);
    }
  }
  function report(stage, message, extra) {
    return postBridge(Object.assign({ type: "progress", stage: stage, message: message }, extra || {}));
  }

  // ---- session / auth helpers ----------------------------------------------
  function csrfToken() {
    const el = document.querySelector('meta[name="csrf-token"]');
    return el ? el.getAttribute("content") : null;
  }

  async function apiGet(path) {
    const headers = { "X-Requested-With": "XMLHttpRequest", "Accept": "application/json" };
    const token = csrfToken();
    if (token) headers["X-CSRFToken"] = token;
    const resp = await fetch(API + path, { method: "GET", credentials: "include", headers: headers });
    // Benchling signals an idle/expired session with a 401 + sessionIdle:true.
    if (resp.status === 401) {
      let body = null;
      try { body = await resp.json(); } catch (e) {}
      const err = new Error("needs_login");
      err.needsLogin = true;
      err.sessionIdle = !!(body && (body.sessionIdle === true ||
        (typeof body.message === "string" && body.message.indexOf("not logged in") !== -1)));
      throw err;
    }
    if (!resp.ok) {
      const err = new Error("benchling api error " + resp.status + " for " + path);
      err.status = resp.status;
      throw err;
    }
    return resp.json();
  }

  // ---- helpers --------------------------------------------------------------
  function abs(url) {
    if (!url) return null;
    if (/^https?:\/\//.test(url)) return url;
    let u = "https://benchling.com" + (url.startsWith("/") ? "" : "/") + url;
    if (u.endsWith("/edit")) u = u.slice(0, -5); // editURL -> view URL
    return u;
  }

  function pickArray(resp, keys) {
    if (Array.isArray(resp)) return resp;
    if (resp && typeof resp === "object") {
      for (const k of keys) { if (Array.isArray(resp[k])) return resp[k]; }
      for (const k of Object.keys(resp)) { if (Array.isArray(resp[k])) return resp[k]; }
    }
    return [];
  }

  // Item type from Benchling id prefix (confirmed 2026-06-18).
  function kindForId(id) {
    if (!id) return "file";
    if (id.indexOf("etr_") === 0) return "entry";
    if (id.indexOf("prt_") === 0) return "protocol";
    if (id.indexOf("seq_") === 0) return "dna_sequence";
    if (id.indexOf("file_") === 0) return "file";
    return "file";
  }

  // ---- per-type content formatters -----------------------------------------
  // A notebook table notePart: { table: { name, table: [[{text, cachedValue}]] } }.
  function tableToText(tbl) {
    if (!tbl || !Array.isArray(tbl.table)) return "";
    return tbl.table.map(function (row) {
      if (!Array.isArray(row)) return "";
      return row.map(function (c) {
        if (c == null) return "";
        const v = (c.text != null && c.text !== "") ? c.text : (c.cachedValue != null ? c.cachedValue : "");
        return String(v);
      }).join(" | ");
    }).join("\n");
  }

  // One notePart -> markdown. Types seen: text, table, code, note_linked_object.
  function notePartToText(p) {
    if (!p) return "";
    if (p.type === "table" && p.table) return tableToText(p.table);
    if (p.type === "code") return p.code ? ("```\n" + p.code + "\n```") : (p.text || "");
    const t = (typeof p.text === "string") ? p.text : "";
    if (!t) return "";
    const indent = "  ".repeat(Math.max(0, p.indentation || 0));
    if (p.headerType) return "## " + t;
    if (p.listType) {
      if (p.listType.indexOf("checkbox-checked") === 0) return indent + "- [x] " + t;
      if (p.listType.indexOf("checkbox") === 0) return indent + "- [ ] " + t;
      return indent + "- " + t;
    }
    return indent ? (indent + t) : t;
  }

  // Entry note tree -> markdown: entryDays[] -> note.noteParts[].
  function entryToText(entry) {
    const out = [];
    const days = Array.isArray(entry.entryDays) ? entry.entryDays : [];
    days.forEach(function (day) {
      if (day.title) out.push("\n## " + day.title);
      else if (day.date) out.push("\n### " + day.date);
      const parts = (day.note && Array.isArray(day.note.noteParts)) ? day.note.noteParts : [];
      parts.forEach(function (p) {
        const s = notePartToText(p);
        if (s) out.push(s);
      });
    });
    return out.join("\n").trim() || (entry.name || "");
  }

  function protocolToText(p) {
    const out = [];
    if (p.name) out.push("# " + p.name);
    if (typeof p.description === "string" && p.description) out.push(p.description);
    else if (p.description != null && typeof p.description === "object") out.push(JSON.stringify(p.description));
    if (Array.isArray(p.materials) && p.materials.length) {
      out.push("\n## Materials");
      p.materials.forEach(function (m) { out.push("- " + (m.name || m.text || JSON.stringify(m))); });
    }
    return out.join("\n").trim() || (p.name || "");
  }

  function sequenceToText(s) {
    const out = [];
    if (s.name) out.push("# " + s.name);
    if (s.length != null) out.push("Length: " + s.length);
    if (s.circular != null) out.push("Topology: " + (s.circular ? "circular" : "linear"));
    if (Array.isArray(s.annotations) && s.annotations.length) {
      out.push("\n## Annotations");
      s.annotations.forEach(function (a) {
        out.push("- " + (a.name || "(unnamed)") + " [" + (a.annotation_type || a.type || "") +
          " " + a.start + "-" + a.end + " strand " + a.strand + "]");
      });
    }
    if (typeof s.bases === "string" && s.bases) {
      out.push("\n## Sequence");
      out.push(s.bases.length > MAX_BASES ? (s.bases.slice(0, MAX_BASES) + "... [truncated]") : s.bases);
    }
    return out.join("\n").trim() || (s.name || "");
  }

  function fileToText(detail, f) {
    const name = (detail && detail.name) || f.name || "";
    const link = abs((detail && detail.editURL) || "");
    return name + (link ? ("\n" + link) : "") + "\n(Uploaded file — open in Benchling to view its contents.)";
  }

  // ---- detail fetch + item mapping -----------------------------------------
  async function fetchDetail(id, kind) {
    let path = null;
    if (kind === "entry") path = "/entries/" + encodeURIComponent(id) + "?view=true";
    else if (kind === "protocol") path = "/protocols/" + encodeURIComponent(id);
    else if (kind === "dna_sequence" || kind === "aa_sequence") path = "/sequences/" + encodeURIComponent(id);
    else if (kind === "file") path = "/files/" + encodeURIComponent(id);
    if (!path) return null;
    try { return await apiGet(path); }
    catch (e) { if (e && e.needsLogin) throw e; return null; }
  }

  function mapFile(f, detail, folder) {
    const id = f.id || f.api_identifier || "";
    const kind = kindForId(id);
    let content;
    if (kind === "entry" && detail) content = entryToText(detail);
    else if (kind === "protocol" && detail) content = protocolToText(detail);
    else if ((kind === "dna_sequence" || kind === "aa_sequence") && detail) content = sequenceToText(detail);
    else if (kind === "file") content = fileToText(detail, f);
    else content = (detail && detail.name) || f.name || "";
    return {
      external_id: id,
      kind: kind,
      title: (detail && detail.name) || f.name || "(untitled)",
      url: abs((detail && (detail.editURL || detail.url || detail.webURL)) || (id ? "/" + id : null)),
      content: content,
      checksum: String((detail && (detail.modified_at || detail.modifiedAt)) || f.modified_at || f.modifiedAt || id || ""),
      metadata: { folder_id: folder.external_id, folder_name: folder.title },
    };
  }

  function mapFolder(f) {
    const id = f.api_identifier || f.id || (f.source && f.source.id) || "";
    return {
      external_id: id,
      kind: "folder",
      title: f.name || f.displayName || "(untitled folder)",
      url: abs(f.webURL || f.url || (id ? "/" + id : null)),
      content: f.description || f.name || "",
      checksum: String(f.modifiedAt || f.updatedAt || f.modified_at || id || ""),
      metadata: { source_id: f.source && f.source.id ? f.source.id : null },
    };
  }

  // List a folder's nested files via get-nested-files?ids[]=<lib_..>, then fetch
  // each file's content from its type-specific endpoint.
  async function fetchFolderItems(folder) {
    const items = [];
    const fid = encodeURIComponent(folder.external_id);
    let nextToken = null;
    let guard = 0;
    do {
      guard += 1;
      let resp;
      const path = "/folders/actions/get-nested-files?ids[]=" + fid +
        (nextToken ? "&nextToken=" + encodeURIComponent(nextToken) : "");
      try {
        resp = await apiGet(path);
      } catch (e) {
        if (e && e.needsLogin) throw e;
        break; // folder not listable for this account — skip its contents
      }
      const files = pickArray(resp, ["files", "items", "results", "data"]);
      for (const f of files) {
        const id = f.id || f.api_identifier;
        if (!id || seenIds.has(id)) continue;
        seenIds.add(id);
        const detail = await fetchDetail(id, kindForId(id));
        items.push(mapFile(f, detail, folder));
      }
      nextToken = (resp && (resp.nextToken || resp.next_token)) || null;
    } while (nextToken && guard < 50);
    return items;
  }

  // ---- main flow ------------------------------------------------------------
  try {
    await report("verify", "Verifying Benchling login");
    let me;
    try {
      me = await apiGet("/users/me");
    } catch (e) {
      if (e && e.needsLogin) {
        await postBridge({ type: "needs_login", message: "Please log in to Benchling, then click Import." });
        return;
      }
      throw e;
    }
    const handle = (me && (me.handle || me.username || (me.email ? me.email.split("@")[0] : null))) || "unknown";

    await report("folders", "Listing folders and projects");
    let folderResp;
    try {
      folderResp = await apiGet("/folders");
    } catch (e) {
      if (e && e.needsLogin) {
        await postBridge({ type: "needs_login", message: "Benchling session expired. Please log in again." });
        return;
      }
      throw e;
    }
    const rawFolders = pickArray(folderResp, ["folders", "results", "data", "items"]);
    const folders = rawFolders.map(mapFolder).filter(function (f) { return f.external_id; });

    const items = [];
    folders.forEach(function (f) { items.push(f); }); // each folder is itself an item

    let i = 0;
    for (const folder of folders) {
      i += 1;
      await report("scan", "Scanning folder " + i + " of " + folders.length + ": " + folder.title, {
        current: i, total: folders.length,
      });
      try {
        const folderItems = await fetchFolderItems(folder);
        folderItems.forEach(function (it) { if (it.external_id) items.push(it); });
      } catch (e) {
        if (e && e.needsLogin) {
          await postBridge({ type: "needs_login", message: "Benchling session expired. Please log in again." });
          return;
        }
        console.error("[beakr] folder scan failed", folder.external_id, e);
      }
    }

    await postBridge({
      type: "complete",
      user_handle: handle,
      tenant_host: "benchling.com",
      items: items,
    });
  } catch (e) {
    await postBridge({ type: "error", message: (e && e.message) ? e.message : String(e) });
  }
})();
"#####;
