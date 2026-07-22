//! BackLog's deterministic trust core: the §5a evidence harvest and the §6
//! validator. Per the design doc these two modules ARE the product — nothing
//! reaches a filesystem or SharePoint without passing the checker.
//!
//! They are deliberately pure: no Tauri, no sidecar, no process spawning, only
//! std + a few small crates. That keeps them in their own crate so
//! `cargo test -p backlog-core` runs every harvest and checker rule instantly,
//! with no sidecar binaries, no icon, and no Tauri build — the fast, cheap
//! safety net the README asks you to run before touching either file.

pub mod checker;
pub mod harvest;
