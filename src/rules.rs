//! Artifact detection rules.
//!
//! The whole safety story of `sweep` starts here: a directory is only ever
//! considered a reclaimable build artifact when its *name* matches a known
//! artifact name **and** a project marker proves it was produced by a build
//! tool. `node_modules` on its own means nothing; `node_modules` next to a
//! `package.json` is npm's output and can always be regenerated.

use std::path::Path;

/// How a candidate directory proves it is really a build artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// At least one of these files exists *next to* the candidate directory.
    Sibling(&'static [&'static str]),
    /// At least one file with this extension exists next to the candidate.
    SiblingExt(&'static str),
    /// A sibling marker file exists *and* the directory is gitignored.
    /// Used for names like `dist`, `build` and `vendor`, which are sometimes
    /// hand-written source. If the repo ignores it, the repo says it is output.
    SiblingIgnored(&'static [&'static str]),
    /// This file exists *inside* the candidate directory.
    Contains(&'static str),
    /// The name alone is unambiguous (e.g. `__pycache__`).
    NameOnly,
    /// Direct child of `~/Library/Developer/Xcode/DerivedData`.
    XcodeDerivedData,
}

/// A single detection rule.
#[derive(Debug, Clone, Copy)]
pub struct Rule {
    /// Stable machine-readable kind, also used for `--kind` filters.
    pub kind: &'static str,
    /// Directory names this rule matches.
    pub dirs: &'static [&'static str],
    /// The proof required before the directory counts as an artifact.
    pub marker: Marker,
    /// One-line human explanation, shown in the TUI detail pane.
    pub description: &'static str,
}

/// Every rule, in priority order. The first rule whose name *and* marker match
/// wins, which is why `build` appears several times: a `build` beside
/// `build.gradle` is Gradle output, beside `pubspec.yaml` it is Flutter output,
/// and beside `package.json` it only counts when git ignores it.
pub static RULES: &[Rule] = &[
    Rule {
        kind: "node_modules",
        dirs: &["node_modules"],
        marker: Marker::Sibling(&["package.json"]),
        description: "npm/yarn/pnpm dependency tree, restored by an install",
    },
    Rule {
        kind: "js_cache",
        dirs: &[
            ".next",
            ".nuxt",
            ".turbo",
            ".parcel-cache",
            ".cache",
            ".svelte-kit",
        ],
        marker: Marker::Sibling(&["package.json"]),
        description: "JavaScript bundler/framework cache, rebuilt on next run",
    },
    Rule {
        kind: "gradle",
        dirs: &[".gradle", "build"],
        marker: Marker::Sibling(&[
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ]),
        description: "Gradle build output and local daemon cache",
    },
    Rule {
        kind: "flutter",
        dirs: &[".dart_tool", "build"],
        marker: Marker::Sibling(&["pubspec.yaml"]),
        description: "Dart/Flutter build output and tool cache",
    },
    Rule {
        kind: "js_build",
        dirs: &["dist", "build", "out"],
        marker: Marker::SiblingIgnored(&["package.json"]),
        description: "JavaScript build output (only when the repo gitignores it)",
    },
    Rule {
        kind: "venv",
        dirs: &[".venv", "venv", "env", ".env"],
        marker: Marker::Contains("pyvenv.cfg"),
        description: "Python virtualenv, recreated from requirements/lockfile",
    },
    Rule {
        kind: "pycache",
        dirs: &["__pycache__"],
        marker: Marker::NameOnly,
        description: "CPython bytecode cache, regenerated on import",
    },
    Rule {
        kind: "py_tooling",
        dirs: &[".pytest_cache", ".mypy_cache", ".ruff_cache", ".tox"],
        marker: Marker::NameOnly,
        description: "Python tooling cache (pytest/mypy/ruff/tox)",
    },
    Rule {
        kind: "cargo_target",
        dirs: &["target"],
        marker: Marker::Sibling(&["Cargo.toml"]),
        description: "Rust build directory, usually the single biggest win",
    },
    Rule {
        kind: "cocoapods",
        dirs: &["Pods"],
        marker: Marker::Sibling(&["Podfile"]),
        description: "CocoaPods checkout, restored by `pod install`",
    },
    Rule {
        kind: "derived_data",
        dirs: &[],
        marker: Marker::XcodeDerivedData,
        description: "Xcode DerivedData: indexes, modules and build products",
    },
    Rule {
        kind: "go_vendor",
        dirs: &["vendor"],
        marker: Marker::SiblingIgnored(&["go.mod"]),
        description: "Go vendor tree (only when the repo gitignores it)",
    },
    Rule {
        kind: "zig",
        dirs: &["zig-cache", ".zig-cache", "zig-out"],
        marker: Marker::Sibling(&["build.zig"]),
        description: "Zig build cache and install prefix",
    },
    Rule {
        kind: "terraform",
        dirs: &[".terraform"],
        marker: Marker::SiblingExt("tf"),
        description: "Terraform provider plugins, restored by `terraform init`",
    },
];

