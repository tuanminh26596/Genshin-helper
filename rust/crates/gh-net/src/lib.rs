//! Networking crate: 127.0.0.1 Sync HTTP Server with lossless `/poll?since=<seq>`,
//! opt-in LAN Companion HTTP/WebSocket + QR SVG, in-process HoYoLAB dataset sync,
//! non-blocking HD/underground tile providers, and bilingual Pin Guide fetcher.
//!
//! Acyclic dependency: depends ONLY on `gh-core`.

pub mod cross_map_service;
pub mod hd_assets;
pub mod hoyolab_sync;
pub mod lan_companion;
pub mod pin_guide;
pub mod sync_server;
pub mod update_check;

pub use gh_core::traits::{
    ConflictAdvisor, DatasetSyncRunner, DiagnosticService, ServerCallbacks, TileProvider,
    UndergroundOverlaySource,
};
