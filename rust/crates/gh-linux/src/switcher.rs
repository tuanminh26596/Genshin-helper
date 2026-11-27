//! Window switcher for Linux — X11-based game/browser window toggling.
//!
//! Mirrors `gh-win32::switcher`.

use crate::capture::WindowFinder;
use std::time::{Duration, Instant};

const FOCUS_TAB_POLL_TIMEOUT: Duration = Duration::from_millis(150);
const FOCUS_TAB_POLL_INTERVAL: Duration = Duration::from_millis(15);

pub trait SwitcherOps {
    fn foreground_hwnd(&self) -> isize;
    fn find_game_window_any_state(&self, titles: &[&str]) -> Option<isize>;
    fn find_browser_window_any_state(&self, specific_only: bool) -> Option<isize>;
    fn focus_window(&self, hwnd: isize) -> bool;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LinuxSwitcherOps;

impl SwitcherOps for LinuxSwitcherOps {
    fn foreground_hwnd(&self) -> isize {
        WindowFinder::get_foreground_window().unwrap_or(0)
    }

    fn find_game_window_any_state(&self, titles: &[&str]) -> Option<isize> {
        WindowFinder::find_game_window_any_state(titles)
    }

    fn find_browser_window_any_state(&self, specific_only: bool) -> Option<isize> {
        WindowFinder::find_browser_window_any_state(specific_only)
    }

    fn focus_window(&self, hwnd: isize) -> bool {
        WindowFinder::focus_window(hwnd)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SandboxSwitcherOps;

impl SwitcherOps for SandboxSwitcherOps {
    fn foreground_hwnd(&self) -> isize {
        LinuxSwitcherOps.foreground_hwnd()
    }

    fn find_game_window_any_state(&self, titles: &[&str]) -> Option<isize> {
        LinuxSwitcherOps.find_game_window_any_state(titles)
    }

    fn find_browser_window_any_state(&self, specific_only: bool) -> Option<isize> {
        LinuxSwitcherOps.find_browser_window_any_state(specific_only)
    }

    fn focus_window(&self, _hwnd: isize) -> bool {
        // Sandbox: no-op
        true
    }
}

pub struct WindowSwitcher;

impl WindowSwitcher {
    pub fn toggle_game_and_browser(
        titles: &[&str],
        on_focus_browser_tab: Option<&dyn Fn()>,
    ) -> &'static str {
        Self::toggle_with_ops(&LinuxSwitcherOps, titles, on_focus_browser_tab)
    }

    pub fn toggle_with_ops<O: SwitcherOps>(
        ops: &O,
        titles: &[&str],
        on_focus_browser_tab: Option<&dyn Fn()>,
    ) -> &'static str {
        let fg = ops.foreground_hwnd();
        let game_hwnd = ops.find_game_window_any_state(titles);
        let browser_hwnd = ops.find_browser_window_any_state(false);

        let game_is_fg = game_hwnd.map(|h| h == fg).unwrap_or(false);
        let browser_is_fg = browser_hwnd.map(|h| h == fg).unwrap_or(false);

        if game_is_fg {
            // Switch to browser
            if let Some(bh) = browser_hwnd {
                if let Some(cb) = on_focus_browser_tab {
                    cb();
                }
                ops.focus_window(bh);
                return "browser";
            }
            return "no_browser";
        }

        if browser_is_fg {
            // Switch back to game
            if let Some(gh) = game_hwnd {
                ops.focus_window(gh);
                return "game";
            }
            return "none";
        }

        // Neither is foreground; focus game first
        if let Some(gh) = game_hwnd {
            ops.focus_window(gh);
            return "game";
        }
        "none"
    }
}
