//! Runtime path resolution replacing PyInstaller's `sys._MEIPASS` (`src/paths.py`).

use std::path::{Path, PathBuf};

pub const GLOBAL_INDEX_FILENAME: &str = "teyvat_global_index_v4.npz";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeMode {
    /// Installed layout: `%APPDATA%\GenshinHelper` & `%LOCALAPPDATA%\GenshinHelper`.
    Installed,
    /// Developer workspace layout: repo root `config.json`, `scratch/diagnostics/`, `data/`.
    Dev,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub mode: RuntimeMode,
    pub config_path: PathBuf,
    pub diagnostics_dir: PathBuf,
    pub data_dir: PathBuf,
    pub tiles_dir: PathBuf,
    pub ug_overlays_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub dev_data_dir: Option<PathBuf>,
}

/// Captured process environment inputs so `AppPaths::resolve_with` is pure and deterministic
/// (avoids `unsafe std::env::set_var` in Rust 2024 unit tests).
#[derive(Debug, Clone, Default)]
pub struct PathEnv {
    pub appdata: Option<PathBuf>,
    pub localappdata: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub dev_root_env: Option<PathBuf>,
    pub data_dir_env: Option<PathBuf>,
    pub cargo_manifest_dir: Option<PathBuf>,
    pub exe_path: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
}

impl PathEnv {
    pub fn from_process() -> Self {
        let env_path = |key: &str| -> Option<PathBuf> {
            std::env::var_os(key).and_then(|val| {
                let s = val.to_string_lossy();
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(trimmed))
                }
            })
        };

        let home = env_path("USERPROFILE").or_else(|| env_path("HOME"));
        Self {
            appdata: env_path("APPDATA"),
            localappdata: env_path("LOCALAPPDATA"),
            xdg_data_home: env_path("XDG_DATA_HOME"),
            home,
            dev_root_env: env_path("GENSHIN_HELPER_DEV_ROOT"),
            data_dir_env: env_path("GENSHIN_HELPER_DATA_DIR"),
            cargo_manifest_dir: env_path("CARGO_MANIFEST_DIR"),
            exe_path: std::env::current_exe().ok(),
            cwd: std::env::current_dir().ok(),
        }
    }
}

pub fn resolve_paths() -> AppPaths {
    AppPaths::resolve()
}

impl AppPaths {
    pub fn resolve() -> Self {
        Self::resolve_with(&PathEnv::from_process())
    }

    pub fn resolve_with(env: &PathEnv) -> Self {
        let dev_root = detect_dev_root(env);

        #[cfg(windows)]
        let (user_app_roaming, user_app_local) = {
            let roaming_base = non_empty_path(env.appdata.as_ref())
                .or_else(|| {
                    non_empty_path(env.home.as_ref())
                        .map(|h| h.join("AppData").join("Roaming"))
                })
                .unwrap_or_else(|| PathBuf::from("."));
            let local_base = non_empty_path(env.localappdata.as_ref())
                .or_else(|| non_empty_path(env.appdata.as_ref()))
                .or_else(|| {
                    non_empty_path(env.home.as_ref())
                        .map(|h| h.join("AppData").join("Local"))
                })
                .unwrap_or_else(|| PathBuf::from("."));
            (
                roaming_base.join("GenshinHelper"),
                local_base.join("GenshinHelper"),
            )
        };

        #[cfg(not(windows))]
        let (user_app_roaming, user_app_local) = {
            let xdg_base = non_empty_path(env.xdg_data_home.as_ref())
                .or_else(|| {
                    non_empty_path(env.home.as_ref()).map(|h| h.join(".local").join("share"))
                })
                .unwrap_or_else(|| PathBuf::from("."));
            let app = xdg_base.join("genshin-helper");
            (app.clone(), app)
        };

        let data_dir =
            non_empty_path(env.data_dir_env.as_ref()).unwrap_or_else(|| user_app_local.join("data"));
        let tiles_dir = user_app_local.join("tiles");
        let ug_overlays_dir = user_app_local.join("ug_overlays");
        let cache_dir = user_app_local.join("cache");

        match dev_root {
            Some(root) => Self {
                mode: RuntimeMode::Dev,
                config_path: root.join("config.json"),
                diagnostics_dir: root.join("scratch").join("diagnostics"),
                data_dir,
                tiles_dir,
                ug_overlays_dir,
                cache_dir,
                dev_data_dir: Some(root.join("data")),
            },
            None => Self {
                mode: RuntimeMode::Installed,
                config_path: user_app_roaming.join("config.json"),
                diagnostics_dir: user_app_roaming.join("diagnostics"),
                data_dir,
                tiles_dir,
                ug_overlays_dir,
                cache_dir,
                dev_data_dir: None,
            },
        }
    }

