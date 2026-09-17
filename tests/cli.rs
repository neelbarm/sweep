//! End-to-end tests: build a miniature developer home, then drive the real
//! binary against it exactly the way a user would.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sweep")
}

fn write(path: &Path, bytes: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'x'; bytes]).unwrap();
}

/// A small but representative fixture: four real projects and four decoys.
fn fixture() -> TempDir {
    let td = TempDir::new().unwrap();
    let r = td.path();

    // Node project: node_modules + a gitignored dist.
    write(&r.join("web/package.json"), 20);
    write(&r.join("web/.gitignore"), 0);
    fs::write(r.join("web/.gitignore"), "node_modules\ndist\n").unwrap();
    write(&r.join("web/src/index.js"), 40);
    write(&r.join("web/node_modules/react/index.js"), 300_000);
    write(&r.join("web/dist/bundle.js"), 50_000);

    // Rust project.
    write(&r.join("cli/Cargo.toml"), 20);
    write(&r.join("cli/src/main.rs"), 30);
    write(&r.join("cli/target/debug/bin"), 900_000);

    // Python project.
    write(&r.join("py/requirements.txt"), 10);
    write(&r.join("py/.venv/pyvenv.cfg"), 30);
    write(&r.join("py/.venv/lib/pkg.py"), 120_000);
    write(&r.join("py/__pycache__/m.pyc"), 4_000);

    // Decoys: none of these may ever be reported.
    write(&r.join("decoy/target/export.psd"), 10_000); // no Cargo.toml
    write(&r.join("decoy/lib/package.json"), 10);
    write(&r.join("decoy/lib/dist/checked-in.js"), 10_000); // not gitignored
    write(&r.join("decoy/conf/env/prod.env"), 100); // no pyvenv.cfg
    write(&r.join("decoy/orphan/node_modules/x.js"), 10_000); // no package.json

    td
}

/// Run the binary, returning (stdout, stderr, success).
fn run(args: &[&str], history: &Path) -> (String, String, bool) {
    let out = Command::new(bin())
        .args(args)
        .env("SWEEP_HISTORY", history)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run sweep");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.success(),
    )
}

fn scan_json(root: &Path, extra: &[&str], history: &Path) -> Value {
    let mut args: Vec<&str> = vec!["scan", root.to_str().unwrap(), "--json"];
    args.extend_from_slice(extra);
    let (stdout, stderr, ok) = run(&args, history);
    assert!(ok, "sweep scan failed: {stderr}");
    serde_json::from_str(&stdout).expect("scan --json must emit valid JSON")
}

fn kinds(v: &Value) -> Vec<String> {
    v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["kind"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn scan_json_finds_every_artifact_and_no_decoys() {
    let td = fixture();
    let hist = td.path().join("history.jsonl");
    let v = scan_json(td.path(), &[], &hist);

    let mut ks = kinds(&v);
    ks.sort();
    assert_eq!(
        ks,
        vec![
            "cargo_target",
            "js_build",
            "node_modules",
            "pycache",
            "venv"
        ],
        "expected exactly the five real artifacts"
    );

    let paths: Vec<&str> = v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["path"].as_str().unwrap())
        .collect();
    for decoy in [
        "decoy/target",
        "decoy/lib/dist",
        "decoy/conf/env",
        "decoy/orphan",
    ] {
        assert!(
            !paths.iter().any(|p| p.contains(decoy)),
            "decoy {decoy} must not be reported, got {paths:?}"
        );
    }

    // Sizes are real, not guessed.
    let target = v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["kind"] == "cargo_target")
        .unwrap();
    assert_eq!(target["size_bytes"].as_u64().unwrap(), 900_000);
    assert_eq!(target["file_count"].as_u64().unwrap(), 1);

    assert_eq!(v["count"].as_u64().unwrap(), 5);
    assert!(v["total_bytes"].as_u64().unwrap() > 1_300_000);
    assert!(v["scanned_dirs"].as_u64().unwrap() > 0);
    assert!(!v["by_kind"].as_array().unwrap().is_empty());
}

#[test]
fn scan_filters_narrow_the_result() {
    let td = fixture();
    let hist = td.path().join("history.jsonl");

    let v = scan_json(td.path(), &["--kind", "cargo_target"], &hist);
    assert_eq!(kinds(&v), vec!["cargo_target"]);

    let v = scan_json(td.path(), &["--min-size", "500KB"], &hist);
    assert_eq!(
        kinds(&v),
        vec!["cargo_target"],
        "only the 900 KB target survives"
    );

    // Everything in the fixture was created seconds ago, so an age filter
    // should empty the list.
    let v = scan_json(td.path(), &["--older-than", "30d"], &hist);
    assert_eq!(v["count"].as_u64().unwrap(), 0);

    let v = scan_json(td.path(), &["--sort", "path"], &hist);
    let paths: Vec<&str> = v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["path"].as_str().unwrap())
        .collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted, "--sort path must be lexicographic");
}

