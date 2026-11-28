//! Linux system tray using `ksni` (pure-Rust D-Bus StatusNotifierItem).
//!
//! Provides the same `TrayState` enum and control API as `gh-win32::tray`.

use ksni::{self, MenuItem, Tray};
use std::sync::{Arc, Mutex};
use tracing::warn;

pub const ID_TOGGLE_ENGINE: u32 = 1001;
pub const ID_OPEN_MAP: u32 = 1002;
pub const ID_OPEN_DIAG: u32 = 1003;
pub const ID_OPEN_CONFIG: u32 = 1004;
pub const ID_QUIT: u32 = 1005;
pub const ID_LAUNCH_BORDERLESS: u32 = 1006;
pub const ID_SUPPORT: u32 = 1007;
pub const ID_CHECK_UPDATE: u32 = 1008;
pub const ID_STOP_SESSION_REC: u32 = 1009;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrayState {
    Connected,
    Idle,
    Paused,
    Offline,
}

impl TrayState {
    pub const ALL: [Self; 4] = [Self::Connected, Self::Idle, Self::Paused, Self::Offline];

    pub fn from_str_lossy(raw: &str) -> Self {
        match raw {
            "connected" => Self::Connected,
            "paused" => Self::Paused,
            "offline" => Self::Offline,
            _ => Self::Idle,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Idle => "idle",
            Self::Paused => "paused",
            Self::Offline => "offline",
        }
    }
}

pub type MenuAction = Box<dyn Fn(u32) + Send + 'static>;

pub struct TrayIcon {
    handle: Option<ksni::Handle<GenshinTray>>,
    shared: Arc<TrayShared>,
    _service: Option<ksni::TrayService<GenshinTray>>,
}

struct TrayShared {
    state: Mutex<TrayState>,
    engine_active: Mutex<bool>,
    session_rec_active: Mutex<bool>,
    action_cb: Mutex<Option<MenuAction>>,
}

struct GenshinTray {
    shared: Arc<TrayShared>,
}

impl Tray for GenshinTray {
    fn id(&self) -> String {
        "GenshinHelper".into()
    }

    fn title(&self) -> String {
        "Genshin Helper".into()
    }

    fn icon_name(&self) -> String {
        let state = *self.shared.state.lock().unwrap();
        match state {
            TrayState::Connected => "network-transmit-receive".into(),
            TrayState::Idle => "dialog-information".into(),
            TrayState::Paused => "media-playback-pause".into(),
            TrayState::Offline => "network-offline".into(),
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let engine_active = *self.shared.engine_active.lock().unwrap();
        let session_rec_active = *self.shared.session_rec_active.lock().unwrap();
        let toggle_label = if engine_active {
            "⏸ Pause Tracking"
        } else {
            "▶ Resume Tracking"
        };

        let mut items = vec![
            MenuItem::Standard(ksni::menu::StandardItem {
                label: toggle_label.into(),
                activate: Box::new(|this: &mut Self| {
                    if let Some(cb) = &*this.shared.action_cb.lock().unwrap() {
                        cb(ID_TOGGLE_ENGINE);
                    }
                }),
                ..Default::default()
            }),
            MenuItem::Separator,
        ];
        if session_rec_active {
            items.push(MenuItem::Standard(ksni::menu::StandardItem {
                label: "⏹ Stop Session Recording".into(),
                activate: Box::new(|this: &mut Self| {
                    if let Some(cb) = &*this.shared.action_cb.lock().unwrap() {
                        cb(ID_STOP_SESSION_REC);
                    }
                }),
                ..Default::default()
            }));
            items.push(MenuItem::Separator);
        }
        items.extend([
            MenuItem::Standard(ksni::menu::StandardItem {
                label: "🗺 Open HoYoLAB Map".into(),
                activate: Box::new(|this: &mut Self| {
                    if let Some(cb) = &*this.shared.action_cb.lock().unwrap() {
                        cb(ID_OPEN_MAP);
                    }
                }),
                ..Default::default()
            }),
            MenuItem::Standard(ksni::menu::StandardItem {
                label: "📂 Open Config".into(),
                activate: Box::new(|this: &mut Self| {
                    if let Some(cb) = &*this.shared.action_cb.lock().unwrap() {
                        cb(ID_OPEN_CONFIG);
                    }
                }),
                ..Default::default()
            }),
            MenuItem::Standard(ksni::menu::StandardItem {
                label: "🔍 Diagnostics".into(),
                activate: Box::new(|this: &mut Self| {
                    if let Some(cb) = &*this.shared.action_cb.lock().unwrap() {
                        cb(ID_OPEN_DIAG);
                    }
                }),
                ..Default::default()
            }),
            MenuItem::Separator,
            MenuItem::Standard(ksni::menu::StandardItem {
                label: "✖ Quit".into(),
                activate: Box::new(|this: &mut Self| {
                    if let Some(cb) = &*this.shared.action_cb.lock().unwrap() {
                        cb(ID_QUIT);
                    }
                }),
                ..Default::default()
            }),
        ]);
        items
    }
}

impl TrayIcon {
    pub fn new(action_cb: MenuAction) -> Option<Self> {
        let shared = Arc::new(TrayShared {
            state: Mutex::new(TrayState::Idle),
            engine_active: Mutex::new(false),
            session_rec_active: Mutex::new(false),
            action_cb: Mutex::new(Some(action_cb)),
        });

        let tray = GenshinTray {
            shared: Arc::clone(&shared),
        };

        let service = ksni::TrayService::new(tray);
        let handle = service.handle();
        std::thread::spawn(move || {
            if let Err(e) = service.run() {
                warn!("gh-linux: tray service error: {e}");
            }
        });
        Some(Self {
            handle: Some(handle),
            shared,
            _service: None,
        })
    }