    pub fn config_dir(&self) -> &Path {
        self.config_path.parent().unwrap_or_else(|| Path::new("."))
    }

    pub fn dev_root(&self) -> Option<&Path> {
        match self.mode {
            RuntimeMode::Dev => self.config_path.parent(),
            RuntimeMode::Installed => None,
        }
    }

    pub fn ocr_cache_dir(&self) -> PathBuf {
        match self.mode {
            RuntimeMode::Dev => self
                .dev_root()
                .unwrap_or_else(|| self.config_dir())
                .join("scratch"),
            RuntimeMode::Installed => self
                .cache_dir
                .parent()
                .unwrap_or_else(|| self.config_dir())
                .join("ocr_cache"),
        }
    }

    pub fn minimap_rec_dir(&self) -> PathBuf {
        self.diagnostics_dir
            .parent()
            .unwrap_or(&self.diagnostics_dir)
            .join("minimap_rec")
    }

    pub fn session_rec_dir(&self) -> PathBuf {
        self.diagnostics_dir
            .parent()
            .unwrap_or(&self.diagnostics_dir)
            .join("session_rec")
    }

    pub fn external_hotkey_backups_path(&self) -> PathBuf {
        self.config_dir().join("external_hotkey_backups.json")
    }

    pub fn paimon_cache_path(&self) -> PathBuf {
        self.cache_dir.join("paimon_tpl.npz")
    }

    pub fn global_index_path(&self) -> PathBuf {
        self.cache_dir.join(GLOBAL_INDEX_FILENAME)
    }

    pub fn routes_dir(&self) -> PathBuf {
        self.cache_dir
            .parent()
            .unwrap_or(&self.cache_dir)
            .join("routes")
    }

    /// Directory for staged, SHA-256-verified app update installers (`%LOCALAPPDATA%\GenshinHelper\updates`).
    pub fn updates_dir(&self) -> PathBuf {
        self.cache_dir
            .parent()
            .unwrap_or(&self.cache_dir)
            .join("updates")
    }

    pub fn resolve_data_asset(&self, filename: &str) -> PathBuf {
        if let Some(dev_data) = &self.dev_data_dir {
            let candidate = dev_data.join(filename);
            if candidate.is_file() {
                return candidate;
            }
        }
        let cached = self.data_dir.join(filename);
        if cached.is_file() {
            return cached;
        }
        if let Some(dev_data) = &self.dev_data_dir {
            return dev_data.join(filename);
        }
        cached
    }
}

fn non_empty_path(path: Option<&PathBuf>) -> Option<PathBuf> {
    path.and_then(|p| {
        let s = p.to_string_lossy();
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    })
}

fn has_component_ci(path: &Path, target: &str) -> bool {
    path.components().any(|c| {
        c.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(target)
    })
}

fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let base = if start.is_file() {
        start.parent()?
    } else {
        start
    };
    for ancestor in base.ancestors() {
        // Require both `extension/manifest.json` and `rust/Cargo.toml` so an installed Inno Setup
        // folder (`C:\Program Files\Genshin Helper\extension\manifest.json`) is never misclassified
        // as a Dev workspace.
        if ancestor.join("extension").join("manifest.json").is_file()
            && ancestor.join("rust").join("Cargo.toml").is_file()
        {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

fn detect_dev_root(env: &PathEnv) -> Option<PathBuf> {
    if let Some(explicit) = non_empty_path(env.dev_root_env.as_ref()) {
        return Some(explicit);
    }

    if env
        .exe_path
        .as_ref()
        .is_some_and(|p| has_component_ci(p, "dist"))
    {
        return None;
    }

    if let Some(exe) = non_empty_path(env.exe_path.as_ref())
        && let Some(root) = find_repo_root(&exe)
    {
        return Some(root);
    }

    let is_cargo_built = non_empty_path(env.cargo_manifest_dir.as_ref()).is_some()
        || env.exe_path.as_ref().is_some_and(|p| {
            has_component_ci(p, "target")
                || has_component_ci(p, "CargoTarget")
                || has_component_ci(p, "deps")
        });

    if is_cargo_built {
        if let Some(manifest_dir) = non_empty_path(env.cargo_manifest_dir.as_ref())
            && let Some(root) = find_repo_root(&manifest_dir)
        {
            return Some(root);
        }
        if let Some(cwd) = non_empty_path(env.cwd.as_ref())
            && let Some(root) = find_repo_root(&cwd)
        {
            return Some(root);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn make_temp_dir(label: &str) -> PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "gh_core_paths_{label}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp test dir");
        dir
    }

    #[test]
    fn installed_mode_uses_roaming_and_local_appdata() {
        let env = PathEnv {
            appdata: Some(PathBuf::from(r"X:\Roaming")),
            localappdata: Some(PathBuf::from(r"X:\Local")),
            home: Some(PathBuf::from(r"X:\Home")),
            exe_path: Some(PathBuf::from(r"X:\ProgramFiles\GenshinHelper\GenshinHelper.exe")),
            ..PathEnv::default()
        };

        let paths = AppPaths::resolve_with(&env);
        assert_eq!(paths.mode, RuntimeMode::Installed);
        assert_eq!(paths.dev_root(), None);
        assert_eq!(paths.dev_data_dir, None);

        #[cfg(windows)]
        {
            assert_eq!(
                paths.config_path,
                PathBuf::from(r"X:\Roaming\GenshinHelper\config.json")
            );
            assert_eq!(
                paths.diagnostics_dir,
                PathBuf::from(r"X:\Roaming\GenshinHelper\diagnostics")
            );
            assert_eq!(
                paths.external_hotkey_backups_path(),
                PathBuf::from(r"X:\Roaming\GenshinHelper\external_hotkey_backups.json")
            );
            assert_eq!(
                paths.data_dir,
                PathBuf::from(r"X:\Local\GenshinHelper\data")
            );
            assert_eq!(
                paths.tiles_dir,
                PathBuf::from(r"X:\Local\GenshinHelper\tiles")
            );
            assert_eq!(
                paths.ug_overlays_dir,
                PathBuf::from(r"X:\Local\GenshinHelper\ug_overlays")
            );
            assert_eq!(
                paths.cache_dir,
                PathBuf::from(r"X:\Local\GenshinHelper\cache")
            );
            assert_eq!(
                paths.ocr_cache_dir(),
                PathBuf::from(r"X:\Local\GenshinHelper\ocr_cache")
            );
            assert_eq!(
                paths.paimon_cache_path(),
                PathBuf::from(r"X:\Local\GenshinHelper\cache\paimon_tpl.npz")
            );
            assert_eq!(
                paths.global_index_path(),
                PathBuf::from(r"X:\Local\GenshinHelper\cache\teyvat_global_index_v4.npz")
            );
        }
    }

    #[test]
    fn dev_mode_via_env_override() {
        let env = PathEnv {
            appdata: Some(PathBuf::from(r"X:\Roaming")),
            localappdata: Some(PathBuf::from(r"X:\Local")),
            dev_root_env: Some(PathBuf::from(r"X:\FakeRepo")),
            exe_path: Some(PathBuf::from(r"X:\ProgramFiles\App.exe")),
            ..PathEnv::default()
        };

        let repo = PathBuf::from(r"X:\FakeRepo");
        let paths = AppPaths::resolve_with(&env);
        assert_eq!(paths.mode, RuntimeMode::Dev);
        assert_eq!(paths.dev_root(), Some(repo.as_path()));
        assert_eq!(paths.config_path, repo.join("config.json"));
        assert_eq!(
            paths.diagnostics_dir,
            repo.join("scratch").join("diagnostics")
        );
        assert_eq!(paths.ocr_cache_dir(), repo.join("scratch"));
        assert_eq!(
            paths.minimap_rec_dir(),
            repo.join("scratch").join("minimap_rec")
        );
        assert_eq!(
            paths.external_hotkey_backups_path(),
            repo.join("external_hotkey_backups.json")
        );
        assert_eq!(paths.dev_data_dir, Some(repo.join("data")));
    }

    #[test]
    fn dist_exe_inside_repo_resolves_as_installed_not_dev() {
        let repo = make_temp_dir("dist_exclusion");
        std::fs::create_dir_all(repo.join("extension")).expect("ext dir");
        std::fs::create_dir_all(repo.join("rust")).expect("rust dir");
        std::fs::write(repo.join("extension").join("manifest.json"), "{}").expect("manifest");
        std::fs::write(repo.join("rust").join("Cargo.toml"), "[workspace]").expect("cargo");

        let env = PathEnv {
            appdata: Some(PathBuf::from(r"X:\Roaming")),
            localappdata: Some(PathBuf::from(r"X:\Local")),
            exe_path: Some(repo.join("dist").join("GenshinHelper").join("GenshinHelper.exe")),
            cwd: Some(repo.clone()),
            ..PathEnv::default()
        };

        let paths = AppPaths::resolve_with(&env);
        assert_eq!(paths.mode, RuntimeMode::Installed);
        assert_eq!(paths.dev_root(), None);

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn installed_dir_with_bundled_extension_stays_installed_mode() {
        let install_dir = make_temp_dir("program_files_install");
        std::fs::create_dir_all(install_dir.join("extension")).expect("ext dir");
        std::fs::write(install_dir.join("extension").join("manifest.json"), "{}")
            .expect("bundled manifest");
        let exe_path = install_dir.join("GenshinHelper.exe");
        std::fs::write(&exe_path, b"exe").expect("exe");

        let env = PathEnv {
            appdata: Some(PathBuf::from(r"X:\Roaming")),
            localappdata: Some(PathBuf::from(r"X:\Local")),
            exe_path: Some(exe_path),
            cwd: Some(install_dir.clone()),
            ..PathEnv::default()
        };

        let paths = AppPaths::resolve_with(&env);
        assert_eq!(paths.mode, RuntimeMode::Installed);
        assert_eq!(paths.dev_root(), None);
        #[cfg(windows)]
        assert_eq!(
            paths.config_path,
            PathBuf::from(r"X:\Roaming\GenshinHelper\config.json")
        );

        let _ = std::fs::remove_dir_all(&install_dir);
    }

    #[test]
    fn cargo_target_exe_falls_back_to_manifest_or_cwd_repo_root() {
        let repo = make_temp_dir("cargo_target");
        std::fs::create_dir_all(repo.join("extension")).expect("ext dir");
        std::fs::create_dir_all(repo.join("rust")).expect("rust dir");
        std::fs::write(repo.join("extension").join("manifest.json"), "{}").expect("manifest");
        std::fs::write(repo.join("rust").join("Cargo.toml"), "[workspace]").expect("cargo");

        let env = PathEnv {
            appdata: Some(PathBuf::from(r"X:\Roaming")),
            localappdata: Some(PathBuf::from(r"X:\Local")),
            exe_path: Some(
                PathBuf::from("CargoTarget")
                    .join("Genshin-Helper")
                    .join("debug")
                    .join("deps")
                    .join("gh_core"),
            ),
            cwd: Some(repo.clone()),
            ..PathEnv::default()
        };

        let paths = AppPaths::resolve_with(&env);
        assert_eq!(paths.mode, RuntimeMode::Dev);
        assert_eq!(paths.dev_root(), Some(repo.as_path()));

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn data_dir_env_override_and_resolve_data_asset_preference() {
        let base = make_temp_dir("data_asset");
        let dev_root = base.join("repo");
        let custom_data = base.join("custom_data");
        std::fs::create_dir_all(dev_root.join("data")).expect("dev data");
        std::fs::create_dir_all(&custom_data).expect("custom data");

        std::fs::write(dev_root.join("data").join("multimap_v4.json"), "{}").expect("dev mm");
        std::fs::write(custom_data.join("multimap_v4.json"), "{}").expect("cache mm");
        std::fs::write(custom_data.join("terrain_n2_v4.npz"), "zip").expect("cache terrain");

        let env = PathEnv {
            appdata: Some(base.join("roaming")),
            localappdata: Some(base.join("local")),
            xdg_data_home: Some(base.join("local")),
            dev_root_env: Some(dev_root.clone()),
            data_dir_env: Some(custom_data.clone()),
            ..PathEnv::default()
        };

        let paths = AppPaths::resolve_with(&env);
        assert_eq!(paths.data_dir, custom_data);
        #[cfg(windows)]
        assert_eq!(
            paths.cache_dir,
            base.join("local").join("GenshinHelper").join("cache")
        );
        #[cfg(not(windows))]
        assert_eq!(
            paths.cache_dir,
            base.join("local").join("genshin-helper").join("cache")
        );
        // Prefers dev_data_dir when file exists there
        assert_eq!(
            paths.resolve_data_asset("multimap_v4.json"),
            dev_root.join("data").join("multimap_v4.json")
        );
        // Falls back to data_dir when absent from dev_data_dir
        assert_eq!(
            paths.resolve_data_asset("terrain_n2_v4.npz"),
            custom_data.join("terrain_n2_v4.npz")
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
