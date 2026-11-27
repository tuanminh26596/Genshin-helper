use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    emit_build_stamp(&manifest_dir);

    if std::env::var_os("CARGO_CFG_TARGET_OS").as_deref() == Some(OsStr::new("windows")) {
        let mut res = winresource::WindowsResource::new();

        let icon_path = manifest_dir
            .join("..")
            .join("..")
            .join("..")
            .join("assets")
            .join("icons")
            .join("app.ico");
        if icon_path.is_file() {
            println!("cargo:rerun-if-changed={}", icon_path.display());
            res.set_icon(&icon_path.to_string_lossy());
        }

        res.set("ProductName", "Genshin Helper");
        res.set(
            "FileDescription",
            "Genshin Helper - Exploration & HoYoLAB Auto-Sync",
        );
        res.set("OriginalFilename", "GenshinHelper.exe");
        res.set("InternalName", "GenshinHelper");

        if let Err(err) = res.compile() {
            panic!("failed to compile Windows resource: {err}");
        }
    }
}

fn emit_build_stamp(manifest_dir: &Path) {
    let crate_version =
        std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=GH_APP_VERSION={crate_version}");

    let repo_root = find_git_repo_root(manifest_dir);
    if let Some(ref root) = repo_root {
        watch_git_head_and_ref(root);
    }

    let git_commit = repo_root
        .as_deref()
        .and_then(|root| {
            query_git_head_commit(root).or_else(|| read_git_head_commit_fallback(root))
        })
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=GH_GIT_COMMIT={git_commit}");
}

fn find_git_repo_root(start: &Path) -> Option<PathBuf> {
    for ancestor in start.ancestors() {
        let dot_git = ancestor.join(".git");
        if dot_git.is_dir() || dot_git.is_file() {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

fn resolve_git_dirs(repo_root: &Path) -> Option<(PathBuf, PathBuf)> {
    let dot_git = repo_root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else if dot_git.is_file() {
        let raw = std::fs::read_to_string(&dot_git).ok()?;
        let rest = raw.trim().strip_prefix("gitdir:")?;
        let target = PathBuf::from(rest.trim());
        if target.is_absolute() {
            target
        } else {
            repo_root.join(target)
        }
    } else {
        return None;
    };

    let commondir_file = git_dir.join("commondir");
    let common_dir = if commondir_file.is_file() {
        match std::fs::read_to_string(&commondir_file) {
            Ok(raw) => {
                let rel = PathBuf::from(raw.trim());
                if rel.is_absolute() {
                    rel
                } else {
                    git_dir.join(rel)
                }
            }
            Err(_) => git_dir.clone(),
        }
    } else {
        git_dir.clone()
    };

    Some((git_dir, common_dir))
}

/// Emits `cargo:rerun-if-changed` ONLY for existing git HEAD/ref files (supports both standard
/// `.git/` directories and git worktree `.git` pointer files) so incremental builds never rerun
/// unnecessarily when HEAD is unchanged.
fn watch_git_head_and_ref(repo_root: &Path) {
    let dot_git = repo_root.join(".git");
    if dot_git.is_file() {
        println!("cargo:rerun-if-changed={}", dot_git.display());
    }
    let Some((git_dir, common_dir)) = resolve_git_dirs(repo_root) else {
        return;
    };
    let commondir_file = git_dir.join("commondir");
    if commondir_file.is_file() {
        println!("cargo:rerun-if-changed={}", commondir_file.display());
    }

    let head_path = git_dir.join("HEAD");
    if !head_path.is_file() {
        return;
    }
    println!("cargo:rerun-if-changed={}", head_path.display());

    let Ok(head_raw) = std::fs::read_to_string(&head_path) else {
        return;
    };
    if let Some(ref_name) = head_raw.trim().strip_prefix("ref:") {
        let ref_rel = ref_name.trim();
        let ref_in_git = git_dir.join(ref_rel);
        let ref_in_common = common_dir.join(ref_rel);
        if ref_in_git.is_file() {
            println!("cargo:rerun-if-changed={}", ref_in_git.display());
        } else if ref_in_common.is_file() {
            println!("cargo:rerun-if-changed={}", ref_in_common.display());
        }
        let packed = common_dir.join("packed-refs");
        if packed.is_file() {
            println!("cargo:rerun-if-changed={}", packed.display());
        }
    }
}

fn normalize_hex_sha(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.len() == 40 && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(trimmed.to_ascii_lowercase())
    } else {
        None
    }
}

fn read_git_head_commit_fallback(repo_root: &Path) -> Option<String> {
    let (git_dir, common_dir) = resolve_git_dirs(repo_root)?;
    let head_raw = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let trimmed = head_raw.trim();
    if let Some(sha) = normalize_hex_sha(trimmed) {
        return Some(sha);
    }
    let ref_name = trimmed.strip_prefix("ref:")?.trim();
    for candidate in [git_dir.join(ref_name), common_dir.join(ref_name)] {
        if candidate.is_file()
            && let Ok(content) = std::fs::read_to_string(&candidate)
            && let Some(sha) = normalize_hex_sha(&content)
        {
            return Some(sha);
        }
    }
    let packed = std::fs::read_to_string(common_dir.join("packed-refs")).ok()?;
    for line in packed.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with('^') {
            continue;
        }
        let mut parts = t.split_whitespace();
        if let (Some(sha), Some(name)) = (parts.next(), parts.next())
            && name == ref_name
            && let Some(norm) = normalize_hex_sha(sha)
        {
            return Some(norm);
        }
    }
    None
}

fn query_git_head_commit(repo_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?;
    normalize_hex_sha(&sha)
}
