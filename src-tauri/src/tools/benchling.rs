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
//! The endpoint contract was confirmed against a live session on 2026-06-18.
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
//!      stay open and a per-call correlation channel back to Rust.
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

/// Benchling's internal global-search endpoint. This is a ranked, full-text search
/// over item names AND notebook/protocol BODY content (verified live 2026-07-05:
/// distinctive body-only terms like "phusion"/"supernatant" return the entry whose
/// steps contain them, with the term absent from the title). It replaces the old
/// "list every entry, then GET each one and grep it" fallback that timed out on
/// accounts with many entries — exactly the failure a free user reported.
///
/// Unlike the read GETs, this is a POST and is CSRF-guarded, so it needs the token
/// from the app-shell HTML `<meta name="csrf-token">` (see [`fetch_csrf_token`]).
const SEARCH_PATH: &str = "/search?includeStorableExtras=false&allowAsyncBlast=true";
/// Default number of ranked hits to request. The web app uses 25; that is plenty
/// to surface the right item for a natural-language query without a large payload.
const SEARCH_DEFAULT_LIMIT: u64 = 25;
/// Hard cap on the caller-supplied page size so a runaway `limit` can't ask Benchling
/// for an unbounded result set.
const SEARCH_MAX_LIMIT: u64 = 50;

/// Every TYPES token the web app's global search requests. Used verbatim for the
/// unscoped `benchling_search`; the per-type tools narrow this to their own token(s).
/// Confirmed against the live global-search request body 2026-07-05.
const ALL_SEARCH_TYPES: &[&str] = &[
    "folder",
    "sequence",
    "rna_sequence",
    "protein",
    "basic_folder_item",
    "oligo",
    "rna_oligo",
    "mixture",
    "entry",
    "protocol",
    "request_v2_submission",
    "request_v2_definition",
    "sequence_analysis",
    "protein_alignment",
    "bulk_assembly",
    "worklist",
    "template",
    "subtemplate",
    "form_definition",
    "template_collection",
    "pipeline_file",
];

/// Item-kind classification by Benchling id prefix (confirmed 2026-06-18).
/// `etr_`=entry, `prt_`=protocol, `seq_`=DNA/AA sequence, `file_`=uploaded file;
/// any other prefix (e.g. `bfi_`, custom-entity ids) is treated as a custom entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Entry,
    Sequence,
    Protocol,
    AaSequence,
    File,
    CustomEntity,
}

