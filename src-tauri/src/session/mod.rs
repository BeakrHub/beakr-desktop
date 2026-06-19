//! Generic "session connector": import data from login-walled websites.
//!
//! A session connector opens a provider's site in a Tauri webview where the user
//! logs in with their OWN browser session, then runs a page-context "gather
//! script" that reads data via the site's own (cookie-authenticated) endpoints and
//! POSTs it to a localhost bridge, which uploads it to the Beakr backend using the
//! stored device token. This works even when a provider's official API is not
//! available to the user's plan (the original Benchling case: `/api/v2` is gated to
//! paid tiers, so the connector uses Benchling's internal `/1/api/*`).
//!
//! The pieces are provider-agnostic:
//!   - `registry`: maps a provider key -> { url, gather_script }. Adding a provider
//!     is one registry arm + one gather script.
//!   - `scripts`: the injected page-context gather scripts (one module per
//!     provider). All follow the same data contract (see `scripts::benchling`).
//!   - `bridge`: the localhost HTTP listener that receives the gathered JSON and
//!     emits generic `session:*` frontend events (each carrying `provider`).
//!   - `commands`: the Tauri commands (`connect_session`, `session_import`) and the
//!     import driver that pushes items to the backend.

pub mod benchling;
pub mod bridge;
pub mod commands;
pub mod registry;
pub mod scripts;
