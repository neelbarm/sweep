//! Deletion, with the safety fence and the audit log.
//!
//! Every removal has to pass the same five checks, whether it was triggered
//! from the TUI or from `sweep clean --yes`. The checks re-validate the path
//! at the moment of deletion rather than trusting the scan result, because a
//! scan of a large home directory can be minutes old by the time a human hits
//! the key.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::rules;
use crate::scan::Hit;
use crate::util;

/// Why a deletion was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    OutsideRoots,
    DangerousPath,
    NotADirectory,
    IsSymlink,
    MarkerGone,
    Missing,
}

impl Refusal {
    pub fn message(&self) -> &'static str {
        match self {
            Refusal::OutsideRoots => "path is outside the scanned roots",
            Refusal::DangerousPath => "refusing to touch a filesystem or home root",
            Refusal::NotADirectory => "path is not a directory",
            Refusal::IsSymlink => "path is a symlink",
            Refusal::MarkerGone => {
                "the project marker is gone; this no longer looks like a build artifact"
            }
            Refusal::Missing => "path no longer exists",
        }
    }
}

/// The result of attempting one deletion.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Moved to the system Trash, recoverable.
    Trashed(u64),
    /// Permanently removed.
    Removed(u64),
    /// Blocked by a safety check.
    Refused(Refusal),
    /// The OS said no.
    Failed(String),
}

impl Outcome {
    pub fn bytes(&self) -> u64 {
        match self {
            Outcome::Trashed(b) | Outcome::Removed(b) => *b,
            _ => 0,
        }
    }
    pub fn is_ok(&self) -> bool {
        matches!(self, Outcome::Trashed(_) | Outcome::Removed(_))
    }
    pub fn describe(&self) -> String {
        match self {
            Outcome::Trashed(b) => format!("trashed {}", util::human_bytes(*b)),
            Outcome::Removed(b) => format!("deleted {}", util::human_bytes(*b)),
            Outcome::Refused(r) => format!("refused: {}", r.message()),
            Outcome::Failed(e) => format!("failed: {e}"),
        }
    }
}

/// One line of `~/.sweep/history.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub ts: i64,
    pub path: PathBuf,
    pub kind: String,
    pub size_bytes: u64,
    pub file_count: u64,
    /// `trash` or `permanent`.
    pub mode: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Paths that are never deletable no matter what the scan says.
fn is_dangerous(path: &Path) -> bool {
    if path.parent().is_none() {
        return true; // "/"
    }
    if let Some(home) = dirs::home_dir() {
        if path == home {
            return true;
        }
        // Direct children of home that are obviously not build output.
        if path.parent() == Some(home.as_path()) {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if matches!(
                    name,
                    "Desktop"
                        | "Documents"
                        | "Downloads"
                        | "Library"
                        | "Movies"
                        | "Music"
                        | "Pictures"
                        | "Public"
                        | "Applications"
                ) {
                    return true;
                }
            }
        }
    }
    let depth = path.components().count();
    depth <= 2 // "/Users", "/Applications", ...
}

/// Run the safety fence for one path. `roots` are the roots the scan walked.
pub fn check(path: &Path, roots: &[PathBuf]) -> Result<(), Refusal> {
    if is_dangerous(path) {
        return Err(Refusal::DangerousPath);
    }
    if !roots.iter().any(|r| path.starts_with(r) && path != r) {
        return Err(Refusal::OutsideRoots);
    }
    let md = match fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(_) => return Err(Refusal::Missing),
    };
    if md.file_type().is_symlink() {
        return Err(Refusal::IsSymlink);
    }
    if !md.is_dir() {
        return Err(Refusal::NotADirectory);
    }
    if rules::match_dir(path).is_none() {
        return Err(Refusal::MarkerGone);
    }
    Ok(())
}

/// Delete one artifact. Trash by default; `permanent` uses `remove_dir_all`.
/// Always appends to the history log, including refusals and failures.
pub fn delete_hit(hit: &Hit, roots: &[PathBuf], permanent: bool) -> Outcome {
    let outcome = match check(&hit.path, roots) {
        Err(r) => Outcome::Refused(r),
        Ok(()) => {
            if permanent {
                match fs::remove_dir_all(&hit.path) {
                    Ok(()) => Outcome::Removed(hit.size_bytes),
                    Err(e) => Outcome::Failed(e.to_string()),
                }
            } else {
                match trash::delete(&hit.path) {
                    Ok(()) => Outcome::Trashed(hit.size_bytes),
                    Err(e) => Outcome::Failed(e.to_string()),
                }
            }
        }
    };
    let _ = log_history(hit, permanent, &outcome);
    outcome
}

/// `~/.sweep/history.jsonl`
pub fn history_path() -> Option<PathBuf> {
    // `SWEEP_HISTORY` redirects the audit log; the test-suite uses it so a
    // `cargo test` run never writes into the developer's real home.
    if let Some(p) = std::env::var_os("SWEEP_HISTORY") {
        return Some(PathBuf::from(p));
    }
    dirs::home_dir().map(|h| h.join(".sweep/history.jsonl"))
}