fn classify_id(id: &str) -> ItemKind {
    if id.starts_with("etr_") {
        ItemKind::Entry
    } else if id.starts_with("seq_") {
        // DNA/RNA sequences and oligos share the seq_ prefix; both surface through
        // the DNA-sequence tools. get-nested-files exposes only id+name, so there
        // is no field to split oligos out without a per-item detail fetch.
        ItemKind::Sequence
    } else if id.starts_with("prtn_") {
        // AA / protein sequences. Confirmed live 2026-06-30: an AA sequence created
        // in Benchling gets a prtn_ id. This branch must precede no others that
        // could swallow it; without it prtn_ falls through to CustomEntity and
        // proteins are silently mislabeled as custom entities.
        ItemKind::AaSequence
    } else if id.starts_with("prt_") {
        ItemKind::Protocol
    } else if id.starts_with("file_") {
        ItemKind::File
    } else {
        // Anything else (e.g. bfi_ registry ids) is a custom entity.
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
        "benchling_search" => search_all(state, &params).await?,
        "benchling_search_projects" => search_projects(state, &params).await?,
        "benchling_search_entries" => search_entries(state, &params).await?,
        "benchling_search_dna_sequences" => search_dna_sequences(state, &params).await?,
        "benchling_search_custom_entities" => search_custom_entities(state, &params).await?,
        "benchling_list_protocols" => list_protocols(state, &params).await?,
        "benchling_get_protocol" => get_protocol(state, &params).await?,
        "benchling_search_protocols" => search_protocols(state, &params).await?,
        "benchling_list_aa_sequences" => list_aa_sequences(state, &params).await?,
        "benchling_get_aa_sequence" => get_aa_sequence(state, &params).await?,
        "benchling_search_aa_sequences" => search_aa_sequences(state, &params).await?,
        "benchling_list_files" => list_files(state, &params).await?,
        "benchling_get_file" => get_file(state, &params).await?,
        "benchling_search_files" => search_files(state, &params).await?,
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
    let mut json: Value = serde_json::from_str(&text)
        .map_err(|e| format!("Invalid Benchling JSON for {path}: {e}"))?;
    // Benchling's /1/api returns RELATIVE URL paths (e.g. "/mstrome/f_/...").
    // Rewrite them to absolute https://<tenant_host>/... so links resolve to
    // Benchling, not the Beakr app origin that renders the tool result (which
    // would be localhost in dev and the Beakr domain in prod).
    absolutize_urls(&mut json, &sess.tenant_host);
    Ok(json)
}

/// Recursively rewrites Benchling's relative URL fields (`url`, `editURL`,
/// `webURL`, `owner_url`) to absolute `https://<host>/...`. The path is kept
/// exactly as Benchling provides it, including a trailing `/edit`: on path-handle
/// (free-tier) tenants the non-`/edit` form 404s, so `/edit` is the URL that
/// actually resolves (verified live against a david-beakr sequence). Absolute
/// values (e.g. `avatar_url` on a CDN) are left untouched since they do not
/// start with `/`.
fn absolutize_urls(value: &mut Value, host: &str) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if matches!(k.as_str(), "url" | "editURL" | "webURL" | "owner_url") {
                    if let Value::String(s) = v {
                        if s.starts_with('/') {
                            *s = format!("https://{host}{s}");
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

// ---- ranked global search (POST /search) -----------------------------------

/// Fetches a CSRF token for the CSRF-guarded POST search.
///
/// Benchling renders the token into `<meta name="csrf-token" content="...">` of the
/// server-rendered app shell. It is NOT in a JS-readable cookie, localStorage, or
/// any `/1/api` JSON body (all verified live 2026-07-05), so the only way to obtain
/// it is to read an authenticated HTML page. The token is bound to the session
/// cookie, so we fetch this session's tenant home and extract it. Read GETs don't
/// need CSRF; only the POST /search path does.
async fn fetch_csrf_token(sess: &BenchlingSession) -> Result<String, String> {
    // The tenant home for a path-handle (free) tenant is /{handle}; any authenticated
    // page carries the meta, so fall back to the tenant root if the handle page fails.
    let candidates = [
        format!("https://{}/{}", sess.tenant_host, sess.user_handle),
        format!("https://{}/", sess.tenant_host),
    ];
    let mut last_err = String::from("no CSRF page candidates");
    for url in candidates {
        match http_client()
            .get(&url)
            .header("Cookie", format!("session={}", sess.session_cookie))
            .header("Accept", "text/html")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let html = resp
                    .text()
                    .await
                    .map_err(|e| format!("Failed to read Benchling page for CSRF: {e}"))?;
                if let Some(token) = extract_csrf_meta(&html) {
                    return Ok(token);
                }
                last_err = "CSRF meta tag not found in Benchling page".to_string();
            }
            Ok(resp) => last_err = format!("CSRF page returned HTTP {}", resp.status()),
            Err(e) => last_err = format!("CSRF page request failed: {e}"),
        }
    }
    Err(last_err)
}

/// Extracts the token from `<meta name="csrf-token" content="...">`. Tolerant of
/// attribute order and single/double quotes; returns None if the tag is absent.
fn extract_csrf_meta(html: &str) -> Option<String> {
    // Anchor on the token's name, then read the nearest following content="…".
    let anchor = html.find("csrf-token")?;
    let after = &html[anchor..];
    let content_at = after.find("content=")?;
    let rest = &after[content_at + "content=".len()..];
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &rest[1..];
    let end = rest.find(quote)?;
    let token = &rest[..end];
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// Builds the header set for a CSRF-guarded Benchling POST.
///
/// Benchling's CSRF guard (Flask-WTF) does a strict referrer check on HTTPS POSTs:
/// it rejects the request outright when the `Referer` header is absent ("The referrer
/// header is missing.") and requires that header to share the request's scheme+host.
/// A browser attaches `Referer` automatically, but reqwest does not, so every
/// `/search` POST failed with a CSRF 400 until we send it explicitly. The value only
/// needs to match the request origin, so the tenant root suffices. Confirmed live
/// 2026-07-07: the identical POST is a CSRF 400 without `Referer` and 200 with it.
fn csrf_post_headers(sess: &BenchlingSession, csrf: &str) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderValue};

    let mut headers = HeaderMap::new();
    let mut set = |name: &'static str, value: String| {
        if let Ok(v) = HeaderValue::from_str(&value) {
            headers.insert(name, v);
        }
    };
    set("Cookie", format!("session={}", sess.session_cookie));
    set("X-CSRFToken", csrf.to_string());
    set("X-Requested-With", "XMLHttpRequest".to_string());
    set("Referer", format!("https://{}/", sess.tenant_host));
    set("Accept", "application/json".to_string());
    set("Content-Type", "application/json".to_string());
    headers
}

