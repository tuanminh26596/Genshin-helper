//! Leaf crate for Genshin-Helper: shared constants, config schema, path layout,
//! pixel buffers, HTTP/WS/Native-Messaging protocol DTOs, and acyclic boundary traits.
//!
//! Strictly zero Win32, zero HTTP runtime, and zero CV library dependencies.

pub mod assets;
pub mod atomic_io;
pub mod config;
pub mod cross_map_import;
pub mod frame;
pub mod i18n;
pub mod map_bridge;
pub mod multimap;
pub mod nav_controller;
pub mod npz;
pub mod paths;
pub mod protocol;
pub mod provenance;
pub mod route_doc;
pub mod sha256;
pub mod traits;
pub mod tsp_route;
pub mod ui_layout;

use thiserror::Error;

pub const APP_VERSION: &str = "5.8.0";
pub const SCRIPT_VERSION: &str = "v5.8.0";
pub const HOST_VERSION: &str = "5.8.0";

pub const PINNED_EXTENSION_ID: &str = "nfkijdoblmphmdljfogooajhbldkeojh";
pub const NATIVE_HOST_NAME: &str = "com.genshinhelper.host";
pub const ELEVATED_TASK_NAME: &str = "GenshinHelper_Elevated";
pub const SINGLE_INSTANCE_MUTEX: &str = r"Local\GenshinHelper_SingleInstance_v4";

pub const DEFAULT_SYNC_HOST: &str = "127.0.0.1";
pub const DEFAULT_SYNC_PORT: u16 = 8765;
pub const DEFAULT_LAN_PORT: u16 = 8766;

/// Verified world-space Y anchor offset from `src/locator.py` (`WORLD_ANCHOR_Y_OFFSET`).
pub const WORLD_ANCHOR_Y_OFFSET: f64 = -8.5;
pub const WORLD_ANCHOR_MAP_IDS: [i32; 4] = [2, 9, 34, 36];
pub const ARROW_PIVOT_OFFSET_PX_1440: f64 = 2.78;

/// Win32 message & affinity constants shared between `gh-net`, `gh-win32`, and `gh-hud`.
pub const WM_APP_CAPTURE_AFFINITY: u32 = 0x8015;
pub const WDA_NONE: u32 = 0x00;
pub const WDA_EXCLUDEFROMCAPTURE: u32 = 0x11;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("ZIP archive error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("invalid data: {0}")]
    InvalidData(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("sync error: {0}")]
    Sync(String),
    #[error("not implemented yet: {0}")]
    Unimplemented(&'static str),
}

pub type CoreResult<T> = Result<T, CoreError>;

pub(crate) fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Ok(<Option<T> as serde::Deserialize<'de>>::deserialize(deserializer)?.unwrap_or_default())
}