/// All kind names, in rule order. Used for `--kind` validation and TUI filters.
pub fn all_kinds() -> Vec<&'static str> {
    RULES.iter().map(|r| r.kind).collect()
}

/// Look up a rule by kind name.
pub fn rule_for_kind(kind: &str) -> Option<&'static Rule> {
    RULES.iter().find(|r| r.kind == kind)
}

/// Does any rule use this directory name? Cheap pre-filter for the walker so it
/// only pays for marker checks on plausible candidates.
pub fn is_candidate_name(name: &str) -> bool {
    RULES.iter().any(|r| r.dirs.contains(&name))
}

/// The absolute path of Xcode's DerivedData directory for this user.
pub fn derived_data_root() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/Developer/Xcode/DerivedData"))
}

/// Decide whether `dir` is a reclaimable artifact, and if so which kind.
///
/// `dir` must be a directory (the caller already knows this from its walk).
/// Returns the first matching rule, or `None` when no marker vouches for it.
pub fn match_dir(dir: &Path) -> Option<&'static Rule> {
    let name = dir.file_name()?.to_str()?;
    let parent = dir.parent();

    for rule in RULES {
        let name_ok = match rule.marker {
            Marker::XcodeDerivedData => true, // name is irrelevant, location is the proof
            _ => rule.dirs.contains(&name),
        };
        if !name_ok {
            continue;
        }
        let matched = match rule.marker {
            Marker::Sibling(files) => parent
                .map(|p| files.iter().any(|f| p.join(f).is_file()))
                .unwrap_or(false),
            Marker::SiblingExt(ext) => parent.map(|p| dir_has_ext(p, ext)).unwrap_or(false),
            Marker::SiblingIgnored(files) => parent
                .map(|p| files.iter().any(|f| p.join(f).is_file()) && is_gitignored(p, name))
                .unwrap_or(false),
            Marker::Contains(file) => dir.join(file).is_file(),
            Marker::NameOnly => true,
            Marker::XcodeDerivedData => match (derived_data_root(), parent) {
                (Some(root), Some(p)) => p == root,
                _ => false,
            },
        };
        if matched {
            return Some(rule);
        }
    }
    None
}

/// Does `dir` contain at least one file with extension `ext` (shallow)?
fn dir_has_ext(dir: &Path, ext: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten().take(512) {
        if entry.path().extension().and_then(|e| e.to_str()) == Some(ext) {
            return true;
        }
    }
    false
}

/// Is `name` ignored by a `.gitignore` at `dir` or at an ancestor up to the
/// repository root?
///
/// This is deliberately a small, predictable subset of gitignore semantics:
/// we look for a literal line naming the directory, tolerating the common
/// decorations (`/dist`, `dist/`, `**/dist`, and a trailing comment-free line).
/// Anything fancier (negations, globs, nested ignore files further down) is
/// treated as "not ignored", which fails safe: the directory is simply not
/// reported as reclaimable.
pub fn is_gitignored(dir: &Path, name: &str) -> bool {
    let mut current = Some(dir);
    let mut hops = 0;
    while let Some(d) = current {
        if gitignore_names(d).iter().any(|p| p == name) {
            return true;
        }
        if d.join(".git").exists() || hops >= 8 {
            break;
        }
        hops += 1;
        current = d.parent();
    }
    false
}

