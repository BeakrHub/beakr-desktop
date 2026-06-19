//! Live Benchling agent tools.
//!
//! These tools let the Beakr assistant read a user's Benchling data on demand by
//! RPC over the desktop WebSocket. The backend sends a tool request (e.g.
//! `benchling_list_projects`) and this module fulfils it by calling Benchling's
//! INTERNAL REST API at `https://benchling.com/1/api/*` using the user's own
//! logged-in session.
//!
//! ## Why the internal `/1/api` (and not the official `/api/v2`)
//! Benchling's official `/api/v2` is gated to paid tiers, so a free user cannot
//! use it. The web app itself talks to an internal `/1/api/*` that any logged-in
//! user can reach with their session cookie. That is exactly what these tools use.
//! The endpoint contract was confirmed against a live session on 2026-06-18 and is
//! documented alongside the gather script in `session::scripts::benchling`.
//!
//! ## Session-capture approach: COOKIE (not in-webview eval)
//! When the user Connects Benchling and logs in, `session::commands` captures the
//! HttpOnly `session` cookie from the benchling.com webview's cookie store into
//! [`crate::state::BenchlingSession`]. These tools then call `/1/api/*` directly
//! from Rust with `http_client()` and a `Cookie: session=<value>` header.
//!
//! We chose the cookie approach over evaluating `fetch` inside the open webview
//! because:
//!   1. The agent must be able to call these tools AT ANY TIME — including long
//!      after the user closed the connect window. A pure-Rust HTTP path has no
//!      dependency on a live webview; an eval path would require the window to
//!      stay open and a per-call correlation bridge.
//!   2. macOS WKWebView's cookie store DOES return HttpOnly cookies via
//!      `WKHTTPCookieStore.getAllCookies`, so the `session` cookie is reliably
//!      capturable (verified against wry 0.54's `cookies()` implementation).
//!   3. Benchling's internal `/1/api` GET reads are authenticated by the session
//!      cookie alone. CSRF (`X-CSRFToken`, sourced from a `meta` tag in the page
//!      DOM) guards MUTATING requests; these tools are read-only GETs, so no CSRF
//!      token is required. If a future Benchling change starts requiring CSRF on
//!      reads, the fallback is to additionally capture the token and send it here
//!      — the in-webview eval path documented above remains available as a last
//!      resort.
//!
//! On a 401 / `sessionIdle` response the session is cleared and the tool returns a
//! clear "reconnect" error so the agent can tell the user to reconnect.

use serde_json::{json, Value};

use crate::state::{AppState, BenchlingSession};

/// Builds the internal-API base for a session's tenant (e.g.
/// `https://benchling.com/1/api`). The tenant host is captured at connect time so
/// a future non-default tenant works without code changes.
fn api_base(sess: &BenchlingSession) -> String {
    format!("https://{}/1/api", sess.tenant_host)
}

/// User-facing error returned when the Benchling session is missing or expired.
/// The agent surfaces this verbatim to tell the user how to recover.
const RECONNECT_ERROR: &str = "Benchling session expired - reconnect in the Beakr desktop app";

/// Item-kind classification by Benchling id prefix (confirmed 2026-06-18).
/// `etr_`=entry, `prt_`=protocol, `seq_`=DNA/AA sequence, `file_`=uploaded file;
/// any other prefix (e.g. `bfi_`, custom-entity ids) is treated as a custom entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Entry,
    Sequence,
    CustomEntity,
}

fn classify_id(id: &str) -> ItemKind {
    if id.starts_with("etr_") {
        ItemKind::Entry
    } else if id.starts_with("seq_") {
        ItemKind::Sequence
    } else {
        // Everything that is not an entry, protocol, or sequence/file is treated
        // as a custom entity for the purposes of the custom-entity tools. Files
        // (`file_`) and protocols (`prt_`) are intentionally not surfaced by the
        // custom-entity tools, so they are excluded explicitly below.
        ItemKind::CustomEntity
    }
}

