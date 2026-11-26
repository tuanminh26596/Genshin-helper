//! Game <-> Browser `WindowSwitcher` using `AllowSetForegroundWindow` (`src/single_monitor.py`).
//!
//! Invariant: Read-only window focus transfer (`SetForegroundWindow` + `AttachThreadInput` fallback
//! via `crate::capture::WindowFinder::focus_window`), strictly zero `SendInput`.

use crate::capture::WindowFinder;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

const FOCUS_TAB_POLL_TIMEOUT: Duration = Duration::from_millis(150);
const FOCUS_TAB_POLL_INTERVAL: Duration = Duration::from_millis(15);

/// Testable Win32 window operations seam for `WindowSwitcher`.
pub trait SwitcherOps {
    fn foreground_hwnd(&self) -> isize;
    fn find_game_window_any_state(&self, titles: &[&str]) -> Option<isize>;
    fn find_browser_window_any_state(&self, specific_only: bool) -> Option<isize>;
    fn find_browser_window_for_provider(
        &self,
        _preferred_provider: Option<&str>,
        specific_only: bool,
    ) -> Option<isize> {
        self.find_browser_window_any_state(specific_only)
    }
    fn focus_window(&self, hwnd: isize) -> bool;
}

/// Live Win32 implementation backed by `GetForegroundWindow` and `crate::capture::WindowFinder`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Win32SwitcherOps;