/// POSTs `<API_BASE><path>` with the session cookie + CSRF token and parses JSON.
/// Mirrors [`api_get`]'s 401 handling (clears the session and returns the reconnect
/// error) so a dead session self-heals through the same path as reads.
async fn api_post(
    state: &AppState,
    sess: &BenchlingSession,
    path: &str,
    csrf: &str,
    body: Value,
) -> Result<Value, String> {
    let url = format!("{}{path}", api_base(sess));
    let resp = http_client()
        .post(&url)
        .headers(csrf_post_headers(sess, csrf))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Benchling request failed: {e}"))?;

    let status = resp.status();
    if status.as_u16() == 401 {
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
    let mut json: Value = serde_json::from_str(&text)
        .map_err(|e| format!("Invalid Benchling JSON for {path}: {e}"))?;
    absolutize_urls(&mut json, &sess.tenant_host);
    Ok(json)
}

/// Clamps a caller-supplied `limit` param to `[1, SEARCH_MAX_LIMIT]`, defaulting to
/// [`SEARCH_DEFAULT_LIMIT`]. Accepts either a JSON number or numeric string (the
/// backend forwards agent args as strings).
fn search_limit(params: &Value) -> u64 {
    let raw = params
        .get("limit")
        .or_else(|| params.get("page_size"))
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
    raw.unwrap_or(SEARCH_DEFAULT_LIMIT).clamp(1, SEARCH_MAX_LIMIT)
}

/// Runs the ranked global search for `query`, scoped to `types`. Returns the raw
/// Benchling response (ids + `*SummariesById` maps + scores); projection into tidy
/// rows is [`project_search_results`].
async fn ranked_search(
    state: &AppState,
    sess: &BenchlingSession,
    query: &str,
    types: &[&str],
    limit: u64,
) -> Result<Value, String> {
    let csrf = fetch_csrf_token(sess).await?;
    let body = json!({
        "filters": [
            { "key": "TYPES", "operator": "IS_ONE_OF", "value": types },
            { "key": "ARCHIVE_PURPOSES", "operator": "IS_ONE_OF", "value": ["NOT_ARCHIVED"] },
            { "key": "IS_ASSOCIATED_WITH_UNSUBMITTED_REQUEST_V2_SUBMISSION", "operator": "IS_FALSE", "value": null },
            { "key": "PROCESSES_IS_SYSTEM_DATA_FILTER", "operator": "IS_FALSE", "value": null },
        ],
        "offset": 0,
        "limit": limit,
        "query": query,
        "sorts": [{ "key": "relevance", "reverse": false }],
        "nextToken": null,
        "groupBy": null,
        "searchSource": "listing:GLOBAL",
    });
    api_post(state, sess, SEARCH_PATH, &csrf, body).await
}

/// Merges every top-level `*ById` summary map in a search response into one
/// id -> summary lookup. Benchling scatters item detail across many buckets
/// (`entrySummariesById`, `protocolSummariesById`, `foldersById`, `fileSummariesById`,
/// …); the caller just needs "the summary for this id" regardless of bucket.
fn merge_summaries(resp: &Value) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    if let Some(obj) = resp.as_object() {
        for (key, val) in obj {
            if key.ends_with("ById") {
                if let Some(map) = val.as_object() {
                    for (id, summary) in map {
                        out.entry(id.clone()).or_insert_with(|| summary.clone());
                    }
                }
            }
        }
    }
    out
}

/// Short type label for a result id, so the agent can tell entries from protocols
/// etc. in a mixed `benchling_search` result.
fn kind_label(id: &str) -> &'static str {
    match classify_id(id) {
        ItemKind::Entry => "entry",
        ItemKind::Sequence => "dna_sequence",
        ItemKind::Protocol => "protocol",
        ItemKind::AaSequence => "aa_sequence",
        ItemKind::File => "file",
        ItemKind::CustomEntity => "custom_entity",
    }
}