/// Dispatch a `benchling_*` tool to its handler. Returns `Ok((data, None))` (these
/// tools do not report file-style byte counts) or `Err(message)`.
pub async fn dispatch(
    tool: &str,
    params: Value,
    state: &AppState,
) -> Result<(Value, Option<u64>), String> {
    let data = match tool {
        "benchling_list_projects" => list_projects(state).await?,
        "benchling_get_project" => get_project(state, &params).await?,
        "benchling_list_entries" => list_entries(state, &params).await?,
        "benchling_get_entry" => get_entry(state, &params).await?,
        "benchling_list_dna_sequences" => list_dna_sequences(state, &params).await?,
        "benchling_get_dna_sequence" => get_dna_sequence(state, &params).await?,
        "benchling_list_custom_entities" => list_custom_entities(state, &params).await?,
        "benchling_get_custom_entity" => get_custom_entity(state, &params).await?,
        "benchling_search_projects" => search_projects(state, &params).await?,
        "benchling_search_entries" => search_entries(state, &params).await?,
        "benchling_search_dna_sequences" => search_dna_sequences(state, &params).await?,
        "benchling_search_custom_entities" => search_custom_entities(state, &params).await?,
        other => return Err(format!("Unknown Benchling tool: {other}")),
    };
    Ok((data, None))
}

/// True for any tool string this module handles, so the central dispatcher can
/// route to [`dispatch`] without duplicating the list.
pub fn handles(tool: &str) -> bool {
    tool.starts_with("benchling_")
}

// ---- HTTP helpers ----------------------------------------------------------

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(format!("BeakrDesktop/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("failed to build HTTP client")
}

/// Reads the captured session or returns the reconnect error.
async fn session(state: &AppState) -> Result<BenchlingSession, String> {
    state
        .benchling_session
        .read()
        .await
        .clone()
        .ok_or_else(|| RECONNECT_ERROR.to_string())
}

/// GETs `<API_BASE><path>` with the captured session cookie and parses JSON.
///
/// `path` must start with `/`. On 401 (idle/expired session) the captured session
/// is cleared and [`RECONNECT_ERROR`] is returned so the agent prompts a reconnect.
async fn api_get(state: &AppState, sess: &BenchlingSession, path: &str) -> Result<Value, String> {
    let url = format!("{}{path}", api_base(sess));
    let resp = http_client()
        .get(&url)
        .header("Cookie", format!("session={}", sess.session_cookie))
        .header("X-Requested-With", "XMLHttpRequest")
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("Benchling request failed: {e}"))?;

    let status = resp.status();
    if status.as_u16() == 401 {
        // Idle / expired session — drop it so subsequent calls fail fast and the
        // UI can reflect a disconnected state, then tell the user to reconnect.
        *state.benchling_session.write().await = None;
        return Err(RECONNECT_ERROR.to_string());
    }
    if !status.is_success() {
        return Err(format!("Benchling API error {status} for {path}"));
    }

    let text = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read Benchling response: {e}"))?;
    let mut json: Value =
        serde_json::from_str(&text).map_err(|e| format!("Invalid Benchling JSON for {path}: {e}"))?;
    // Benchling's /1/api returns RELATIVE URL paths (e.g. "/mstrome/f_/...").
    // Rewrite them to absolute https://<tenant_host>/... so links resolve to
    // Benchling, not the Beakr app origin that renders the tool result (which
    // would be localhost in dev and the Beakr domain in prod).
    absolutize_urls(&mut json, &sess.tenant_host);
    Ok(json)
}

/// Recursively rewrites Benchling's relative URL fields (`url`, `editURL`,
/// `webURL`, `owner_url`) to absolute `https://<host>/...`, stripping a trailing
/// `/edit` so links open the view (not the editor). Absolute values (e.g.
/// `avatar_url` on a CDN) are left untouched since they do not start with `/`.
fn absolutize_urls(value: &mut Value, host: &str) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if matches!(k.as_str(), "url" | "editURL" | "webURL" | "owner_url") {
                    if let Value::String(s) = v {
                        if s.starts_with('/') {
                            let path = s.strip_suffix("/edit").unwrap_or(s).to_string();
                            *s = format!("https://{host}{path}");
                        }
                        continue;
                    }
                }
                absolutize_urls(v, host);
            }
        }
        Value::Array(arr) => arr.iter_mut().for_each(|i| absolutize_urls(i, host)),
        _ => {}
    }
}

// ---- shared listing helpers ------------------------------------------------

/// Extracts the first array found under any of `keys`, else an empty list. This
/// tolerates Benchling's inconsistent envelopes (`{folders:[...]}`, `{files:[...]}`,
/// bare arrays, etc.).
fn pick_array(resp: &Value, keys: &[&str]) -> Vec<Value> {
    if let Some(arr) = resp.as_array() {
        return arr.clone();
    }
    if let Some(obj) = resp.as_object() {
        for k in keys {
            if let Some(arr) = obj.get(*k).and_then(|v| v.as_array()) {
                return arr.clone();
            }
        }
        for (_k, v) in obj {
            if let Some(arr) = v.as_array() {
                return arr.clone();
            }
        }
    }
    Vec::new()
}

