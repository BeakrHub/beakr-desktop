//! The JavaScript "gather script" injected into the benchling.com webview.
//!
//! This script runs in the page context of https://benchling.com after the user
//! has logged in with their own Benchling session. It uses the logged-in session
//! cookie (sent automatically by the browser because requests use
//! `credentials: "include"` on the benchling.com origin) to call Benchling's
//! INTERNAL REST API at `/1/api/*`.
//!
//! IMPORTANT — this is the ONE place where Benchling's internal API shapes live.
//! Everything uncertain about Benchling's entry/sequence endpoints is isolated in
//! the `fetchFolderItems` function below (see the big comment block there). When a
//! fresh + populated Benchling account is available to confirm the exact query
//! params and response shapes, edit ONLY that function.
//!
//! Data bridge: the script POSTs the gathered JSON to a localhost HTTP endpoint
//! served by the Rust side (`http://127.0.0.1:<port>/benchling/ingest`). We do NOT
//! expose Tauri IPC to the remote benchling.com origin; the localhost listener
//! responds with `Access-Control-Allow-Origin: *` so the page-context fetch
//! succeeds. The `__BEAKR_BRIDGE_PORT__` / `__BEAKR_USER_HANDLE_HINT__` tokens are
//! substituted by Rust before injection (see `benchling::commands`).

/// Placeholder token replaced by Rust with the localhost bridge port.
pub const PORT_PLACEHOLDER: &str = "__BEAKR_BRIDGE_PORT__";

