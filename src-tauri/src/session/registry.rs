//! Provider registry for the generic session connector.
//!
//! A "session connector" lets a user log into a login-walled website with their
//! own browser session inside a Tauri webview, then runs a page-context "gather
//! script" that reads data via the site's own (cookie-authenticated) endpoints
//! and POSTs it back to a localhost bridge for upload to Beakr.
//!
//! Adding a new provider is a two-step change:
//!   1. Add a gather script under `session::scripts` (one file, one `const`).
//!   2. Add a `Provider` arm here mapping the provider key to its url + script.
//!      Then list the provider origin in `capabilities/session-connectors.json`.
//!
//! Everything else — the bridge, the import driver, the commands, the frontend
//! `SessionConnect` component — is provider-agnostic and needs no changes.

use crate::session::scripts;

/// A registered session-capture provider: the site to open and the gather script
/// to inject once the user has logged in.
///
/// The provider key itself is not stored here — it is the lookup key passed to
/// [`lookup`] and carried independently by the caller (window label, events,
/// backend body), so duplicating it on the struct would just risk drift.
#[derive(Debug, Clone, Copy)]
pub struct Provider {
    /// The URL opened in the session webview window for the user to log into.
    pub url: &'static str,
    /// The page-context gather script. `PORT_PLACEHOLDER` is substituted with the
    /// localhost bridge port immediately before injection.
    pub gather_script: &'static str,
}

/// Looks up a provider by its key. Returns `None` for unknown keys so callers can
/// surface a clean error to the frontend rather than panicking.
pub fn lookup(key: &str) -> Option<Provider> {
    match key {
        "benchling" => Some(Provider {
            url: "https://benchling.com",
            gather_script: scripts::benchling::BENCHLING_GATHER_SCRIPT,
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
