//! Live "session connector": capture a user's logged-in browser session.
//!
//! A session connector opens a provider's site in a Tauri webview where the user
//! logs in with their OWN browser session. We then capture the session cookie and
//! use it to call the provider's own (cookie-authenticated) endpoints directly
//! from Rust. This works even when a provider's official API is not available to
//! the user's plan (the original Benchling case: `/api/v2` is gated to paid tiers,
//! so the live tools use Benchling's internal `/1/api/*`).
//!
//! The pieces:
//!   - `registry`: maps a provider key -> { url, window-label } for the webview to
//!     open. Provider-agnostic.
//!   - `benchling`: the Benchling connect flow (login watch, session capture,
//!     connector registration, liveness watcher).
//!   - `commands`: the Tauri commands (`connect_session`, `benchling_status`).

pub mod benchling;
pub mod commands;
pub mod registry;