    pub fn set_state(&self, state: TrayState, engine_active: bool) {
        *self.shared.state.lock().unwrap() = state;
        *self.shared.engine_active.lock().unwrap() = engine_active;
        if let Some(h) = &self.handle {
            h.update(|_| {});
        }
    }

    pub fn set_session_recording(&self, active: bool) {
        *self.shared.session_rec_active.lock().unwrap() = active;
        if let Some(h) = &self.handle {
            h.update(|_| {});
        }
    }

    pub fn close(&self) {
        // ksni handles cleanup automatically on drop
    }
}

pub type TrayCallback = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
pub struct TrayCallbacks {
    pub on_toggle_engine: Option<TrayCallback>,
    pub on_open_map: Option<TrayCallback>,
    pub on_open_diagnostics: Option<TrayCallback>,
    pub on_open_config_dir: Option<TrayCallback>,
    pub on_quit: Option<TrayCallback>,
    pub on_launch_borderless: Option<TrayCallback>,
    pub on_open_support: Option<TrayCallback>,
    pub on_check_update: Option<TrayCallback>,
    pub on_stop_session_rec: Option<TrayCallback>,
}

pub struct SystemTrayApp {
    callbacks: Arc<TrayCallbacks>,
    initial_active: bool,
    inner: Mutex<Option<TrayIcon>>,
}

impl SystemTrayApp {
    pub fn new(callbacks: TrayCallbacks, initial_active: bool, _lang: Option<String>) -> Self {
        Self {
            callbacks: Arc::new(callbacks),
            initial_active,
            inner: Mutex::new(None),
        }
    }

    pub fn start(&self) -> bool {
        let cbs = Arc::clone(&self.callbacks);
        let action_cb: MenuAction = Box::new(move |id| match id {
            ID_TOGGLE_ENGINE => {
                if let Some(ref f) = cbs.on_toggle_engine {
                    f();
                }
            }
            ID_OPEN_MAP => {
                if let Some(ref f) = cbs.on_open_map {
                    f();
                }
            }
            ID_OPEN_DIAG => {
                if let Some(ref f) = cbs.on_open_diagnostics {
                    f();
                }
            }
            ID_OPEN_CONFIG => {
                if let Some(ref f) = cbs.on_open_config_dir {
                    f();
                }
            }
            ID_LAUNCH_BORDERLESS => {
                if let Some(ref f) = cbs.on_launch_borderless {
                    f();
                }
            }
            ID_SUPPORT => {
                if let Some(ref f) = cbs.on_open_support {
                    f();
                }
            }
            ID_CHECK_UPDATE => {
                if let Some(ref f) = cbs.on_check_update {
                    f();
                }
            }
            ID_STOP_SESSION_REC => {
                if let Some(ref f) = cbs.on_stop_session_rec {
                    f();
                }
            }
            ID_QUIT => {
                if let Some(ref f) = cbs.on_quit {
                    f();
                }
            }
            _ => {}
        });
        if let Some(icon) = TrayIcon::new(action_cb) {
            icon.set_state(
                if self.initial_active {
                    TrayState::Idle
                } else {
                    TrayState::Paused
                },
                self.initial_active,
            );
            *self.inner.lock().unwrap() = Some(icon);
            true
        } else {
            false
        }
    }

    pub fn set_state(&self, state: TrayState, _tooltip: &str, engine_active: Option<bool>) {
        if let Some(ref icon) = *self.inner.lock().unwrap() {
            let active = engine_active.unwrap_or(state != TrayState::Paused);
            icon.set_state(state, active);
        }
    }

    pub fn set_state_str(&self, state: &str, tooltip: &str, engine_active: Option<bool>) {
        self.set_state(TrayState::from_str_lossy(state), tooltip, engine_active);
    }

    pub fn set_uipi_warning(&self, _mismatch: bool) {}
    pub fn set_available_update(&self, _version: Option<&str>) {}
    pub fn set_update_status(&self, _version: Option<&str>, _ready_to_restart: bool) {}
    pub fn set_session_recording(&self, active: bool) {
        if let Some(ref icon) = *self.inner.lock().unwrap() {
            icon.set_session_recording(active);
        }
    }
    pub fn set_language(&self, _lang: Option<&str>) {}

    pub fn close(&self) {
        if let Some(icon) = self.inner.lock().unwrap().take() {
            icon.close();
        }
    }
}