impl SwitcherOps for Win32SwitcherOps {
    fn foreground_hwnd(&self) -> isize {
        // SAFETY: Querying current foreground window handle.
        unsafe { GetForegroundWindow().0 as isize }
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

/// Desktop-safe ops for the black-box sandbox (`GH_DESKTOP_SANDBOX=1`): real read-only window
/// lookup, but `focus_window` is a no-op so tests never steal focus or restore the game window.
#[derive(Debug, Default, Clone, Copy)]
pub struct SandboxSwitcherOps;

impl SwitcherOps for SandboxSwitcherOps {
    fn foreground_hwnd(&self) -> isize {
        Win32SwitcherOps.foreground_hwnd()
    }

    fn find_game_window_any_state(&self, titles: &[&str]) -> Option<isize> {
        Win32SwitcherOps.find_game_window_any_state(titles)
    }

    fn find_browser_window_any_state(&self, specific_only: bool) -> Option<isize> {
        Win32SwitcherOps.find_browser_window_any_state(specific_only)
    }

    fn find_browser_window_for_provider(
        &self,
        preferred_provider: Option<&str>,
        specific_only: bool,
    ) -> Option<isize> {
        Win32SwitcherOps.find_browser_window_for_provider(preferred_provider, specific_only)
    }

    fn focus_window(&self, _hwnd: isize) -> bool {
        true
    }
}

/// Single-Monitor quick toggle between Genshin Impact and the active interactive map browser.
pub struct WindowSwitcher;

impl WindowSwitcher {
    /// Toggles focus between the game window matching `titles` and the browser map window.
    ///
    /// Returns `"browser"`, `"no_browser"`, `"game"`, or `"none"`, matching
    /// `src/single_monitor.py::WindowSwitcher.toggle_game_and_browser`. Under
    /// `GH_DESKTOP_SANDBOX=1` the same decision runs but no window is focused.
    pub fn toggle_game_and_browser(
        titles: &[&str],
        on_focus_browser_tab: Option<&dyn Fn()>,
    ) -> &'static str {
        Self::toggle_game_and_browser_for_provider(titles, None, on_focus_browser_tab)
    }

    /// Provider-aware toggle: prioritizes the browser window hosting `preferred_provider`
    /// when multiple browsers with different map sources are open simultaneously.
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
            &Win32SwitcherOps,
            titles,
            preferred_provider,
            on_focus_browser_tab,
            FOCUS_TAB_POLL_TIMEOUT,
            FOCUS_TAB_POLL_INTERVAL,
        )
    }

    /// Injectable implementation of `toggle_game_and_browser` used by unit tests.
    pub fn toggle_with_ops<O: SwitcherOps>(
        ops: &O,
        titles: &[&str],
        on_focus_browser_tab: Option<&dyn Fn()>,
        poll_timeout: Duration,
        poll_interval: Duration,
    ) -> &'static str {
        Self::toggle_with_ops_for_provider(
            ops,
            titles,
            None,
            on_focus_browser_tab,
            poll_timeout,
            poll_interval,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    struct MockSwitcherOps {
        fg: isize,
        game: Option<isize>,
        specific_seq: RefCell<Vec<Option<isize>>>,
        generic_browser: Option<isize>,
        focused: RefCell<Vec<isize>>,
        specific_calls: Cell<usize>,
        generic_calls: Cell<usize>,
    }

    impl MockSwitcherOps {
        fn new(
            fg: isize,
            game: Option<isize>,
            specific_seq: Vec<Option<isize>>,
            generic_browser: Option<isize>,
        ) -> Self {
            Self {
                fg,
                game,
                specific_seq: RefCell::new(specific_seq),
                generic_browser,
                focused: RefCell::new(Vec::new()),
                specific_calls: Cell::new(0),
                generic_calls: Cell::new(0),
            }
        }
    }

    impl SwitcherOps for MockSwitcherOps {
        fn foreground_hwnd(&self) -> isize {
            self.fg
        }

        fn find_game_window_any_state(&self, _titles: &[&str]) -> Option<isize> {
            self.game
        }

        fn find_browser_window_any_state(&self, specific_only: bool) -> Option<isize> {
            if specific_only {
                let idx = self.specific_calls.get();
                self.specific_calls.set(idx + 1);
                let seq = self.specific_seq.borrow();
                seq.get(idx).copied().flatten()
            } else {
                self.generic_calls.set(self.generic_calls.get() + 1);
                self.generic_browser
            }
        }

        fn focus_window(&self, hwnd: isize) -> bool {
            self.focused.borrow_mut().push(hwnd);
            true
        }
    }

    #[test]
    fn toggle_from_game_foreground_invokes_callback_and_focuses_specific_browser() {
        let ops = MockSwitcherOps::new(100, Some(100), vec![Some(200)], Some(300));
        let cb_calls = Cell::new(0usize);
        let cb = || cb_calls.set(cb_calls.get() + 1);

        let res = WindowSwitcher::toggle_with_ops(
            &ops,
            &["Genshin Impact"],
            Some(&cb),
            Duration::from_millis(150),
            Duration::from_millis(5),
        );

        assert_eq!(res, "browser");
        assert_eq!(cb_calls.get(), 1);
        assert_eq!(ops.specific_calls.get(), 1);
        assert_eq!(ops.generic_calls.get(), 0);
        assert_eq!(*ops.focused.borrow(), vec![200]);
    }

    #[test]
    fn toggle_from_game_foreground_polls_until_delayed_specific_browser_appears() {
        let ops = MockSwitcherOps::new(100, Some(100), vec![None, None, Some(205)], Some(300));
        let cb_calls = Cell::new(0usize);
        let cb = || cb_calls.set(cb_calls.get() + 1);

        let res = WindowSwitcher::toggle_with_ops(
            &ops,
            &["Genshin Impact"],
            Some(&cb),
            Duration::from_millis(150),
            Duration::from_millis(5),
        );

        assert_eq!(res, "browser");
        assert_eq!(cb_calls.get(), 1);
        assert_eq!(ops.specific_calls.get(), 3);
        assert_eq!(ops.generic_calls.get(), 0);
        assert_eq!(*ops.focused.borrow(), vec![205]);
    }

    #[test]
    fn toggle_from_game_foreground_falls_back_to_generic_browser_when_specific_times_out() {
        let ops = MockSwitcherOps::new(100, Some(100), vec![None], Some(300));
        let cb_calls = Cell::new(0usize);
        let cb = || cb_calls.set(cb_calls.get() + 1);

        let res = WindowSwitcher::toggle_with_ops(
            &ops,
            &["Genshin Impact"],
            Some(&cb),
            Duration::from_millis(25),
            Duration::from_millis(5),
        );

        assert_eq!(res, "browser");
        assert_eq!(cb_calls.get(), 1);
        assert!(ops.specific_calls.get() >= 1);
        assert_eq!(ops.generic_calls.get(), 1);
        assert_eq!(*ops.focused.borrow(), vec![300]);
    }

    #[test]
    fn toggle_from_game_foreground_without_callback_uses_generic_browser() {
        let ops = MockSwitcherOps::new(100, Some(100), vec![Some(200)], Some(300));

        let res = WindowSwitcher::toggle_with_ops(
            &ops,
            &["Genshin Impact"],
            None,
            Duration::from_millis(150),
            Duration::from_millis(5),
        );

        assert_eq!(res, "browser");
        assert_eq!(ops.specific_calls.get(), 0);
        assert_eq!(ops.generic_calls.get(), 1);
        assert_eq!(*ops.focused.borrow(), vec![300]);
    }

    #[test]
    fn toggle_from_game_foreground_returns_no_browser_when_none_found() {
        let ops = MockSwitcherOps::new(100, Some(100), vec![None], None);
        let cb = || {};

        let res = WindowSwitcher::toggle_with_ops(
            &ops,
            &["Genshin Impact"],
            Some(&cb),
            Duration::from_millis(20),
            Duration::from_millis(5),
        );

        assert_eq!(res, "no_browser");
        assert!(ops.focused.borrow().is_empty());
    }

    #[test]
    fn toggle_focuses_game_when_game_exists_but_not_foreground() {
        let ops = MockSwitcherOps::new(300, Some(100), vec![Some(200)], Some(300));
        let cb_calls = Cell::new(0usize);
        let cb = || cb_calls.set(cb_calls.get() + 1);

        let res = WindowSwitcher::toggle_with_ops(
            &ops,
            &["Genshin Impact"],
            Some(&cb),
            Duration::from_millis(150),
            Duration::from_millis(5),
        );

        assert_eq!(res, "game");
        assert_eq!(cb_calls.get(), 0);
        assert_eq!(*ops.focused.borrow(), vec![100]);
    }

    #[test]
    fn toggle_focuses_browser_when_no_game_exists() {
        let ops = MockSwitcherOps::new(999, None, vec![], Some(300));
        let res = WindowSwitcher::toggle_with_ops(
            &ops,
            &["Genshin Impact"],
            None,
            Duration::from_millis(20),
            Duration::from_millis(5),
        );

        assert_eq!(res, "browser");
        assert_eq!(*ops.focused.borrow(), vec![300]);
    }

    #[test]
    fn toggle_returns_none_when_neither_game_nor_browser_exists() {
        let ops = MockSwitcherOps::new(0, None, vec![], None);
        let res = WindowSwitcher::toggle_with_ops(
            &ops,
            &["Genshin Impact"],
            None,
            Duration::from_millis(20),
            Duration::from_millis(5),
        );

        assert_eq!(res, "none");
        assert!(ops.focused.borrow().is_empty());
    }

    #[test]
    fn sandbox_ops_look_up_windows_but_never_focus() {
        let ops = SandboxSwitcherOps;
        assert_eq!(
            ops.find_game_window_any_state(&["__GH_Nonexistent_Window_Title_Test__"]),
            None
        );
        // No-op focus reports success for any handle, including invalid ones, without
        // calling SetForegroundWindow / ShowWindow / AttachThreadInput.
        assert!(ops.focus_window(0));
        assert!(ops.focus_window(-1));
    }

    #[test]
    fn live_win32_ops_non_invasive_smoke() {
        let ops = Win32SwitcherOps;
        let _fg = ops.foreground_hwnd();
        assert_eq!(
            ops.find_game_window_any_state(&["__GH_Nonexistent_Window_Title_Test__"]),
            None
        );
    }
}
