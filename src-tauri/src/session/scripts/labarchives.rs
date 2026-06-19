//! The JavaScript "gather script" injected into the LabArchives webview.
//!
//! STATUS: SKELETON. This proves the provider-registry/adapter shape. It follows
//! the SAME data contract as the Benchling script (`session::scripts::benchling`)
//! but the LabArchives-specific endpoint/DOM contract has NOT yet been
//! reverse-engineered. As shipped it verifies the session and then emits
//! `needs_login` (if not logged in) or a clearly-labeled `error` placeholder so a
//! developer can confirm the registry wiring end-to-end before filling in the real
//! gather logic.
//!
//! ===========================================================================
//! TODO: REVERSE-ENGINEER THE LABARCHIVES CONTRACT (mirror of how Benchling's was
//!       discovered against a live, logged-in session). Until each item below is
//!       answered and encoded, this script intentionally does NOT return items.
//! ===========================================================================
//!
//!   1. LOGIN / SESSION MECHANISM
//!      - What origin(s) does the user land on after login? LabArchives ELN is at
//!        https://mynotebook.labarchives.com (US). There are regional hosts (EU
//!        `eln.labarchives.com`, AU `au-mynotebook.labarchives.com`, UK, etc.) —
//!        decide whether to register them as separate providers or detect host at
//!        runtime and set `tenant_host` accordingly.
//!      - Is the session a cookie (sent automatically with `credentials:"include"`)
//!        or a token in localStorage / a per-tab `uid`/`akid` query param? Confirm
//!        by inspecting authenticated XHRs in DevTools on a live session.
//!
//!   2. INTERNAL API vs DOM
//!      - LabArchives' public API (`/api/...`) requires an institutional API key +
//!        HMAC signature (akid/expires/sig) that a free/individual user does NOT
//!        have, so it is NOT usable here (this is the analogue of Benchling's
//!        unavailable `/api/v2`). Determine the INTERNAL endpoints the SPA itself
//!        calls (watch the Network tab): likely under a path like `/api/...` or an
//!        RPC endpoint returning the notebook tree and page/entry content.
//!      - If no clean internal JSON API exists, fall back to DOM scraping of the
//!        rendered notebook/page (last resort; brittle — document the selectors).
//!
//!   3. AUTH / CSRF HEADERS
//!      - Identify any required headers: CSRF token (meta tag? cookie-mirrored
//!        header like `X-CSRF-Token`?), `X-Requested-With: XMLHttpRequest`, and the
//!        per-session `uid`/`akid` values. Benchling needed `X-CSRFToken` +
//!        `X-Requested-With`; LabArchives' set will differ — capture it exactly.
//!
//!   4. LIST CALL (notebook / folder / page tree)
//!      - Find the endpoint that returns the user's notebooks and their tree of
//!        folders/pages (the analogue of Benchling `/folders` +
//!        `/folders/actions/get-nested-files`). Note the id field names and the
//!        nesting shape. Map each notebook/folder to a `folder` item.
//!
//!   5. CONTENT CALL (per page / per entry)
//!      - Find the endpoint that returns a single page's entries/content (the
//!        analogue of Benchling's `/entries/<id>?view=true`). LabArchives pages are
//!        composed of typed "entries" (rich text, attachments, plain text, widgets,
//!        sketches, etc.). Map id prefixes / entry types to a `kind`.
//!
//!   6. PAGINATION
//!      - Determine how large notebooks paginate (page tokens? offset/limit?
//!        per-notebook batches?) and add a bounded loop with a guard, exactly like
//!        the Benchling `nextToken` loop (guarded at 50 iterations).
//!
//!   7. CONTENT -> MARKDOWN FLATTENING
//!      - Write per-entry-type formatters (analogous to Benchling's
//!        `entryToText`/`tableToText`/`sequenceToText`) that flatten LabArchives
//!        entry payloads into the `content` markdown string. Cap any unbounded
//!        fields (attachments, large blobs) the way Benchling caps `MAX_BASES`.
//!
//!   8. SESSION-EXPIRY SIGNAL
//!      - Determine how LabArchives signals an idle/expired session (HTTP 401/403?
//!        a redirect to a login HTML page returned with 200? a JSON error flag?).
//!        Surface it as `needs_login` (NOT `error`) so the user can re-auth and
//!        retry, mirroring Benchling's 401 + `sessionIdle` handling.
//!
//! DATA CONTRACT (must match Benchling exactly):
//!   - Bridge: POST JSON to `http://127.0.0.1:<port>/session/ingest`.
//!   - Messages: `{type:"progress"|"needs_login"|"complete"|"error", ...}`.
//!   - On complete: `{ user_handle, tenant_host, items: [...] }`.
//!   - Each item: `{ external_id, kind, title, url, content, checksum, metadata }`.

