//! Window switcher for Linux — Hyprland/X11-based game/browser window toggling.
//!
//! Mirrors `gh-win32::switcher`.

use crate::capture::WindowFinder;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

const FOCUS_TAB_POLL_TIMEOUT: Duration = Duration::from_millis(350);
const FOCUS_TAB_POLL_INTERVAL: Duration = Duration::from_millis(25);

pub trait SwitcherOps {
    fn foreground_hwnd(&self) -> isize;
    fn find_game_window_any_state(&self, titles: &[&str]) -> Option<isize>;
    fn find_browser_window_any_state(&self, specific_only: bool) -> Option<isize>;
    fn find_browser_window_for_provider(
        &self,
        preferred_provider: Option<&str>,
        specific_only: bool,
    ) -> Option<isize> {
        let _ = preferred_provider;
        self.find_browser_window_any_state(specific_only)
    }
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

    fn find_browser_window_for_provider(
        &self,
        preferred_provider: Option<&str>,
        specific_only: bool,
    ) -> Option<isize> {
        WindowFinder::find_browser_window_for_provider(preferred_provider, specific_only)
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

    fn find_browser_window_for_provider(
        &self,
        preferred_provider: Option<&str>,
        specific_only: bool,
    ) -> Option<isize> {
        LinuxSwitcherOps.find_browser_window_for_provider(preferred_provider, specific_only)
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
        Self::toggle_game_and_browser_for_provider(titles, None, on_focus_browser_tab)
    }

    /// Like [`toggle_game_and_browser`] but prefers a browser window hosting the given map provider
    /// (e.g. `"appsample"`, `"kongying"`, `"hoyolab"`). Falls back to any browser if not found.
    /// Mirrors `gh-win32::switcher::WindowSwitcher::toggle_game_and_browser_for_provider`.
    pub fn toggle_game_and_browser_for_provider(
        titles: &[&str],
        preferred_provider: Option<&str>,
        on_focus_browser_tab: Option<&dyn Fn()>,
    ) -> &'static str {
        if crate::runtime::is_desktop_sandbox_env() {
            return Self::toggle_with_ops_for_provider(
                &SandboxSwitcherOps,
                titles,
                preferred_provider,
                on_focus_browser_tab,
                FOCUS_TAB_POLL_TIMEOUT,
                FOCUS_TAB_POLL_INTERVAL,
            );
        }
        Self::toggle_with_ops_for_provider(
            &LinuxSwitcherOps,
            titles,
            preferred_provider,
            on_focus_browser_tab,
            FOCUS_TAB_POLL_TIMEOUT,
            FOCUS_TAB_POLL_INTERVAL,
        )
    }

    pub fn toggle_with_ops<O: SwitcherOps>(
        ops: &O,
        titles: &[&str],
        on_focus_browser_tab: Option<&dyn Fn()>,
    ) -> &'static str {
        Self::toggle_with_ops_for_provider(
            ops,
            titles,
            None,
            on_focus_browser_tab,
            FOCUS_TAB_POLL_TIMEOUT,
            FOCUS_TAB_POLL_INTERVAL,
        )
    }

    pub fn toggle_with_ops_for_provider<O: SwitcherOps>(
        ops: &O,
        titles: &[&str],
        preferred_provider: Option<&str>,
        on_focus_browser_tab: Option<&dyn Fn()>,
        poll_timeout: Duration,
        poll_interval: Duration,
    ) -> &'static str {
        let fg = ops.foreground_hwnd();
        let game_hwnd = ops.find_game_window_any_state(titles);

        if let Some(game) = game_hwnd
            && game != 0
            && fg == game
        {
            let mut browser_hwnd = None;
            if let Some(cb) = on_focus_browser_tab {
                let _ = catch_unwind(AssertUnwindSafe(cb));
                let deadline = Instant::now() + poll_timeout;
                while Instant::now() < deadline {
                    browser_hwnd = ops.find_browser_window_for_provider(preferred_provider, true);
                    if browser_hwnd.is_some() {
                        break;
                    }
                    std::thread::sleep(poll_interval);
                }
            }
            if browser_hwnd.is_none() {
                browser_hwnd = ops.find_browser_window_for_provider(preferred_provider, false);
            }
            if let Some(browser) = browser_hwnd {
                ops.focus_window(browser);
                return "browser";
            }
            return "no_browser";
        }

        if let Some(game) = game_hwnd {
            ops.focus_window(game);
            return "game";
        }

        if let Some(browser) = ops.find_browser_window_for_provider(preferred_provider, false) {
            ops.focus_window(browser);
            return "browser";
        }

        "none"
    }
}
