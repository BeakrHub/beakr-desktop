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

  function mapEntry(e, folder) {
    return {
      external_id: e.id || e.apiId || e.api_identifier || "",
      kind: "entry",
      title: e.name || e.displayName || "(untitled entry)",
      url: abs(e.webURL || e.url || (e.id ? "/" + e.id : null)),
      content: entryToText(e),
      checksum: String(e.modifiedAt || e.updatedAt || e.modified_at || e.id || ""),
      metadata: { folder_id: folder.external_id, folder_name: folder.title },
    };
  }

  function mapSequence(s, kind, folder) {
    const bases = s.bases || s.sequence || s.aminoAcids || "";
    return {
      external_id: s.id || s.apiId || s.api_identifier || "",
      kind: kind, // dna_sequence | aa_sequence | custom_entity
      title: s.name || s.displayName || "(untitled)",
      url: abs(s.webURL || s.url || (s.id ? "/" + s.id : null)),
      content: bases ? (s.name ? s.name + "\n" + bases : bases) : (s.name || ""),
      checksum: String(s.modifiedAt || s.updatedAt || s.modified_at || s.id || ""),
      metadata: { folder_id: folder.external_id, folder_name: folder.title, length: s.length || (bases ? bases.length : null) },
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
  // ENTRY / SEQUENCE FETCHING — THE ONE UNCERTAIN FUNCTION.
  //
  // Confirmed from a stale session (401, not 404) that these endpoints EXIST:
  //   GET /1/api/entries?folderId=<lib_...>
  //   GET /1/api/dna-sequences?folderId=<lib_...>
  //   GET /1/api/aa-sequences            (folder filter param unconfirmed)
  //   GET /1/api/custom-entities         (folder filter param unconfirmed)
  //
  // UNKNOWNS to confirm against a FRESH + POPULATED account, then edit here only:
  //   1. Exact filter param name (folderId vs folder_id vs projectId).
  //   2. Whether aa-sequences / custom-entities accept the same folder filter.
  //   3. The response envelope key. We try, in order: entries / dnaSequences /
  //      aaSequences / customEntities / results / data / items, else assume the
  //      response itself is the array.
  //   4. Pagination (nextToken / pageToken / offset) — currently single-page.
  //   5. Whether list responses include note content or only a summary (if only
  //      a summary, a per-id GET /1/api/entries/<id> follow-up will be needed).
  //
  // We deliberately swallow per-endpoint errors (except needs_login) so one
  // unconfirmed shape never aborts the whole import.
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

    // Each endpoint is attempted independently; a failure on one (e.g. an
    // unconfirmed param) must not kill the others.
    const attempts = [
      { path: API + "/entries?folderId=" + fid, keys: ["entries", "results", "data", "items"], kind: "entry" },
      { path: API + "/dna-sequences?folderId=" + fid, keys: ["dnaSequences", "results", "data", "items"], kind: "dna_sequence" },
      { path: API + "/aa-sequences?folderId=" + fid, keys: ["aaSequences", "results", "data", "items"], kind: "aa_sequence" },
      { path: API + "/custom-entities?folderId=" + fid, keys: ["customEntities", "results", "data", "items"], kind: "custom_entity" },
    ];

    for (const a of attempts) {
      let resp;
      try {
        const headers = { "X-Requested-With": "XMLHttpRequest", "Accept": "application/json" };
        const token = csrfToken();
        if (token) headers["X-CSRFToken"] = token;
        const r = await fetch(a.path, { method: "GET", credentials: "include", headers: headers });
        if (r.status === 401) {
          const err = new Error("needs_login");
          err.needsLogin = true;
          throw err;
        }
        if (!r.ok) continue; // unconfirmed/unsupported endpoint for this account — skip
        resp = await r.json();
      } catch (e) {
        if (e && e.needsLogin) throw e;
        continue;
      }
      const arr = pickArray(resp, a.keys);
      for (const raw of arr) {
        items.push(a.kind === "entry" ? mapEntry(raw, folder) : mapSequence(raw, a.kind, folder));
      }
    }
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
