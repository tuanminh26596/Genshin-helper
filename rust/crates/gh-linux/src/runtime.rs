//! Linux runtime utilities: Unix-socket single-instance mutex and high-resolution timer.
//!
//! Mirrors `gh-win32::runtime`.

use std::net::{TcpListener, TcpStream};

pub const DESKTOP_SANDBOX_ENV: &str = "GH_DESKTOP_SANDBOX";

pub fn is_desktop_sandbox_env() -> bool {
    desktop_sandbox_flag(std::env::var(DESKTOP_SANDBOX_ENV).ok().as_deref())
}

pub fn desktop_sandbox_flag(raw: Option<&str>) -> bool {
    raw.map(|s| s.trim().to_ascii_lowercase())
        .is_some_and(|s| matches!(s.as_str(), "1" | "true" | "yes" | "on"))
}

pub fn resolve_mutex_name() -> String {
    let env_val = std::env::var("GH_SINGLE_INSTANCE_MUTEX").ok();
    resolve_mutex_name_from(env_val.as_deref())
}

pub fn resolve_mutex_name_from(env_override: Option<&str>) -> String {
    if let Some(raw) = env_override {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    gh_core::SINGLE_INSTANCE_MUTEX.to_owned()
}

// ---------------------------------------------------------------------------
// High-resolution timer (Linux already has nanosecond precision; no-op guard)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct HighResTimer {
    active: bool,
}

impl HighResTimer {
    pub const fn new() -> Self {
        Self { active: false }
    }

    pub const fn is_active(&self) -> bool {
        self.active
    }

    pub fn set(&mut self, on: bool) {
        // Linux CLOCK_MONOTONIC already has nanosecond resolution; no setup needed.
        self.active = on;
    }
}

impl Drop for HighResTimer {
    fn drop(&mut self) {
        self.set(false);
    }
}

// ---------------------------------------------------------------------------
// Single-instance mutex via TCP port binding
// ---------------------------------------------------------------------------

/// Ensures only one instance of the app runs by binding a local TCP port.
pub struct SingleInstanceMutex {
    _listener: Option<TcpListener>,
    already_running: bool,
}

impl std::fmt::Debug for SingleInstanceMutex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SingleInstanceMutex")
            .field("already_running", &self.already_running)
            .finish()
    }
}

impl SingleInstanceMutex {
    /// Port used for single-instance detection (not the main sync port).
    const MUTEX_PORT: u16 = 58_321;

    pub fn acquire_for_port(_port: u16) -> std::io::Result<Option<Self>> {
        let name = resolve_mutex_name();
        let m = Self::acquire(&name);
        if m.is_already_running() {
            Ok(None)
        } else {
            Ok(Some(m))
        }
    }

    pub fn acquire(_name: &str) -> Self {
        // Try to bind; if it fails, another instance is running
        match TcpListener::bind(format!("127.0.0.1:{}", Self::MUTEX_PORT)) {
            Ok(listener) => Self {
                _listener: Some(listener),
                already_running: false,
            },
            Err(_) => Self {
                _listener: None,
                already_running: true,
            },
        }
    }

    pub fn is_already_running(&self) -> bool {
        self.already_running
    }

    /// Wake an existing instance (send a message to its port).
    pub fn wake_existing(&self) -> bool {
        if !self.already_running {
            return false;
        }
        // Connect and send a wake signal
        if let Ok(mut stream) = TcpStream::connect(format!("127.0.0.1:{}", Self::MUTEX_PORT)) {
            use std::io::Write;
            let _ = stream.write_all(b"WAKE\n");
            return true;
        }
        false
    }
}