#[test]
fn scan_rejects_bad_input() {
    let td = fixture();
    let hist = td.path().join("history.jsonl");
    let (_, stderr, ok) = run(
        &["scan", td.path().to_str().unwrap(), "--kind", "bogus"],
        &hist,
    );
    assert!(!ok);
    assert!(stderr.contains("unknown kind"), "{stderr}");

    let (_, stderr, ok) = run(
        &["scan", td.path().to_str().unwrap(), "--sort", "sideways"],
        &hist,
    );
    assert!(!ok);
    assert!(stderr.contains("unknown sort key"), "{stderr}");
}

#[test]
fn clean_dry_run_touches_nothing() {
    let td = fixture();
    let hist = td.path().join("history.jsonl");
    let before = snapshot(td.path());

    let (stdout, stderr, ok) = run(&["clean", td.path().to_str().unwrap()], &hist);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("DRY RUN"), "{stdout}");
    assert!(stdout.contains("Would"), "{stdout}");

    // Explicit --dry-run behaves identically.
    let (stdout2, _, ok2) = run(&["clean", td.path().to_str().unwrap(), "--dry-run"], &hist);
    assert!(ok2);
    assert!(stdout2.contains("DRY RUN"));

    assert_eq!(
        before,
        snapshot(td.path()),
        "a dry run must not change the tree"
    );
    assert!(!hist.exists(), "a dry run must not write to the audit log");
}

#[test]
fn clean_yes_permanent_removes_only_artifacts_and_audits_them() {
    let td = fixture();
    let hist = td.path().join("history.jsonl");

    let (stdout, stderr, ok) = run(
        &[
            "clean",
            td.path().to_str().unwrap(),
            "--kind",
            "cargo_target",
            "--yes",
            "--permanent",
            "--json",
        ],
        &hist,
    );
    assert!(ok, "{stderr}");
    let v: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["dry_run"], false);
    assert_eq!(v["permanent"], true);
    assert_eq!(v["count"].as_u64().unwrap(), 1);
    assert_eq!(v["reclaimed_bytes"].as_u64().unwrap(), 900_000);
    assert!(v["refused"].as_array().unwrap().is_empty());

    assert!(
        !td.path().join("cli/target").exists(),
        "the artifact is gone"
    );
    assert!(
        td.path().join("cli/Cargo.toml").is_file(),
        "the project survives"
    );
    assert!(
        td.path().join("cli/src/main.rs").is_file(),
        "sources survive"
    );
    assert!(
        td.path().join("web/node_modules").exists(),
        "unselected kinds are untouched"
    );

    let log = fs::read_to_string(&hist).unwrap();
    let line = log.lines().next().expect("one audit line");
    let entry: Value = serde_json::from_str(line).unwrap();
    assert_eq!(entry["kind"], "cargo_target");
    assert_eq!(entry["mode"], "permanent");
    assert_eq!(entry["ok"], true);
    assert_eq!(entry["size_bytes"].as_u64().unwrap(), 900_000);
}

#[test]
fn clean_never_leaves_the_given_roots() {
    let td = fixture();
    let outside = TempDir::new().unwrap();
    write(&outside.path().join("other/Cargo.toml"), 10);
    write(&outside.path().join("other/target/x"), 1000);
    let hist = td.path().join("history.jsonl");

    // Cleaning the fixture must not reach into an unrelated directory.
    let (_, stderr, ok) = run(
        &[
            "clean",
            td.path().to_str().unwrap(),
            "--kind",
            "cargo_target",
            "--yes",
            "--permanent",
        ],
        &hist,
    );
    assert!(ok, "{stderr}");
    assert!(
        outside.path().join("other/target").exists(),
        "a directory outside the roots must never be touched"
    );
}

