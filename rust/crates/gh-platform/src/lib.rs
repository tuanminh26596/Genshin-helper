//! Platform abstraction layer: re-exports `gh-win32` on Windows and `gh-linux`
//! on Linux, providing a unified `gh_platform::*` namespace for all platform-
//! specific functionality consumed by `gh-hud`, `gh-app`, and `gh-cv`.

#[cfg(windows)]
pub use gh_win32::*;

#[cfg(target_os = "linux")]
pub use gh_linux::*;

// Compiler error if neither platform is supported
#[cfg(not(any(windows, target_os = "linux")))]
compile_error!("gh-platform: only Windows and Linux are supported");