/// Projects a raw search response into ranked result rows, preserving Benchling's
/// relevance order. Optional client-side filters:
///   - `kind`: keep only ids of this item kind (id-prefix). None keeps all types.
///   - `project_id`: keep only items whose containing folder's `api_identifier`
///     matches (Benchling's search has no working server-side folder filter — the
///     documented keys 500 — so scoping is done here from each item's summary).
///   - `modified_after`: keep only items modified on/after this ISO date/timestamp
///     (lexicographic compare is chronological for ISO-8601 UTC).
/// Each row carries id, name, type, project id/name, timestamps, a Benchling link,
/// and the relevance score.
fn project_search_results(
    resp: &Value,
    kind: Option<ItemKind>,
    project_id: Option<&str>,
    modified_after: Option<&str>,
) -> Vec<Value> {
    let summaries = merge_summaries(resp);
    let scores = resp.get("scoresById").and_then(|v| v.as_object());
    let ids = resp.get("ids").and_then(|v| v.as_array()).cloned().unwrap_or_default();

    let mut rows = Vec::new();
    for id_val in ids {
        let Some(id) = id_val.as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        if let Some(k) = kind {
            if classify_id(id) != k {
                continue;
            }
        }
        let summary = summaries.get(id).cloned().unwrap_or_else(|| json!({}));
        let folder = summary.get("folder");
        let project_api = folder
            .and_then(|f| f.get("api_identifier"))
            .and_then(|v| v.as_str());
        if let Some(want) = project_id {
            if project_api != Some(want) {
                continue;
            }
        }
        let modified = summary.get("modified_at").and_then(|v| v.as_str());
        if let Some(after) = modified_after {
            match modified {
                Some(m) if m >= after => {}
                // Drop items with no timestamp or an older one when a floor is set.
                _ => continue,
            }
        }
        rows.push(json!({
            "id": id,
            "type": kind_label(id),
            "name": summary.get("name").and_then(|v| v.as_str()).unwrap_or(""),
            "project_id": project_api,
            "project_name": folder.and_then(|f| f.get("name")).and_then(|v| v.as_str()),
            "modified_at": modified,
            "created_at": summary.get("created_at"),
            "url": summary.get("editURL").or_else(|| summary.get("url")),
            "score": scores.and_then(|s| s.get(id)),
            "match_source": "ranked_content",
        }));
    }
    rows
}

