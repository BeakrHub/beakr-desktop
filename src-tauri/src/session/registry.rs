//! Provider registry for the live session connector.
//!
//! A "session connector" lets a user log into a login-walled website with their
//! own browser session inside a Tauri webview, after which we capture the session
//! cookie and call the site's own (cookie-authenticated) endpoints from Rust (see
//! `session::benchling` and `tools::benchling`).
//!
//! Adding a new provider is a one-line change here: add a `Provider` arm mapping
//! the provider key to its url. Then list the provider origin in
//! `capabilities/session-connectors.json`. Everything else — the commands and the
//! frontend `SessionConnect` component — is provider-agnostic and needs no changes.

/// A registered session-capture provider: the site to open in the session webview.
///
/// The provider key itself is not stored here — it is the lookup key passed to
/// [`lookup`] and carried independently by the caller (window label, events,
/// backend body), so duplicating it on the struct would just risk drift.
#[derive(Debug, Clone, Copy)]
pub struct Provider {
    /// The URL opened in the session webview window for the user to log into.
    pub url: &'static str,
}

/// Looks up a provider by its key. Returns `None` for unknown keys so callers can
/// surface a clean error to the frontend rather than panicking.
pub fn lookup(key: &str) -> Option<Provider> {
    match key {
        "benchling" => Some(Provider {
            url: "https://benchling.com",
        }),
        _ => None,
    }
}

/// The Tauri window label for a provider's session-capture webview.
///
/// Labels follow the `session-<provider>` convention so a single capability with
/// a `session-*` window glob covers every provider (see
/// `capabilities/session-connectors.json`).
pub fn window_label(provider: &str) -> String {
    format!("session-{provider}")
}
