//! Page-context "gather scripts" for each session connector provider.
//!
//! Every gather script follows the SAME data contract (see `benchling.rs` for the
//! reference implementation):
//!   - It is an async IIFE injected via `webview.eval` into the provider's origin.
//!   - It reads the localhost bridge port from the substituted `PORT_PLACEHOLDER`
//!     token and POSTs JSON messages to `http://127.0.0.1:<port>/session/ingest`.
//!   - Messages are tagged objects: `{type:"progress"|"needs_login"|"complete"|"error", ...}`.
//!   - On `complete` it sends `{ user_handle, tenant_host, items }` where each item
//!     is `{ external_id, kind, title, url, content, checksum, metadata }`.
//!
//! `PORT_PLACEHOLDER` is shared so the injection substitution is identical for
//! every provider.

pub mod benchling;
pub mod labarchives;

/// Placeholder token replaced by Rust with the localhost bridge port before a
/// gather script is injected. Shared by all provider scripts.
pub const PORT_PLACEHOLDER: &str = "__BEAKR_BRIDGE_PORT__";
