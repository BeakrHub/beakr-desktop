//! Benchling connector for FREE Benchling users.
//!
//! Opens benchling.com in a webview where the user logs in with their own
//! session, then gathers items via Benchling's internal `/1/api/*` endpoints
//! (the official `/api/v2` is not available to free users) and pushes them to the
//! Beakr backend using the stored device token.
//!
//! Modules:
//!   - `gather_script`: the injected page-context JS. The ONE uncertain piece —
//!     the entry/sequence endpoint shapes — lives in its `fetchFolderItems`.
//!   - `bridge`: the localhost HTTP listener that receives the gathered JSON.
//!   - `commands`: the Tauri commands (`connect_benchling`, `benchling_import`)
//!     and the import driver that pushes items to the backend.

pub mod bridge;
pub mod commands;
pub mod gather_script;
