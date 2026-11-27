//! Capture outcome types mirroring `gh-win32::wgc_hdr` for platform parity.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WgcWindowCaptureOutcome<T> {
    Frame(T),
    WarmingUp,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonitorHdrInfo {
    pub sdr_white_nits: f32,
}