/// The raw gather script. Rust substitutes `PORT_PLACEHOLDER` before `webview.eval`.
pub const BENCHLING_GATHER_SCRIPT: &str = r#####"
(async function beakrBenchlingGather() {
  const BRIDGE_PORT = "__BEAKR_BRIDGE_PORT__";
  const BRIDGE_URL = "http://127.0.0.1:" + BRIDGE_PORT + "/benchling/ingest";
  const API = "/1/api";

  // ---- bridge helpers -------------------------------------------------------
  async function postBridge(payload) {
    try {
      await fetch(BRIDGE_URL, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(payload),
      });
    } catch (e) {
      // The bridge listener may have been torn down; nothing else we can do.
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
    const resp = await fetch(API + path, {
      method: "GET",
      credentials: "include",
      headers: headers,
    });
    // Benchling signals an idle/expired session with a 401 and a JSON body
    // containing sessionIdle:true. Treat either as "needs login".
    if (resp.status === 401) {
      let body = null;
      try { body = await resp.json(); } catch (e) {}
      const idle = body && (body.sessionIdle === true ||
        (typeof body.message === "string" && body.message.indexOf("not logged in") !== -1));
      const err = new Error("needs_login");
      err.needsLogin = true;
      err.sessionIdle = !!idle;
      throw err;
    }
    if (!resp.ok) {
      const err = new Error("benchling api error " + resp.status + " for " + path);
      err.status = resp.status;
      throw err;
    }
    return resp.json();
  }

  // ---- mappers --------------------------------------------------------------
  function abs(url) {
    if (!url) return null;
    if (/^https?:\/\//.test(url)) return url;
    return "https://benchling.com" + (url.startsWith("/") ? "" : "/") + url;
  }

  // Benchling entry "notes" are stored as a structured day/note JSON tree. This
  // best-effort flattener turns the common node shapes into readable plaintext so
  // the backend receives something useful even before we lock the exact schema.
  function entryToText(entry) {
    const out = [];
    function walk(node) {
      if (node == null) return;
      if (typeof node === "string") { out.push(node); return; }
      if (Array.isArray(node)) { node.forEach(walk); return; }
      if (typeof node !== "object") { out.push(String(node)); return; }
      // Common text-bearing fields across Benchling note node shapes.
      if (typeof node.text === "string") out.push(node.text);
      if (typeof node.plainText === "string") out.push(node.plainText);
      if (typeof node.name === "string" && node.name) out.push("## " + node.name);
      // Recurse into the usual container fields.
      ["days", "notes", "noteParts", "children", "content", "rows", "cells"].forEach(function (k) {
        if (node[k] != null) walk(node[k]);
      });
    }
    walk(entry.days != null ? entry.days : entry);
    const joined = out.join("\n").trim();
    return joined || (entry.name || "");
  }

  function kindForId(id) {
    if (!id) return "file";
    if (id.indexOf("etr_") === 0) return "entry";
    if (id.indexOf("seq_") === 0 || id.indexOf("dseq_") === 0) return "dna_sequence";
    if (id.indexOf("aseq_") === 0 || id.indexOf("prot_") === 0) return "aa_sequence";
    if (id.indexOf("bfi_") === 0) return "custom_entity";
    return "file";
  }

  // Per-item content fetch (confirmed path: GET /1/api/entries/<id>?view=true).
  async function fetchEntryDetail(id) {
    try {
      return await apiGet("/entries/" + encodeURIComponent(id) + "?view=true");
    } catch (e) {
      if (e && e.needsLogin) throw e;
      return null;
    }
  }

  // Build an item from a get-nested-files files[] element (+ optional detail).
  function mapFile(f, detail, folder) {
    const id = f.id || f.api_identifier || "";
    const kind = kindForId(id);
    let content;
    if (kind === "entry") {
      content = entryToText(detail || f);
    } else if (detail) {
      const bases = detail.bases || detail.sequence || detail.aminoAcids || "";
      content = bases ? ((detail.name || f.name || "") + "\n" + bases) : (detail.name || f.name || "");
    } else {
      content = f.name || "";
    }
    return {
      external_id: id,
      kind: kind,
      title: f.name || (detail && detail.name) || "(untitled)",
      url: abs(f.path || f.webURL || f.url || (id ? "/" + id : null)),
      content: content,
      checksum: String((detail && (detail.modifiedAt || detail.updatedAt)) || f.modifiedAt || f.updatedAt || id || ""),
      metadata: { folder_id: folder.external_id, folder_name: folder.title, path: f.path || null },
    };
  }

  function mapFolder(f) {
    // /1/api/folders items expose api_identifier (lib_...) and source.id (src_...).
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

  // =========================================================================
  // FOLDER CONTENTS — confirmed against a live session 2026-06-18.
  //
  // CONFIRMED endpoints (these are the real ones the SPA uses):
  //   GET /1/api/folders                                       -> array of folders
  //   GET /1/api/folders/actions/get-nested-files?folderId=<lib_..>
  //        -> { files: [ {id, name, path, ...} ] }   (also accepts folderIds)
  //   GET /1/api/entries/<id>?view=true                        -> one entry's note tree
  //
  // NOTE: /1/api/entries?folderId= does NOT exist (404). Listing a folder's
  // contents is ONLY via get-nested-files; per-item content is a follow-up
  // GET /1/api/entries/<id>. We route files by Benchling id prefix (etr_/seq_/...).
  //
  // STILL UNCONFIRMED (the test account had zero content — every folder returned
  // files:[]). Edit ONLY the marked spots when a POPULATED account is available:
  //   1. The full set of fields on a files[] item (confirmed so far: id,name,path).
  //   2. The note-content JSON shape from /1/api/entries/<id>?view=true, which
  //      entryToText() flattens best-effort.
  //   3. Pagination token key on get-nested-files (handled if `nextToken` present).
  // =========================================================================
  function pickArray(resp, keys) {
    if (Array.isArray(resp)) return resp;
    if (resp && typeof resp === "object") {
      for (const k of keys) {
        if (Array.isArray(resp[k])) return resp[k];
      }
      // last resort: first array-valued property
      for (const k of Object.keys(resp)) {
        if (Array.isArray(resp[k])) return resp[k];
      }
    }
    return [];
  }

  async function fetchFolderItems(folder) {
    const items = [];
    const fid = encodeURIComponent(folder.external_id);
    let nextToken = null;
    let guard = 0;

    do {
      guard += 1;
      let resp;
      const path = "/folders/actions/get-nested-files?folderId=" + fid +
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
        if (!id) continue;
        // Notebook entries need a follow-up GET for their note content;
        // other file types use the listing metadata as-is.
        const detail = kindForId(id) === "entry" ? await fetchEntryDetail(id) : null;
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
    // Each folder is itself an importable item.
    folders.forEach(function (f) { items.push(f); });

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
        // Non-fatal: skip this folder's items.
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