/// The raw gather script. Rust substitutes `PORT_PLACEHOLDER` before `webview.eval`.
///
/// Skeleton: verifies presence of a session, then emits `needs_login` or a labeled
/// placeholder `error`. Replace the body of `gatherItems()` with the real list +
/// content + flatten logic once the contract above is reverse-engineered.
pub const LABARCHIVES_GATHER_SCRIPT: &str = r#####"
(async function beakrLabArchivesGather() {
  const BRIDGE_PORT = "__BEAKR_BRIDGE_PORT__";
  const BRIDGE_URL = "http://127.0.0.1:" + BRIDGE_PORT + "/session/ingest";

  // ---- bridge helpers (identical contract to the Benchling connector) -------
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

  // ---- session detection ----------------------------------------------------
  // TODO(labarchives): replace this heuristic with a real authenticated probe
  // (e.g. GET the internal "current user"/"my notebooks" endpoint and treat a
  // 401/403 or a login-page redirect as "not logged in"). For now we infer login
  // from the presence of any LabArchives session cookie.
  function looksLoggedIn() {
    try {
      const cookie = document.cookie || "";
      // Placeholder signal; the real session cookie name must be confirmed.
      return /(?:labarchives|_la_session|uid)=/i.test(cookie);
    } catch (e) {
      return false;
    }
  }

  // ---- item gathering -------------------------------------------------------
  // TODO(labarchives): implement using the discovered internal API:
  //   1. list notebooks/folders/pages, map each to a `folder` item
  //   2. for each page, fetch entries and flatten to markdown `content`
  //   3. dedup by external_id, paginate with a bounded guard loop
  // Each returned item MUST be:
  //   { external_id, kind, title, url, content, checksum, metadata }
  // matching the Benchling contract exactly.
  async function gatherItems() {
    // Not implemented yet — the LabArchives endpoint contract is unknown.
    return null;
  }

  // ---- main flow ------------------------------------------------------------
  try {
    await report("verify", "Verifying LabArchives login");
    if (!looksLoggedIn()) {
      await postBridge({ type: "needs_login", message: "Please log in to LabArchives, then click Import." });
      return;
    }

    await report("gather", "Gathering LabArchives notebooks");
    const items = await gatherItems();
    if (!items) {
      // Skeleton: contract not yet implemented. Surface a clear, non-terminal
      // error rather than reporting a bogus empty success.
      await postBridge({
        type: "error",
        message: "LabArchives import is not implemented yet. The provider is registered as a skeleton; see session/scripts/labarchives.rs for the contract still to be reverse-engineered.",
      });
      return;
    }

    await postBridge({
      type: "complete",
      // TODO(labarchives): derive a real user handle + tenant host from the
      // authenticated session / current host.
      user_handle: "unknown",
      tenant_host: "mynotebook.labarchives.com",
      items: items,
    });
  } catch (e) {
    await postBridge({ type: "error", message: (e && e.message) ? e.message : String(e) });
  }
})();
"#####;