/// Directory names listed in `dir/.gitignore`, normalised.
fn gitignore_names(dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(dir.join(".gitignore")) else {
        return Vec::new();
    };
    text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('!'))
        .map(|l| {
            l.trim_start_matches("**/")
                .trim_start_matches('/')
                .trim_end_matches('/')
                .to_string()
        })
        .filter(|l| !l.is_empty() && !l.contains('/') && !l.contains('*'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Build a project directory with the given files (relative paths, `dir/`
    /// suffix means "create a directory") and return the tempdir.
    fn fixture(entries: &[&str]) -> TempDir {
        let td = TempDir::new().unwrap();
        for e in entries {
            let p: PathBuf = td.path().join(e.trim_end_matches('/'));
            if e.ends_with('/') {
                fs::create_dir_all(&p).unwrap();
            } else {
                if let Some(parent) = p.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::write(&p, b"x").unwrap();
            }
        }
        td
    }

    fn kind_of(dir: &Path) -> Option<&'static str> {
        match_dir(dir).map(|r| r.kind)
    }

    #[test]
    fn node_modules_needs_a_package_json() {
        let td = fixture(&["proj/package.json", "proj/node_modules/lodash/index.js"]);
        assert_eq!(
            kind_of(&td.path().join("proj/node_modules")),
            Some("node_modules")
        );

        let td = fixture(&["notaproj/node_modules/whatever.txt"]);
        assert_eq!(kind_of(&td.path().join("notaproj/node_modules")), None);
    }

    #[test]
    fn js_caches_need_a_package_json() {
        let td = fixture(&[
            "app/package.json",
            "app/.next/cache/x",
            "app/.turbo/y",
            "app/.parcel-cache/z",
        ]);
        for d in [".next", ".turbo", ".parcel-cache"] {
            assert_eq!(
                kind_of(&td.path().join("app").join(d)),
                Some("js_cache"),
                "{d}"
            );
        }

        let td = fixture(&["docs/.cache/x"]);
        assert_eq!(kind_of(&td.path().join("docs/.cache")), None);
    }

    #[test]
    fn dist_needs_package_json_and_gitignore() {
        // package.json but no gitignore -> not claimed (could be checked-in output)
        let td = fixture(&["app/package.json", "app/dist/bundle.js"]);
        assert_eq!(kind_of(&td.path().join("app/dist")), None);

        // package.json + gitignored -> claimed
        let td = fixture(&["app/package.json", "app/dist/bundle.js"]);
        fs::write(td.path().join("app/.gitignore"), "node_modules\ndist/\n").unwrap();
        assert_eq!(kind_of(&td.path().join("app/dist")), Some("js_build"));

        // gitignore at the repo root, one level up, also counts
        let td = fixture(&[
            "repo/.git/HEAD",
            "repo/pkg/package.json",
            "repo/pkg/dist/a.js",
        ]);
        fs::write(td.path().join("repo/.gitignore"), "/dist\n").unwrap();
        assert_eq!(kind_of(&td.path().join("repo/pkg/dist")), Some("js_build"));

        // gitignore that does not mention it -> not claimed
        let td = fixture(&["app/package.json", "app/dist/bundle.js"]);
        fs::write(td.path().join("app/.gitignore"), "*.log\ncoverage\n").unwrap();
        assert_eq!(kind_of(&td.path().join("app/dist")), None);
    }

    #[test]
    fn virtualenvs_are_identified_by_pyvenv_cfg() {
        let td = fixture(&["py/.venv/pyvenv.cfg", "py/.venv/lib/x"]);
        assert_eq!(kind_of(&td.path().join("py/.venv")), Some("venv"));

        let td = fixture(&["py/venv/pyvenv.cfg"]);
        assert_eq!(kind_of(&td.path().join("py/venv")), Some("venv"));

        // A directory literally named `env` full of source is NOT a venv.
        let td = fixture(&["py/env/settings.py"]);
        assert_eq!(kind_of(&td.path().join("py/env")), None);
    }

    #[test]
    fn python_tool_caches_need_no_marker() {
        let td = fixture(&[
            "py/__pycache__/mod.pyc",
            "py/.pytest_cache/v",
            "py/.mypy_cache/3.11",
            "py/.ruff_cache/x",
            "py/.tox/py311",
        ]);
        assert_eq!(kind_of(&td.path().join("py/__pycache__")), Some("pycache"));
        for d in [".pytest_cache", ".mypy_cache", ".ruff_cache", ".tox"] {
            assert_eq!(
                kind_of(&td.path().join("py").join(d)),
                Some("py_tooling"),
                "{d}"
            );
        }
    }

    #[test]
    fn cargo_target_needs_a_manifest() {
        let td = fixture(&["rs/Cargo.toml", "rs/target/debug/bin"]);
        assert_eq!(kind_of(&td.path().join("rs/target")), Some("cargo_target"));

        // `target` in a non-Rust project (e.g. a Java project without Maven files) is ignored.
        let td = fixture(&["misc/target/notes.txt"]);
        assert_eq!(kind_of(&td.path().join("misc/target")), None);
    }

    #[test]
    fn gradle_and_flutter_claim_build_before_the_js_rule() {
        let td = fixture(&[
            "and/build.gradle.kts",
            "and/build/outputs/x",
            "and/.gradle/y",
        ]);
        assert_eq!(kind_of(&td.path().join("and/build")), Some("gradle"));
        assert_eq!(kind_of(&td.path().join("and/.gradle")), Some("gradle"));

        let td = fixture(&["fl/pubspec.yaml", "fl/build/app/x", "fl/.dart_tool/y"]);
        assert_eq!(kind_of(&td.path().join("fl/build")), Some("flutter"));
        assert_eq!(kind_of(&td.path().join("fl/.dart_tool")), Some("flutter"));

        let td = fixture(&["plain/build/x"]);
        assert_eq!(kind_of(&td.path().join("plain/build")), None);
    }

    #[test]
    fn pods_need_a_podfile() {
        let td = fixture(&["ios/Podfile", "ios/Pods/Alamofire/x"]);
        assert_eq!(kind_of(&td.path().join("ios/Pods")), Some("cocoapods"));

        let td = fixture(&["ios/Pods/x"]);
        assert_eq!(kind_of(&td.path().join("ios/Pods")), None);
    }

    #[test]
    fn go_vendor_needs_go_mod_and_gitignore() {
        let td = fixture(&["go/go.mod", "go/vendor/github.com/x"]);
        assert_eq!(kind_of(&td.path().join("go/vendor")), None);
        fs::write(td.path().join("go/.gitignore"), "vendor/\n").unwrap();
        assert_eq!(kind_of(&td.path().join("go/vendor")), Some("go_vendor"));
    }

    #[test]
    fn zig_and_terraform() {
        let td = fixture(&["z/build.zig", "z/zig-cache/o", "z/zig-out/bin/x"]);
        assert_eq!(kind_of(&td.path().join("z/zig-cache")), Some("zig"));
        assert_eq!(kind_of(&td.path().join("z/zig-out")), Some("zig"));

        let td = fixture(&["tf/main.tf", "tf/.terraform/providers/x"]);
        assert_eq!(kind_of(&td.path().join("tf/.terraform")), Some("terraform"));

        let td = fixture(&["nottf/.terraform/x"]);
        assert_eq!(kind_of(&td.path().join("nottf/.terraform")), None);
    }

    #[test]
    fn candidate_name_prefilter_agrees_with_rules() {
        assert!(is_candidate_name("node_modules"));
        assert!(is_candidate_name("target"));
        assert!(is_candidate_name("__pycache__"));
        assert!(!is_candidate_name("src"));
        assert!(!is_candidate_name("Documents"));
    }

    #[test]
    fn gitignore_parsing_handles_decorations() {
        let td = fixture(&["r/x"]);
        fs::write(
            td.path().join("r/.gitignore"),
            "# comment\n\n/dist\nbuild/\n**/coverage\n*.log\n!keep\nsub/dir\n",
        )
        .unwrap();
        let d = td.path().join("r");
        assert!(is_gitignored(&d, "dist"));
        assert!(is_gitignored(&d, "build"));
        assert!(is_gitignored(&d, "coverage"));
        assert!(!is_gitignored(&d, "keep"));
        assert!(!is_gitignored(&d, "src"));
    }

    #[test]
    fn every_kind_is_unique_and_described() {
        let mut kinds = all_kinds();
        let n = kinds.len();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), n, "kind names must be unique");
        assert!(RULES.iter().all(|r| !r.description.is_empty()));
        assert!(rule_for_kind("cargo_target").is_some());
        assert!(rule_for_kind("nope").is_none());
    }
}