fn log_history(hit: &Hit, permanent: bool, outcome: &Outcome) -> std::io::Result<()> {
    let Some(path) = history_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let entry = HistoryEntry {
        ts: util::now_secs(),
        path: hit.path.clone(),
        kind: hit.kind.clone(),
        size_bytes: hit.size_bytes,
        file_count: hit.file_count,
        mode: if permanent { "permanent" } else { "trash" }.to_string(),
        ok: outcome.is_ok(),
        error: match outcome {
            Outcome::Refused(r) => Some(r.message().to_string()),
            Outcome::Failed(e) => Some(e.clone()),
            _ => None,
        },
    };
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{}", serde_json::to_string(&entry)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn hit_at(path: &Path) -> Hit {
        Hit {
            path: path.to_path_buf(),
            kind: "node_modules".into(),
            size_bytes: 10,
            file_count: 1,
            last_used: None,
            project_root: path.parent().unwrap().to_path_buf(),
        }
    }

    fn project(td: &TempDir) -> PathBuf {
        let app = td.path().join("app");
        fs::create_dir_all(app.join("node_modules")).unwrap();
        fs::write(app.join("package.json"), "{}").unwrap();
        fs::write(app.join("node_modules/x.js"), "x").unwrap();
        app
    }

    #[test]
    fn valid_artifact_passes_the_fence() {
        let td = TempDir::new().unwrap();
        let app = project(&td);
        let roots = vec![td.path().to_path_buf()];
        assert!(check(&app.join("node_modules"), &roots).is_ok());
    }

    #[test]
    fn refuses_paths_outside_the_scanned_roots() {
        let td = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let app = project(&td);
        let roots = vec![other.path().to_path_buf()];
        assert_eq!(
            check(&app.join("node_modules"), &roots),
            Err(Refusal::OutsideRoots)
        );
    }

    #[test]
    fn refuses_when_the_marker_is_gone() {
        let td = TempDir::new().unwrap();
        let app = project(&td);
        fs::remove_file(app.join("package.json")).unwrap();
        let roots = vec![td.path().to_path_buf()];
        assert_eq!(
            check(&app.join("node_modules"), &roots),
            Err(Refusal::MarkerGone)
        );
    }

    #[test]
    fn refuses_symlinks_and_missing_paths() {
        let td = TempDir::new().unwrap();
        let app = project(&td);
        let roots = vec![td.path().to_path_buf()];
        let link = app.join("link_modules");
        #[cfg(unix)]
        std::os::unix::fs::symlink(app.join("node_modules"), &link).unwrap();
        assert_eq!(check(&link, &roots), Err(Refusal::IsSymlink));
        assert_eq!(check(&app.join("nope"), &roots), Err(Refusal::Missing));
    }

    #[test]
    fn refuses_filesystem_and_home_roots() {
        let roots = vec![PathBuf::from("/")];
        assert_eq!(check(Path::new("/"), &roots), Err(Refusal::DangerousPath));
        assert_eq!(
            check(Path::new("/Users"), &roots),
            Err(Refusal::DangerousPath)
        );
        if let Some(home) = dirs::home_dir() {
            assert_eq!(check(&home, &roots), Err(Refusal::DangerousPath));
            assert_eq!(
                check(&home.join("Documents"), &roots),
                Err(Refusal::DangerousPath)
            );
        }
    }

    /// Point the audit log at one scratch file shared by the whole test binary.
    /// Done once, because the environment is process-wide and tests run in
    /// parallel.
    fn temp_history() -> PathBuf {
        static INIT: std::sync::Once = std::sync::Once::new();
        let p = std::env::temp_dir().join("sweep-test-history.jsonl");
        INIT.call_once(|| {
            let _ = fs::remove_file(&p);
            std::env::set_var("SWEEP_HISTORY", &p);
        });
        p
    }

    #[test]
    fn permanent_delete_removes_only_the_artifact() {
        let log_path = temp_history();
        let td = TempDir::new().unwrap();
        let app = project(&td);
        let roots = vec![td.path().to_path_buf()];
        let hit = hit_at(&app.join("node_modules"));
        let outcome = delete_hit(&hit, &roots, true);
        assert!(outcome.is_ok(), "{}", outcome.describe());
        assert!(!app.join("node_modules").exists());
        assert!(
            app.join("package.json").exists(),
            "the project itself survives"
        );
        let log = fs::read_to_string(&log_path).unwrap();
        let line = log
            .lines()
            .find(|l| l.contains(app.join("node_modules").to_str().unwrap()))
            .expect("the deletion is audited");
        assert!(line.contains("\"mode\":\"permanent\""), "{line}");
        assert!(line.contains("\"ok\":true"), "{line}");
    }

    #[test]
    fn refusals_do_not_delete() {
        temp_history();
        let td = TempDir::new().unwrap();
        let app = project(&td);
        let hit = hit_at(&app.join("node_modules"));
        let outcome = delete_hit(&hit, &[PathBuf::from("/nowhere-at-all")], true);
        assert!(!outcome.is_ok());
        assert!(app.join("node_modules").exists());
    }
}