/// A symlink *named* like an artifact must never be reported, and cleaning
/// around it must not reach through it into the real directory.
#[test]
#[cfg(unix)]
fn a_symlink_named_node_modules_is_never_followed_or_deleted_through() {
    let td = TempDir::new().unwrap();
    let r = td.path();
    let hist = r.join("history.jsonl");

    // Real source that the symlink points at.
    write(&r.join("realsrc/precious.js"), 120);
    // A project whose `node_modules` is a symlink to that source.
    write(&r.join("app/package.json"), 20);
    std::os::unix::fs::symlink(r.join("realsrc"), r.join("app/node_modules")).unwrap();

    let v = scan_json(r, &[], &hist);
    assert_eq!(
        v["count"].as_u64().unwrap(),
        0,
        "a symlinked node_modules must not be reported, got {:?}",
        kinds(&v)
    );

    let (_, stderr, ok) = run(
        &["clean", r.to_str().unwrap(), "--yes", "--permanent"],
        &hist,
    );
    assert!(ok, "{stderr}");
    assert!(
        r.join("realsrc/precious.js").is_file(),
        "the real directory behind the symlink must survive"
    );
    assert!(
        fs::symlink_metadata(r.join("app/node_modules"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the symlink itself must be left alone"
    );
}

/// A symlink *inside* an artifact that escapes it must not be measured and
/// must not be followed when the artifact is removed.
#[test]
#[cfg(unix)]
fn a_symlink_inside_an_artifact_is_not_followed_out_of_it() {
    let td = TempDir::new().unwrap();
    let r = td.path();
    let hist = r.join("history.jsonl");

    write(&r.join("src/keep.rs"), 4_000);
    write(&r.join("app/package.json"), 20);
    write(&r.join("app/node_modules/pkg/blob.bin"), 50_000);
    std::os::unix::fs::symlink(r.join("src"), r.join("app/node_modules/escape")).unwrap();

    let v = scan_json(r, &[], &hist);
    let nm = v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["kind"] == "node_modules")
        .expect("the node_modules is found");
    assert_eq!(
        nm["size_bytes"].as_u64().unwrap(),
        50_000,
        "the escaping symlink's target must not be counted"
    );
    assert_eq!(nm["file_count"].as_u64().unwrap(), 1);

    let (_, stderr, ok) = run(
        &["clean", r.to_str().unwrap(), "--yes", "--permanent"],
        &hist,
    );
    assert!(ok, "{stderr}");
    assert!(!r.join("app/node_modules").exists(), "the artifact is gone");
    assert!(
        r.join("src/keep.rs").is_file(),
        "deletion must remove the link, never what it points at"
    );
}

/// The fence is re-evaluated at deletion time: a `Hit` from a scan whose
/// marker has since vanished must be refused, and the refusal audited.
#[test]
fn a_marker_that_vanished_after_the_scan_refuses_the_delete() {
    let td = TempDir::new().unwrap();
    let r = td.path();
    let hist = r.join("history.jsonl");
    std::env::set_var("SWEEP_HISTORY", &hist);

    write(&r.join("cli/Cargo.toml"), 20);
    write(&r.join("cli/target/debug/bin"), 900_000);

    let res = sweep::scan::scan(
        &sweep::scan::ScanOptions {
            roots: vec![r.to_path_buf()],
            max_depth: None,
            include_derived_data: false,
        },
        None,
    );
    let hit = res
        .hits
        .iter()
        .find(|h| h.kind == "cargo_target")
        .expect("the target is found")
        .clone();

    // The window between scan and keypress: the manifest goes away.
    fs::remove_file(r.join("cli/Cargo.toml")).unwrap();

    let outcome = sweep::remove::delete_hit(&hit, &res.roots, true);
    assert!(!outcome.is_ok(), "{}", outcome.describe());
    assert!(
        outcome.describe().contains("marker is gone"),
        "{}",
        outcome.describe()
    );
    assert!(
        r.join("cli/target").is_dir(),
        "a target with no manifest must survive"
    );

    let log = fs::read_to_string(&hist).unwrap();
    assert!(
        log.contains("\"ok\":false"),
        "the refusal is audited: {log}"
    );
}

#[test]
fn rules_command_documents_every_marker() {
    let td = TempDir::new().unwrap();
    let (stdout, _, ok) = run(&["rules"], &td.path().join("h.jsonl"));
    assert!(ok);
    for kind in [
        "node_modules",
        "cargo_target",
        "venv",
        "derived_data",
        "terraform",
    ] {
        assert!(stdout.contains(kind), "rules output must mention {kind}");
    }
    assert!(stdout.contains("sibling package.json"));
    assert!(stdout.contains("contains pyvenv.cfg"));
}

#[test]
fn human_table_output_is_aligned_and_totalled() {
    let td = fixture();
    let hist = td.path().join("history.jsonl");
    let (stdout, _, ok) = run(&["scan", td.path().to_str().unwrap(), "--all"], &hist);
    assert!(ok);
    assert!(stdout.contains("KIND"));
    assert!(stdout.contains("LAST USED"));
    assert!(stdout.contains("Reclaimable"));
    assert!(stdout.contains("node_modules"));
    // NO_COLOR is set, so no escape sequences may leak into the output.
    assert!(
        !stdout.contains('\u{1b}'),
        "NO_COLOR must suppress ANSI codes"
    );
}

/// A stable fingerprint of every path under `root`.
fn snapshot(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            out.push(e.path());
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                walk(&e.path(), out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}