/// A project (Benchling "folder") id from a `/folders` list item. Benchling uses
/// `api_identifier` (e.g. `lib_…`) as the stable id, falling back to `id`.
fn folder_id(folder: &Value) -> Option<String> {
    folder
        .get("api_identifier")
        .and_then(|v| v.as_str())
        .or_else(|| folder.get("id").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

fn item_name(item: &Value) -> String {
    item.get("name")
        .and_then(|v| v.as_str())
        .or_else(|| item.get("displayName").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string()
}

/// Lists all projects (folders) for the session.
async fn fetch_projects(state: &AppState, sess: &BenchlingSession) -> Result<Vec<Value>, String> {
    let resp = api_get(state, sess, "/folders").await?;
    Ok(pick_array(&resp, &["folders", "results", "data", "items"]))
}

/// Lists nested files for a single project via `get-nested-files?ids[]=<id>`,
/// following `nextToken` pagination. Returns the raw `{id, name}` file stubs.
async fn fetch_nested_files(
    state: &AppState,
    sess: &BenchlingSession,
    project_id: &str,
) -> Result<Vec<Value>, String> {
    let encoded = urlencoding::encode(project_id);
    let mut out = Vec::new();
    let mut next_token: Option<String> = None;
    // get-nested-files paginates; guard against a pathological loop.
    for _ in 0..50 {
        let path = match &next_token {
            Some(t) => format!(
                "/folders/actions/get-nested-files?ids[]={encoded}&nextToken={}",
                urlencoding::encode(t)
            ),
            None => format!("/folders/actions/get-nested-files?ids[]={encoded}"),
        };
        let resp = api_get(state, sess, &path).await?;
        for f in pick_array(&resp, &["files", "items", "results", "data"]) {
            out.push(f);
        }
        next_token = resp
            .get("nextToken")
            .or_else(|| resp.get("next_token"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if next_token.is_none() {
            break;
        }
    }
    Ok(out)
}

/// Collects nested files across one project (if `project_id` given) or all
/// projects, deduped by id, filtered to `kind`. `get-nested-files` returns content
/// recursively so the same item can appear under multiple projects.
async fn collect_items(
    state: &AppState,
    sess: &BenchlingSession,
    project_id: Option<&str>,
    kind: ItemKind,
) -> Result<Vec<Value>, String> {
    let project_ids: Vec<String> = match project_id {
        Some(p) => vec![p.to_string()],
        None => fetch_projects(state, sess)
            .await?
            .iter()
            .filter_map(folder_id)
            .collect(),
    };

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for pid in project_ids {
        let files = fetch_nested_files(state, sess, &pid).await?;
        for f in files {
            let id = f
                .get("id")
                .and_then(|v| v.as_str())
                .or_else(|| f.get("api_identifier").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();
            if id.is_empty() || seen.contains(&id) {
                continue;
            }
            // Files and protocols are never surfaced as custom entities.
            if kind == ItemKind::CustomEntity
                && (id.starts_with("file_") || id.starts_with("prt_"))
            {
                continue;
            }
            if classify_id(&id) != kind {
                continue;
            }
            seen.insert(id.clone());
            out.push(json!({
                "id": id,
                "name": item_name(&f),
                "project_id": pid,
            }));
        }
    }
    Ok(out)
}

// ---- tool handlers ---------------------------------------------------------

async fn list_projects(state: &AppState) -> Result<Value, String> {
    let sess = session(state).await?;
    let projects = fetch_projects(state, &sess).await?;
    Ok(json!({ "projects": projects }))
}

async fn get_project(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let id = require_str(params, "project_id")?;
    api_get(state, &sess, &format!("/folders/{}", urlencoding::encode(&id))).await
}

async fn list_entries(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let project_id = optional_str(params, "project_id");
    let entries = collect_items(state, &sess, project_id.as_deref(), ItemKind::Entry).await?;
    Ok(json!({ "entries": entries }))
}

async fn get_entry(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let id = require_str(params, "entry_id")?;
    api_get(
        state,
        &sess,
        &format!("/entries/{}?view=true", urlencoding::encode(&id)),
    )
    .await
}

async fn list_dna_sequences(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let project_id = optional_str(params, "project_id");
    let sequences = collect_items(state, &sess, project_id.as_deref(), ItemKind::Sequence).await?;
    Ok(json!({ "dna_sequences": sequences }))
}

async fn get_dna_sequence(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let id = require_str(params, "sequence_id")?;
    api_get(
        state,
        &sess,
        &format!("/sequences/{}", urlencoding::encode(&id)),
    )
    .await
}

async fn list_custom_entities(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let project_id = optional_str(params, "project_id");
    let entities =
        collect_items(state, &sess, project_id.as_deref(), ItemKind::CustomEntity).await?;
    Ok(json!({ "custom_entities": entities }))
}

async fn get_custom_entity(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let id = require_str(params, "entity_id")?;
    api_get(
        state,
        &sess,
        &format!("/custom-entities/{}", urlencoding::encode(&id)),
    )
    .await
}

async fn search_projects(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let query = require_str(params, "query")?;
    let projects = fetch_projects(state, &sess).await?;
    let matches: Vec<Value> = projects
        .into_iter()
        .filter(|p| name_contains(&item_name(p), &query))
        .collect();
    Ok(json!({ "projects": matches }))
}

async fn search_entries(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let query = require_str(params, "query")?;
    let entries = collect_items(state, &sess, None, ItemKind::Entry).await?;
    Ok(json!({ "entries": filter_by_name(entries, &query) }))
}

async fn search_dna_sequences(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let query = require_str(params, "query")?;
    let sequences = collect_items(state, &sess, None, ItemKind::Sequence).await?;
    Ok(json!({ "dna_sequences": filter_by_name(sequences, &query) }))
}

async fn search_custom_entities(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let query = require_str(params, "query")?;
    let entities = collect_items(state, &sess, None, ItemKind::CustomEntity).await?;
    Ok(json!({ "custom_entities": filter_by_name(entities, &query) }))
}

// ---- param + filter utilities ----------------------------------------------

fn require_str(params: &Value, key: &str) -> Result<String, String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("Missing required '{key}' parameter"))
}

fn optional_str(params: &Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn name_contains(name: &str, query: &str) -> bool {
    name.to_lowercase().contains(&query.to_lowercase())
}

fn filter_by_name(items: Vec<Value>, query: &str) -> Vec<Value> {
    items
        .into_iter()
        .filter(|it| name_contains(&item_name(it), query))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_id_maps_prefixes() {
        assert_eq!(classify_id("etr_abc"), ItemKind::Entry);
        assert_eq!(classify_id("seq_abc"), ItemKind::Sequence);
        // A custom-entity-style id (no entry/sequence prefix) is a custom entity.
        assert_eq!(classify_id("bfi_abc"), ItemKind::CustomEntity);
    }

    #[test]
    fn pick_array_handles_envelopes_and_bare_arrays() {
        // Bare array.
        let bare = json!([{ "id": "etr_1" }]);
        assert_eq!(pick_array(&bare, &["files"]).len(), 1);
        // Keyed envelope.
        let keyed = json!({ "files": [{ "id": "etr_1" }, { "id": "etr_2" }] });
        assert_eq!(pick_array(&keyed, &["files"]).len(), 2);
        // Fallback to the first array-valued field when no known key matches.
        let other = json!({ "weird": [{ "id": "etr_1" }] });
        assert_eq!(pick_array(&other, &["files"]).len(), 1);
        // No array anywhere -> empty.
        let none = json!({ "n": 1 });
        assert!(pick_array(&none, &["files"]).is_empty());
    }

    #[test]
    fn folder_id_prefers_api_identifier() {
        let f = json!({ "api_identifier": "lib_1", "id": "internal_1" });
        assert_eq!(folder_id(&f).as_deref(), Some("lib_1"));
        let f2 = json!({ "id": "internal_2" });
        assert_eq!(folder_id(&f2).as_deref(), Some("internal_2"));
    }

    #[test]
    fn name_filter_is_case_insensitive_contains() {
        assert!(name_contains("My Plasmid Library", "plasmid"));
        assert!(name_contains("PCR-01", "pcr"));
        assert!(!name_contains("Genome", "plasmid"));
    }

    #[test]
    fn handles_matches_benchling_prefix_only() {
        assert!(handles("benchling_list_projects"));
        assert!(handles("benchling_get_entry"));
        assert!(!handles("list_files"));
    }
}