/// Assembles the standard search tool envelope: the projected rows plus the ranking
/// metadata the backend/agent use to decide whether to page or narrow.
fn search_envelope(items_key: &str, rows: Vec<Value>, resp: &Value) -> Value {
    json!({
        items_key: rows,
        "search_strategy": "ranked_content_search",
        "total_ranked": resp.get("total"),
        "is_exact": resp.get("isExact"),
        "timed_out": resp.get("timedOut"),
    })
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

/// Surface a project's stable `lib_…` identifier as its `id` so the agent can
/// round-trip list -> get. The raw `/folders` object carries a numeric `id` plus
/// the stable `api_identifier`; `get_project` resolves `/folders/{id}`, which only
/// accepts the `api_identifier`. Without this the agent passes the numeric id and
/// gets a 404 ("project does not exist"). The numeric id is preserved as `db_id`.
fn normalize_project(mut folder: Value) -> Value {
    if let Some(fid) = folder_id(&folder) {
        if let Value::Object(map) = &mut folder {
            if let Some(prev) = map.insert("id".to_string(), Value::String(fid)) {
                map.entry("db_id".to_string()).or_insert(prev);
            }
        }
    }
    folder
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
            // Every non-custom-entity kind (entry, sequence, protocol, AA sequence,
            // file) now has its own ItemKind, so the prefix check below is the only
            // filter needed: anything that is not the requested kind is skipped, and
            // only unprefixed/registry ids fall through to CustomEntity.
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
    let projects: Vec<Value> = fetch_projects(state, &sess)
        .await?
        .into_iter()
        .map(normalize_project)
        .collect();
    Ok(json!({ "projects": projects }))
}

async fn get_project(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let id = require_str(params, "project_id")?;
    api_get(
        state,
        &sess,
        &format!("/folders/{}", urlencoding::encode(&id)),
    )
    .await
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
        .map(normalize_project)
        .collect();
    Ok(json!({ "projects": matches }))
}

/// Ranked, full-text entry search over names AND notebook body content.
///
/// Replaces the old "list every entry then GET each and grep it" scan, which timed
/// out on accounts with many entries (a free user hit exactly this). One ranked
/// `/search` call returns the right entries with names, so the agent no longer
/// enumerates. `project_id`/`modified_after` narrow results client-side.
async fn search_entries(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let query = require_str(params, "query")?;
    let project_id = optional_str(params, "project_id");
    let modified_after = optional_str(params, "modified_after");
    let resp = ranked_search(state, &sess, &query, &["entry"], search_limit(params)).await?;
    let rows = project_search_results(
        &resp,
        Some(ItemKind::Entry),
        project_id.as_deref(),
        modified_after.as_deref(),
    );
    Ok(search_envelope("entries", rows, &resp))
}

/// Ranked global search across all Benchling item types (entries, protocols,
/// sequences, proteins, files, folders). Use when the user asks "where did I write
/// about X" without knowing the object type. Rows carry a `type` so the agent can
/// route a follow-up get_* to the right detail tool.
async fn search_all(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let query = require_str(params, "query")?;
    let project_id = optional_str(params, "project_id");
    let modified_after = optional_str(params, "modified_after");
    let resp = ranked_search(state, &sess, &query, ALL_SEARCH_TYPES, search_limit(params)).await?;
    let rows =
        project_search_results(&resp, None, project_id.as_deref(), modified_after.as_deref());
    Ok(search_envelope("results", rows, &resp))
}

async fn search_dna_sequences(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let query = require_str(params, "query")?;
    let project_id = optional_str(params, "project_id");
    // DNA/RNA sequences and oligos all surface under the seq_ prefix; scope the
    // ranked search to the sequence type family. Oligo ids don't share the seq_
    // prefix, so rely on the server TYPES scope rather than a client kind filter.
    let resp = ranked_search(
        state,
        &sess,
        &query,
        &["sequence", "rna_sequence", "oligo", "rna_oligo"],
        search_limit(params),
    )
    .await?;
    let rows = project_search_results(&resp, None, project_id.as_deref(), None);
    Ok(search_envelope("dna_sequences", rows, &resp))
}

async fn search_custom_entities(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let query = require_str(params, "query")?;
    let project_id = optional_str(params, "project_id");
    // Custom-entity ids have no single stable TYPES token, so search broadly and
    // keep only ids that classify as custom entities (the else-branch prefix).
    let resp = ranked_search(state, &sess, &query, ALL_SEARCH_TYPES, search_limit(params)).await?;
    let rows =
        project_search_results(&resp, Some(ItemKind::CustomEntity), project_id.as_deref(), None);
    Ok(search_envelope("custom_entities", rows, &resp))
}

// ---- protocol handlers ------------------------------------------------------
//
// Protocols are returned by `get-nested-files` like any other project content
// (id prefix `prt_`), so list/search reuse the same `collect_items` path as
// entries and sequences. This closes the gap behind "Beakr finds my project but
// not the protocol inside it" — e.g. a recipe stored as a Benchling Protocol.

async fn list_protocols(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let project_id = optional_str(params, "project_id");
    let protocols = collect_items(state, &sess, project_id.as_deref(), ItemKind::Protocol).await?;
    Ok(json!({ "protocols": protocols }))
}

async fn get_protocol(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let id = require_str(params, "protocol_id")?;
    api_get(
        state,
        &sess,
        &format!("/protocols/{}", urlencoding::encode(&id)),
    )
    .await
}

async fn search_protocols(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let query = require_str(params, "query")?;
    let project_id = optional_str(params, "project_id");
    let resp = ranked_search(state, &sess, &query, &["protocol"], search_limit(params)).await?;
    let rows =
        project_search_results(&resp, Some(ItemKind::Protocol), project_id.as_deref(), None);
    Ok(search_envelope("protocols", rows, &resp))
}

// ---- AA sequence handlers ---------------------------------------------------
//
// AA / protein sequences carry the prtn_ id prefix (confirmed live 2026-06-30).
// They come back from get-nested-files alongside other content; before this they
// fell through classify_id to CustomEntity and were mislabeled as custom entities.

async fn list_aa_sequences(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let project_id = optional_str(params, "project_id");
    let sequences =
        collect_items(state, &sess, project_id.as_deref(), ItemKind::AaSequence).await?;
    Ok(json!({ "aa_sequences": sequences }))
}

async fn get_aa_sequence(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let id = require_str(params, "aa_sequence_id")?;
    api_get(
        state,
        &sess,
        &format!("/aa-sequences/{}", urlencoding::encode(&id)),
    )
    .await
}

async fn search_aa_sequences(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let query = require_str(params, "query")?;
    let project_id = optional_str(params, "project_id");
    let resp = ranked_search(state, &sess, &query, &["protein"], search_limit(params)).await?;
    let rows =
        project_search_results(&resp, Some(ItemKind::AaSequence), project_id.as_deref(), None);
    Ok(search_envelope("aa_sequences", rows, &resp))
}

// ---- file handlers ----------------------------------------------------------
//
// Uploaded files carry the file_ id prefix and are returned by get-nested-files.
// They were previously dropped (never surfaced by any tool). The get-file detail
// endpoint (`/files/{id}`) is the documented internal path; live-verify against a
// real file_ id. list/search ride get-nested-files and work regardless.

async fn list_files(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let project_id = optional_str(params, "project_id");
    let files = collect_items(state, &sess, project_id.as_deref(), ItemKind::File).await?;
    Ok(json!({ "files": files }))
}

async fn get_file(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let id = require_str(params, "file_id")?;
    api_get(
        state,
        &sess,
        &format!("/files/{}", urlencoding::encode(&id)),
    )
    .await
}

async fn search_files(state: &AppState, params: &Value) -> Result<Value, String> {
    let sess = session(state).await?;
    let query = require_str(params, "query")?;
    let project_id = optional_str(params, "project_id");
    // The exact TYPES token for uploaded files is uncertain, so search broadly and
    // keep only file_ ids (classify by prefix). This stays correct even if the
    // "pipeline_file" token doesn't cover every uploaded-file variant.
    let resp = ranked_search(state, &sess, &query, ALL_SEARCH_TYPES, search_limit(params)).await?;
    let rows = project_search_results(&resp, Some(ItemKind::File), project_id.as_deref(), None);
    Ok(search_envelope("files", rows, &resp))
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

/// Case-insensitive substring match. Still used by `search_projects`, which filters
/// the small `/folders` list locally rather than through the ranked item search
/// (projects are folders, not search items).
fn name_contains(name: &str, query: &str) -> bool {
    name.to_lowercase().contains(&query.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csrf_post_sends_referer_matching_tenant_origin() {
        // Root cause of the "search returns 400" bug: Benchling's Flask-WTF CSRF
        // guard rejects HTTPS POSTs with no Referer ("The referrer header is
        // missing."), and requires it to share the request origin. reqwest does not
        // set Referer automatically the way a browser does, so this header must be
        // present and point at the tenant. Without it every /search POST is a CSRF
        // 400 while GET reads (not CSRF-checked) keep working.
        let sess = BenchlingSession {
            session_cookie: "cookievalue".to_string(),
            tenant_host: "benchling.com".to_string(),
            user_handle: "mstrome".to_string(),
        };
        let headers = csrf_post_headers(&sess, "tok123");

        let referer = headers
            .get("Referer")
            .expect("CSRF POST must send a Referer header or Benchling returns a 400");
        assert_eq!(referer, "https://benchling.com/");

        // The token and session must still ride along, and the request must be
        // flagged as an XHR (Benchling only serves the JSON search that way).
        assert_eq!(headers.get("X-CSRFToken").unwrap(), "tok123");
        assert_eq!(headers.get("Cookie").unwrap(), "session=cookievalue");
        assert_eq!(headers.get("X-Requested-With").unwrap(), "XMLHttpRequest");
    }

    #[test]
    fn classify_id_maps_prefixes() {
        assert_eq!(classify_id("etr_abc"), ItemKind::Entry);
        assert_eq!(classify_id("seq_abc"), ItemKind::Sequence);
        assert_eq!(classify_id("prt_abc"), ItemKind::Protocol);
        assert_eq!(classify_id("prtn_abc"), ItemKind::AaSequence);
        assert_eq!(classify_id("file_abc"), ItemKind::File);
        // A registry-style id (no known prefix) is a custom entity.
        assert_eq!(classify_id("bfi_abc"), ItemKind::CustomEntity);
    }

    #[test]
    fn classify_id_does_not_confuse_protocol_and_aa_sequence() {
        // prt_ (protocol) and prtn_ (AA sequence) share a leading "prt"; the
        // classifier must keep them distinct or proteins leak into protocols.
        assert_eq!(classify_id("prt_9elVn4zgKN"), ItemKind::Protocol);
        assert_eq!(classify_id("prtn_ID7vHq6T6D"), ItemKind::AaSequence);
    }

    #[test]
    fn dispatch_routes_new_type_tools() {
        // The protocol/AA/file tools must be reachable through the central prefix
        // router; a missing arm would surface as "Unknown Benchling tool" at runtime.
        for tool in [
            "benchling_list_protocols",
            "benchling_get_protocol",
            "benchling_search_protocols",
            "benchling_list_aa_sequences",
            "benchling_get_aa_sequence",
            "benchling_search_aa_sequences",
            "benchling_list_files",
            "benchling_get_file",
            "benchling_search_files",
        ] {
            assert!(handles(tool), "{tool} not handled");
        }
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
    fn absolutize_urls_preserves_edit_suffix() {
        // Regression: Benchling returns the item link as a relative editURL ending
        // in `/edit`. The non-/edit form 404s on path-handle (free-tier) tenants, so
        // the suffix must be preserved (it was previously stripped -> dead link).
        let mut v = json!({
            "editURL": "/david-beakr/f/lib_1-proj/seq_1-scramble/edit",
            "avatar_url": "https://cdn.example.com/x.png",
        });
        absolutize_urls(&mut v, "benchling.com");
        assert_eq!(
            v["editURL"],
            json!("https://benchling.com/david-beakr/f/lib_1-proj/seq_1-scramble/edit")
        );
        // Absolute URLs (not starting with `/`) are left untouched.
        assert_eq!(v["avatar_url"], json!("https://cdn.example.com/x.png"));
    }

    #[test]
    fn folder_id_prefers_api_identifier() {
        let f = json!({ "api_identifier": "lib_1", "id": "internal_1" });
        assert_eq!(folder_id(&f).as_deref(), Some("lib_1"));
        let f2 = json!({ "id": "internal_2" });
        assert_eq!(folder_id(&f2).as_deref(), Some("internal_2"));
    }

    #[test]
    fn normalize_project_surfaces_lib_id_for_get_roundtrip() {
        // Regression: list_projects returned the raw numeric `id`, but get_project
        // hits /folders/{id} which only accepts the `lib_…` api_identifier, so the
        // agent's list->get round-trip 404'd. normalize_project must surface the
        // lib_ id as `id` (numeric preserved as db_id).
        let raw = json!({ "id": 8160812, "api_identifier": "lib_abc", "name": "Cancer Genomics" });
        let n = normalize_project(raw);
        assert_eq!(n.get("id").and_then(|v| v.as_str()), Some("lib_abc"));
        assert_eq!(n.get("db_id").and_then(|v| v.as_i64()), Some(8160812));
        assert_eq!(
            n.get("name").and_then(|v| v.as_str()),
            Some("Cancer Genomics")
        );
    }

    #[test]
    fn name_filter_is_case_insensitive_contains() {
        assert!(name_contains("My Plasmid Library", "plasmid"));
        assert!(name_contains("PCR-01", "pcr"));
        assert!(!name_contains("Genome", "plasmid"));
    }

    #[test]
    fn extract_csrf_meta_reads_content_attr() {
        // The token lives in the server-rendered app shell as a meta tag; the SPA
        // strips it from the live DOM, so we parse the raw HTML (verified 2026-07-05).
        let html = r#"<head><meta name="csrf-token" content="ImQ3NDhmYWU4Ig.akpjDw.sig"/></head>"#;
        assert_eq!(
            extract_csrf_meta(html).as_deref(),
            Some("ImQ3NDhmYWU4Ig.akpjDw.sig")
        );
        // Single-quoted variant.
        let single = "<meta content='tok123' name='csrf-token'>";
        // name comes after content here, so the anchor-on-name path must still find
        // the nearest content= — which precedes it. This returns None (content= is
        // before the anchor), so callers fall through to the next page candidate.
        assert_eq!(extract_csrf_meta(single), None);
        // Missing tag -> None.
        assert_eq!(extract_csrf_meta("<html><body>no token</body></html>"), None);
    }

    #[test]
    fn search_limit_clamps_and_defaults() {
        assert_eq!(search_limit(&json!({})), SEARCH_DEFAULT_LIMIT);
        assert_eq!(search_limit(&json!({ "limit": 5 })), 5);
        // Numeric strings (how the backend forwards agent args) are accepted.
        assert_eq!(search_limit(&json!({ "limit": "10" })), 10);
        // Over the cap is clamped; zero/garbage falls back sanely to >= 1.
        assert_eq!(search_limit(&json!({ "limit": 9999 })), SEARCH_MAX_LIMIT);
        assert_eq!(search_limit(&json!({ "limit": 0 })), 1);
        // page_size is accepted as an alias for the schema's pagination hint.
        assert_eq!(search_limit(&json!({ "page_size": 7 })), 7);
    }

    #[test]
    fn merge_summaries_flattens_all_by_id_maps() {
        let resp = json!({
            "entrySummariesById": { "etr_1": { "name": "Entry One" } },
            "protocolSummariesById": { "prt_1": { "name": "Protocol One" } },
            "scoresById": { "etr_1": 0.9 },
            "ids": ["etr_1", "prt_1"],
        });
        let merged = merge_summaries(&resp);
        assert_eq!(merged.get("etr_1").unwrap()["name"], json!("Entry One"));
        assert_eq!(merged.get("prt_1").unwrap()["name"], json!("Protocol One"));
        // scoresById is also a *ById map but its values aren't summaries; harmless to
        // include since lookups are by the caller's item id against summary maps.
        assert!(merged.contains_key("etr_1"));
    }

    /// A response shaped like the live `/1/api/search` payload for assertions.
    fn sample_search_response() -> Value {
        json!({
            "ids": ["prt_lig", "etr_old", "etr_other_proj"],
            "total": 3,
            "isExact": true,
            "timedOut": false,
            "scoresById": { "prt_lig": 12.5, "etr_old": 3.1, "etr_other_proj": 2.0 },
            "protocolSummariesById": {
                "prt_lig": {
                    "name": "Ligation Protocol",
                    "modified_at": "2026-06-20T10:00:00+00:00",
                    "created_at": "2026-01-01T00:00:00+00:00",
                    "editURL": "https://benchling.com/mstrome/f/lib_A-proj/prt_lig-ligation/edit",
                    "folder": { "api_identifier": "lib_A", "name": "Cloning" }
                }
            },
            "entrySummariesById": {
                "etr_old": {
                    "name": "Old Prep",
                    "modified_at": "2026-02-01T00:00:00+00:00",
                    "folder": { "api_identifier": "lib_A", "name": "Cloning" }
                },
                "etr_other_proj": {
                    "name": "Other Project Note",
                    "modified_at": "2026-06-25T00:00:00+00:00",
                    "folder": { "api_identifier": "lib_B", "name": "Sequencing" }
                }
            }
        })
    }

    #[test]
    fn project_search_results_preserves_rank_and_projects_fields() {
        let rows = project_search_results(&sample_search_response(), None, None, None);
        // Ranked order from `ids` is preserved.
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["id"], json!("prt_lig"));
        assert_eq!(rows[0]["type"], json!("protocol"));
        assert_eq!(rows[0]["name"], json!("Ligation Protocol"));
        assert_eq!(rows[0]["project_id"], json!("lib_A"));
        assert_eq!(rows[0]["project_name"], json!("Cloning"));
        assert_eq!(rows[0]["score"], json!(12.5));
        assert_eq!(rows[0]["match_source"], json!("ranked_content"));
        // editURL is surfaced as the clickable link.
        assert_eq!(
            rows[0]["url"],
            json!("https://benchling.com/mstrome/f/lib_A-proj/prt_lig-ligation/edit")
        );
    }

    #[test]
    fn project_search_results_filters_by_kind() {
        let rows = project_search_results(
            &sample_search_response(),
            Some(ItemKind::Entry),
            None,
            None,
        );
        // Only the etr_ ids survive the kind filter; the protocol is dropped.
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r["type"] == json!("entry")));
    }

    #[test]
    fn project_search_results_filters_by_project() {
        // Scoping to lib_B keeps only the entry whose folder.api_identifier matches.
        let rows =
            project_search_results(&sample_search_response(), None, Some("lib_B"), None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], json!("etr_other_proj"));
    }

    #[test]
    fn project_search_results_filters_by_modified_after() {
        // Only items modified on/after the floor survive; undated items are dropped.
        let rows = project_search_results(
            &sample_search_response(),
            None,
            None,
            Some("2026-06-01"),
        );
        let ids: Vec<&str> = rows.iter().filter_map(|r| r["id"].as_str()).collect();
        assert!(ids.contains(&"prt_lig")); // 2026-06-20
        assert!(ids.contains(&"etr_other_proj")); // 2026-06-25
        assert!(!ids.contains(&"etr_old")); // 2026-02-01, before the floor
    }

    #[test]
    fn search_envelope_carries_ranking_metadata() {
        let resp = sample_search_response();
        let rows = project_search_results(&resp, None, None, None);
        let env = search_envelope("results", rows, &resp);
        assert_eq!(env["search_strategy"], json!("ranked_content_search"));
        assert_eq!(env["total_ranked"], json!(3));
        assert_eq!(env["is_exact"], json!(true));
        assert_eq!(env["timed_out"], json!(false));
        assert!(env["results"].is_array());
    }

    #[test]
    fn dispatch_routes_global_search_tool() {
        // benchling_search is the new global ranked-search tool; it must be routable.
        assert!(handles("benchling_search"));
    }

    #[test]
    fn handles_matches_benchling_prefix_only() {
        assert!(handles("benchling_list_projects"));
        assert!(handles("benchling_get_entry"));
        assert!(handles("benchling_search"));
        assert!(!handles("list_files"));
    }
}
