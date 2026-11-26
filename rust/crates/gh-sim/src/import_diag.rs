//! `gh-sim import-diag`: imports an `Alt+F12` diagnostic bundle into `scratch/diagnostics/`
//! and registers its human-verified ground truth entry in git-tracked `tests/verified_gt.json`.

use crate::verified_gt::{DiagGtEntry, VerifiedGtFile};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ImportDiagArgs {
    pub source_dir: PathBuf,
    pub diagnostics_root: PathBuf,
    pub gt_file: PathBuf,
    pub expect_rejected: bool,
    pub expected_map_id: Option<i32>,
    pub ref_xy: Option<[f64; 2]>,
    pub note: String,
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct ImportDiagOutcome {
    pub bundle_name: String,
    pub dest_dir: PathBuf,
    pub files_copied: usize,
    pub gt_file: PathBuf,
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<usize, String> {
    std::fs::create_dir_all(dst)
        .map_err(|e| format!("failed to create {}: {e}", dst.display()))?;
    let mut count = 0usize;
    let entries = std::fs::read_dir(src)
        .map_err(|e| format!("failed to read {}: {e}", src.display()))?;
    for ent in entries {
        let ent = ent.map_err(|e| format!("dir entry error: {e}"))?;
        let sp = ent.path();
        let dp = dst.join(ent.file_name());
        if sp.is_dir() {
            count += copy_dir_recursive(&sp, &dp)?;
        } else if sp.is_file() {
            std::fs::copy(&sp, &dp)
                .map_err(|e| format!("failed to copy {} -> {}: {e}", sp.display(), dp.display()))?;
            count += 1;
        }
    }
    Ok(count)
}

pub fn import_diagnostic_bundle(args: &ImportDiagArgs) -> Result<ImportDiagOutcome, String> {
    if !args.source_dir.is_dir() {
        return Err(format!(
            "source diagnostic folder does not exist: {}",
            args.source_dir.display()
        ));
    }
    if !args.source_dir.join("03_report.json").is_file() {
        return Err(format!(
            "source folder is missing 03_report.json: {}",
            args.source_dir.display()
        ));
    }
    let bundle_name = args
        .source_dir
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| n.starts_with("diag_") && !n.contains('/') && !n.contains('\\'))
        .ok_or_else(|| {
            format!(
                "source folder name must start with 'diag_': {}",
                args.source_dir.display()
            )
        })?
        .to_owned();

    if !args.expect_rejected && args.expected_map_id.is_none() {
        return Err(
            "must specify either --expect-rejected or --map-id <ID> when importing verified GT"
                .to_owned(),
        );
    }

    let dest_dir = args.diagnostics_root.join(&bundle_name);
    let same_dir = std::fs::canonicalize(&args.source_dir)
        .ok()
        .zip(std::fs::canonicalize(&dest_dir).ok())
        .is_some_and(|(a, b)| a == b);

    let files_copied = if same_dir {
        0
    } else {
        if dest_dir.exists() && !args.force {
            return Err(format!(
                "destination already exists: {} (pass --force to overwrite)",
                dest_dir.display()
            ));
        }
        copy_dir_recursive(&args.source_dir, &dest_dir)?
    };

    let mut gt = VerifiedGtFile::load_or_default(&args.gt_file)?;
    if gt.diagnostics.contains_key(&bundle_name) && !args.force {
        return Err(format!(
            "GT entry '{bundle_name}' already exists in {} (pass --force to overwrite)",
            args.gt_file.display()
        ));
    }
    gt.diagnostics.insert(
        bundle_name.clone(),
        DiagGtEntry {
            expect_rejected: args.expect_rejected,
            expected_map_id: if args.expect_rejected {
                None
            } else {
                args.expected_map_id
            },
            ref_xy: if args.expect_rejected {
                None
            } else {
                args.ref_xy
            },
            note: args.note.clone(),
        },
    );
    gt.save_atomic(&args.gt_file)?;

    Ok(ImportDiagOutcome {
        bundle_name,
        dest_dir,
        files_copied,
        gt_file: args.gt_file.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_diag_copies_bundle_and_updates_verified_gt_atomically() {
        let root = std::env::temp_dir().join(format!(
            "gh_sim_import_diag_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("incoming").join("diag_20261126_120000");
        std::fs::create_dir_all(src.join("07_pre_lost_frames")).unwrap();
        std::fs::write(src.join("03_report.json"), r#"{"schema_version":2}"#).unwrap();
        std::fs::write(src.join("07_pre_lost_frames").join("0001.png"), b"stub").unwrap();

        let diag_root = root.join("scratch").join("diagnostics");
        let gt_file = root.join("tests").join("verified_gt.json");

        let out = import_diagnostic_bundle(&ImportDiagArgs {
            source_dir: src.clone(),
            diagnostics_root: diag_root.clone(),
            gt_file: gt_file.clone(),
            expect_rejected: false,
            expected_map_id: Some(2),
            ref_xy: Some([-123.5, 456.0]),
            note: "Regression case".to_owned(),
            force: false,
        })
        .unwrap();

        assert_eq!(out.bundle_name, "diag_20261126_120000");
        assert_eq!(out.files_copied, 2);
        assert!(diag_root.join("diag_20261126_120000").join("03_report.json").is_file());
        assert!(
            diag_root
                .join("diag_20261126_120000")
                .join("07_pre_lost_frames")
                .join("0001.png")
                .is_file()
        );

        let loaded = VerifiedGtFile::load(&gt_file).unwrap();
        let ent = &loaded.diagnostics["diag_20261126_120000"];
        assert!(!ent.expect_rejected);
        assert_eq!(ent.expected_map_id, Some(2));
        assert_eq!(ent.ref_xy, Some([-123.5, 456.0]));

        // Duplicate without --force must fail cleanly
        let dup = import_diagnostic_bundle(&ImportDiagArgs {
            source_dir: src,
            diagnostics_root: diag_root,
            gt_file,
            expect_rejected: true,
            expected_map_id: None,
            ref_xy: None,
            note: String::new(),
            force: false,
        });
        assert!(dup.is_err());

        let _ = std::fs::remove_dir_all(&root);
    }
}
