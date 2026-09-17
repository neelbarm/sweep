//! The scanner: a pruned, parallel directory walk that finds artifacts,
//! measures them, and reports how stale their project is.

use crossbeam_channel::Sender;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::rules;
use crate::util;

/// One reclaimable directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    /// Absolute path of the artifact directory.
    pub path: PathBuf,
    /// Rule kind, e.g. `node_modules` or `cargo_target`.
    pub kind: String,
    /// Apparent size in bytes (sum of file lengths, symlinks excluded).
    pub size_bytes: u64,
    /// Number of regular files inside.
    pub file_count: u64,
    /// Unix seconds of the newest human activity in the owning project.
    pub last_used: Option<i64>,
    /// The project directory the artifact belongs to.
    pub project_root: PathBuf,
}

impl Hit {
    /// Whole days since the owning project was last touched.
    pub fn age_days(&self) -> Option<i64> {
        util::days_since(self.last_used)
    }
}

/// Live scan telemetry, so the TUI can render progress instead of freezing.
#[derive(Debug, Clone)]
pub enum Event {
    /// Counters as the walk proceeds.
    Progress { dirs: u64, hits: u64, bytes: u64 },
    /// A fully measured artifact.
    Hit(Box<Hit>),
    /// The walk finished.
    Done { dirs: u64, elapsed_ms: u64 },
}

/// Scanner configuration.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Directories to search. Normalised and de-nested by [`normalize_roots`].
    pub roots: Vec<PathBuf>,
    /// Maximum depth below each root (`None` = unlimited).
    pub max_depth: Option<usize>,
    /// Include Xcode's DerivedData even though `~/Library` is skipped.
    pub include_derived_data: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            roots: default_roots(),
            max_depth: None,
            include_derived_data: true,
        }
    }
}

/// Everything a scan produced.
#[derive(Debug, Clone)]
pub struct ScanResult {
    pub hits: Vec<Hit>,
    /// The roots actually walked, used later as a delete-safety fence.
    pub roots: Vec<PathBuf>,
    pub dirs_scanned: u64,
    pub elapsed_ms: u64,
}

impl ScanResult {
    pub fn total_bytes(&self) -> u64 {
        self.hits.iter().map(|h| h.size_bytes).sum()
    }
}

/// `~` when nothing is given on the command line.
pub fn default_roots() -> Vec<PathBuf> {
    dirs::home_dir().into_iter().collect()
}

/// Absolute path prefixes that are never worth walking: system volumes, other
/// disks, the Trash, and `~/Library` (Xcode's DerivedData is re-added as its
/// own root when requested, which is the one thing in there worth reclaiming).
fn skip_prefixes() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = [
        "/System", "/Volumes", "/private", "/dev", "/net", "/Library", "/cores",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    if let Some(home) = dirs::home_dir() {
        v.push(home.join("Library"));
        v.push(home.join(".Trash"));
        for d in TOOL_DIRS {
            v.push(home.join(d));
        }
    }
    v
}

/// Home directories owned by a *tool*, not by the user's projects.
///
/// They are full of directories that match the rules perfectly — a VS Code
/// extension really does ship a `node_modules` next to a `package.json` — but
/// deleting them breaks an installed program rather than freeing regenerable
/// build output. `sweep` reports what `npm install` and `cargo build` can put
/// back, so it stays out of here. Point sweep at one explicitly and it will
/// still look: an explicit root always wins over this list.
pub const TOOL_DIRS: &[&str] = &[
    ".npm",
    ".nvm",
    ".bun",
    ".deno",
    ".yarn",
    ".pnpm-store",
    ".cache",
    ".cargo",
    ".rustup",
    ".rbenv",
    ".pyenv",
    ".gem",
    ".m2",
    ".gradle",
    ".android",
    ".docker",
    ".ollama",
    ".vscode",
    ".vscode-insiders",
    ".vscode-server",
    ".cursor",
    ".windsurf",
    ".zed",
    ".codex",
    ".claude",
    ".oh-my-zsh",
    ".local/share",
    ".local/lib",
    "Applications",
];

/// Directory names that never contain a project and cost a lot to walk.
fn skip_dir_name(name: &str) -> bool {
    matches!(name, ".Trash" | ".git" | ".hg" | ".svn" | ".DS_Store")
        || name.ends_with(".app")
        || name.ends_with(".photoslibrary")
        || name.ends_with(".musiclibrary")
        || name.ends_with(".tvlibrary")
        || name.ends_with(".imovielibrary")
        || name.ends_with(".fcpbundle")
        || name.ends_with(".sparsebundle")
        || name.ends_with(".xcodeproj")
        || name.ends_with(".xcworkspace")
        || name.ends_with(".framework")
}

/// Canonicalise roots, drop ones that do not exist, and drop any root nested
/// inside another so nothing is scanned (or offered for deletion) twice.
pub fn normalize_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for r in roots {
        let Ok(c) = fs::canonicalize(r) else { continue };
        if !c.is_dir() {
            continue;
        }
        out.push(c);
    }
    out.sort();
    out.dedup();
    let mut kept: Vec<PathBuf> = Vec::new();
    for r in out {
        if kept.iter().any(|k| r.starts_with(k)) {
            continue;
        }
        kept.push(r);
    }
    kept
}

/// Must Xcode's DerivedData be walked as a root of its own?
///
/// Yes exactly when the user asked for somewhere that contains it but our own
/// pruning would stop the walk getting there — which is the normal case, since
/// `~/Library` is skipped wholesale and DerivedData lives inside it. Adding it
/// as a root (rather than un-pruning `~/Library`) keeps the walk cheap.
///
/// This used to test "is DerivedData under some root?" on its own, which is
/// true for *every* scan of `~` — so the extra root was never added and the
/// `derived_data` rule could never fire from a plain `sweep ~`.
fn needs_derived_data_root(roots: &[PathBuf], dd: &Path, skip: &[PathBuf]) -> bool {
    let asked_for = roots.iter().any(|r| dd.starts_with(r));
    let pruned = skip.iter().any(|p| dd.starts_with(p));
    let already_a_root = roots.iter().any(|r| r.starts_with(dd));
    asked_for && pruned && !already_a_root
}

struct Ctx {
    max_depth: Option<usize>,
    skip: Vec<PathBuf>,
    derived_root: Option<PathBuf>,
    dirs: AtomicU64,
    hit_count: AtomicU64,
    bytes: AtomicU64,
    hits: Mutex<Vec<Hit>>,
    stale_cache: Mutex<HashMap<PathBuf, Option<i64>>>,
    tx: Option<Sender<Event>>,
}

impl Ctx {
    fn tick(&self) {
        let n = self.dirs.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_multiple_of(96) {
            self.emit_progress(n);
        }
    }

    fn emit_progress(&self, dirs: u64) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(Event::Progress {
                dirs,
                hits: self.hit_count.load(Ordering::Relaxed),
                bytes: self.bytes.load(Ordering::Relaxed),
            });
        }
    }

    fn should_skip(&self, path: &Path, name: &str) -> bool {
        if skip_dir_name(name) {
            return true;
        }
        self.skip.iter().any(|p| path == p || path.starts_with(p))
    }
}

/// Run a scan. Blocks until finished; send `tx` in from another thread if you
/// want to render progress while it runs.
pub fn scan(opts: &ScanOptions, tx: Option<Sender<Event>>) -> ScanResult {
    let started = Instant::now();
    let mut roots = normalize_roots(&opts.roots);

    let derived_root = rules::derived_data_root().filter(|p| p.is_dir());

    // A skip prefix loses to an explicit request: if the user points sweep at
    // something inside `/private` or `~/Library`, that is what they meant.
    // Computed from the roots the *user* gave, before DerivedData is appended.
    let skip: Vec<PathBuf> = skip_prefixes()
        .into_iter()
        .filter(|p| !roots.iter().any(|r| r.starts_with(p)))
        .collect();

    if opts.include_derived_data {
        if let Some(dd) = &derived_root {
            if needs_derived_data_root(&roots, dd, &skip) {
                roots.push(dd.clone());
            }
        }
    }

    let ctx = Ctx {
        max_depth: opts.max_depth,
        skip,
        derived_root,
        dirs: AtomicU64::new(0),
        hit_count: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        hits: Mutex::new(Vec::new()),
        stale_cache: Mutex::new(HashMap::new()),
        tx: tx.clone(),
    };

    rayon::scope(|s| {
        for root in &roots {
            let root = root.clone();
            let ctx = &ctx;
            s.spawn(move |s| walk(s, root, 0, ctx));
        }
    });

    let dirs = ctx.dirs.load(Ordering::Relaxed);
    ctx.emit_progress(dirs);
    let elapsed_ms = started.elapsed().as_millis() as u64;
    if let Some(tx) = &tx {
        let _ = tx.send(Event::Done { dirs, elapsed_ms });
    }

    let mut hits = ctx.hits.into_inner().unwrap_or_default();
    hits.sort_by_key(|h| std::cmp::Reverse(h.size_bytes));
    ScanResult {
        hits,
        roots,
        dirs_scanned: dirs,
        elapsed_ms,
    }
}

/// Walk one directory. Matched artifacts are measured and **not** descended
/// into, which is what keeps the walk cheap: a 60k-file `node_modules` costs
/// one size pass instead of 60k rule evaluations.
fn walk<'s>(scope: &rayon::Scope<'s>, dir: PathBuf, depth: usize, ctx: &'s Ctx) {
    ctx.tick();
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let in_derived_root = ctx.derived_root.as_deref() == Some(dir.as_path());

    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() || !ft.is_dir() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if ctx.should_skip(&path, name) && !in_derived_root {
            continue;
        }

        // Only plausible names pay for a marker check.
        let rule = if in_derived_root || rules::is_candidate_name(name) {
            rules::match_dir(&path)
        } else {
            None
        };

        if let Some(rule) = rule {
            let kind = rule.kind;
            scope.spawn(move |_| measure(path, kind, ctx));
            continue; // never descend into a match
        }

        if ctx.max_depth.map(|m| depth + 1 > m).unwrap_or(false) {
            continue;
        }
        scope.spawn(move |s| walk(s, path, depth + 1, ctx));
    }
}

/// Size an artifact and attach the staleness signal, then publish it.
fn measure(path: PathBuf, kind: &'static str, ctx: &Ctx) {
    let (size_bytes, file_count) = dir_size(&path);
    let project_root = if kind == "derived_data" {
        path.clone()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.clone())
    };
    let last_used = if kind == "derived_data" {
        mtime_secs(&path)
    } else {
        cached_last_used(&project_root, ctx)
    };

    let hit = Hit {
        path,
        kind: kind.to_string(),
        size_bytes,
        file_count,
        last_used,
        project_root,
    };
    ctx.bytes.fetch_add(size_bytes, Ordering::Relaxed);
    ctx.hit_count.fetch_add(1, Ordering::Relaxed);
    if let Some(tx) = &ctx.tx {
        let _ = tx.send(Event::Hit(Box::new(hit.clone())));
    }
    if let Ok(mut v) = ctx.hits.lock() {
        v.push(hit);
    }
}

/// Apparent size of a directory: the sum of file lengths. Symlinks are never
/// followed, so a symlinked cache is never double-counted.
///
/// The traversal is breadth-first over an explicit worklist — one level at a
/// time, fanned across the rayon pool — rather than recursive. Recursing here
/// used to overflow the worker stack and abort the whole process on deeply
/// nested trees (a few hundred levels is enough, and npm used to nest that
/// far). An explicit worklist makes the depth a heap cost instead.
pub fn dir_size(path: &Path) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut level: Vec<PathBuf> = vec![path.to_path_buf()];

    while !level.is_empty() {
        let (b, f, next) = level.par_iter().map(|dir| shallow_size(dir)).reduce(
            || (0u64, 0u64, Vec::new()),
            |mut a, b| {
                a.0 = a.0.saturating_add(b.0);
                a.1 = a.1.saturating_add(b.1);
                a.2.extend(b.2);
                a
            },
        );
        bytes = bytes.saturating_add(b);
        files = files.saturating_add(f);
        level = next;
    }
    (bytes, files)
}

/// One directory, no recursion: its own file bytes/count plus the
/// sub-directories still to visit.
fn shallow_size(dir: &Path) -> (u64, u64, Vec<PathBuf>) {
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut subdirs: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                subdirs.push(entry.path());
            } else if ft.is_file() {
                if let Ok(md) = entry.metadata() {
                    bytes = bytes.saturating_add(md.len());
                    files = files.saturating_add(1);
                }
            }
        }
    }
    (bytes, files, subdirs)
}

/// Top-level contents of a directory by size, for the TUI detail view.
pub fn top_level_breakdown(path: &Path, limit: usize) -> Vec<(String, u64)> {
    let mut rows: Vec<(String, u64)> = Vec::new();
    if let Ok(entries) = fs::read_dir(path) {
        let items: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| !e.file_type().map(|t| t.is_symlink()).unwrap_or(true))
            .map(|e| e.path())
            .collect();
        rows = items
            .par_iter()
            .map(|p| {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let size = if p.is_dir() {
                    dir_size(p).0
                } else {
                    p.metadata().map(|m| m.len()).unwrap_or(0)
                };
                (name, size)
            })
            .collect();
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.1));
    rows.truncate(limit);
    rows
}

fn mtime_secs(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

fn cached_last_used(root: &Path, ctx: &Ctx) -> Option<i64> {
    if let Ok(cache) = ctx.stale_cache.lock() {
        if let Some(v) = cache.get(root) {
            return *v;
        }
    }
    let v = project_last_used(root);
    if let Ok(mut cache) = ctx.stale_cache.lock() {
        cache.insert(root.to_path_buf(), v);
    }
    v
}

/// When did a human last touch this project?
///
/// Two signals, maxed together:
///  * the newest mtime among the project's own shallow entries, skipping
///    artifact directories (a rebuild must not make a project look alive), and
///  * git activity, approximated by the mtime of `.git/HEAD`, `.git/packed-refs`
///    and the loose refs under `.git/refs/heads` — which git rewrites on every
///    commit, checkout and fetch.
pub fn project_last_used(root: &Path) -> Option<i64> {
    const MAX_ENTRIES: usize = 2000;
    let mut newest: Option<i64> = None;

    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten().take(MAX_ENTRIES) {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if rules::is_candidate_name(&name) || name == ".git" || name == ".DS_Store" {
                continue;
            }
            let Ok(md) = entry.metadata() else { continue };
            if let Some(t) = md
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
            {
                newest = Some(newest.map_or(t, |n: i64| n.max(t)));
            }
        }
    }

    let git = root.join(".git");
    if git.exists() {
        for p in [
            git.join("HEAD"),
            git.join("packed-refs"),
            git.join("FETCH_HEAD"),
        ] {
            if let Some(t) = mtime_secs(&p) {
                newest = Some(newest.map_or(t, |n: i64| n.max(t)));
            }
        }
        if let Ok(entries) = fs::read_dir(git.join("refs/heads")) {
            for entry in entries.flatten().take(256) {
                if let Some(t) = mtime_secs(&entry.path()) {
                    newest = Some(newest.map_or(t, |n: i64| n.max(t)));
                }
            }
        }
    }

    // Guard against clock skew, and against archive sentinel dates: npm and
    // many package tarballs stamp every file with 1985-10-26 or the unix
    // epoch, which would otherwise report a freshly installed dependency tree
    // as "40 y ago". A project we cannot honestly date reports `unknown`.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(i64::MAX);
    newest.map(|t| t.min(now)).filter(|t| *t >= SENTINEL_FLOOR)
}

/// Timestamps before 2000-01-01 are archive sentinels, not evidence of use.
const SENTINEL_FLOOR: i64 = 946_684_800;

/// Sort orders shared by the TUI and the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Size,
    LastUsed,
    Kind,
    Path,
}

impl SortKey {
    pub fn label(&self) -> &'static str {
        match self {
            SortKey::Size => "size",
            SortKey::LastUsed => "last used",
            SortKey::Kind => "kind",
            SortKey::Path => "path",
        }
    }

    pub fn next(&self) -> SortKey {
        match self {
            SortKey::Size => SortKey::LastUsed,
            SortKey::LastUsed => SortKey::Kind,
            SortKey::Kind => SortKey::Path,
            SortKey::Path => SortKey::Size,
        }
    }

    pub fn parse(s: &str) -> Option<SortKey> {
        match s.to_ascii_lowercase().as_str() {
            "size" => Some(SortKey::Size),
            "age" | "last-used" | "lastused" | "last_used" => Some(SortKey::LastUsed),
            "kind" => Some(SortKey::Kind),
            "path" => Some(SortKey::Path),
            _ => None,
        }
    }
}

/// Sort hits in place. Oldest-first for `LastUsed`, biggest-first for `Size`.
pub fn sort_hits(hits: &mut [Hit], key: SortKey) {
    match key {
        SortKey::Size => hits.sort_by_key(|h| std::cmp::Reverse(h.size_bytes)),
        SortKey::LastUsed => hits.sort_by(|a, b| {
            let av = a.last_used.unwrap_or(0);
            let bv = b.last_used.unwrap_or(0);
            av.cmp(&bv).then(b.size_bytes.cmp(&a.size_bytes))
        }),
        SortKey::Kind => {
            hits.sort_by(|a, b| a.kind.cmp(&b.kind).then(b.size_bytes.cmp(&a.size_bytes)))
        }
        SortKey::Path => hits.sort_by(|a, b| a.path.cmp(&b.path)),
    }
}

/// Post-scan filters shared by the TUI and the CLI.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub kinds: Option<Vec<String>>,
    pub older_than_days: Option<i64>,
    pub min_size: Option<u64>,
}

impl Filter {
    pub fn matches(&self, hit: &Hit) -> bool {
        if let Some(kinds) = &self.kinds {
            if !kinds.iter().any(|k| k == &hit.kind) {
                return false;
            }
        }
        if let Some(days) = self.older_than_days {
            // An unknown age fails the filter. "I could not date this
            // project" must never be presented as "nobody has touched it in a
            // year".
            match hit.age_days() {
                Some(a) if a >= days => {}
                _ => return false,
            }
        }
        if let Some(min) = self.min_size {
            if hit.size_bytes < min {
                return false;
            }
        }
        true
    }

    pub fn apply(&self, hits: &[Hit]) -> Vec<Hit> {
        hits.iter().filter(|h| self.matches(h)).cloned().collect()
    }
}

/// Totals per kind, biggest first. Powers the header bar chart.
pub fn totals_by_kind(hits: &[Hit]) -> Vec<(String, u64, usize)> {
    let mut map: HashMap<&str, (u64, usize)> = HashMap::new();
    for h in hits {
        let e = map.entry(h.kind.as_str()).or_insert((0, 0));
        e.0 += h.size_bytes;
        e.1 += 1;
    }
    let mut v: Vec<(String, u64, usize)> = map
        .into_iter()
        .map(|(k, (b, c))| (k.to_string(), b, c))
        .collect();
    v.sort_by_key(|k| std::cmp::Reverse(k.1));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(p: &Path, bytes: usize) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![b'x'; bytes]).unwrap();
    }

    #[test]
    fn dir_size_sums_files_recursively() {
        let td = TempDir::new().unwrap();
        write(&td.path().join("a.bin"), 1000);
        write(&td.path().join("sub/b.bin"), 2000);
        write(&td.path().join("sub/deep/c.bin"), 3000);
        let (bytes, files) = dir_size(td.path());
        assert_eq!(bytes, 6000);
        assert_eq!(files, 3);
    }

    #[test]
    fn dir_size_survives_a_deeply_nested_tree() {
        // Regression: `dir_size` used to recurse, so a few hundred levels
        // overflowed the rayon worker stack and aborted the whole process.
        let td = TempDir::new().unwrap();
        let mut p = td.path().to_path_buf();
        let mut depth = 0;
        while depth < 450 {
            p.push("d");
            if fs::create_dir(&p).is_err() {
                p.pop();
                break;
            }
            depth += 1;
        }
        assert!(
            depth > 150,
            "need a deep tree to exercise this, got {depth}"
        );
        fs::write(p.join("leaf.bin"), vec![b'x'; 1234]).unwrap();
        let (bytes, files) = dir_size(td.path());
        assert_eq!(bytes, 1234);
        assert_eq!(files, 1);
    }

    #[test]
    fn derived_data_joins_as_a_root_when_library_is_pruned() {
        let home = PathBuf::from("/Users/someone");
        let dd = home.join("Library/Developer/Xcode/DerivedData");
        let library = home.join("Library");

        let library_only = std::slice::from_ref(&library);

        // Scanning `~`: `~/Library` is pruned, so DerivedData must be added.
        // This is the case that silently never fired.
        assert!(needs_derived_data_root(
            std::slice::from_ref(&home),
            &dd,
            library_only
        ));

        // Pointed straight at DerivedData: nothing is pruned, no extra root.
        assert!(!needs_derived_data_root(
            std::slice::from_ref(&dd),
            &dd,
            &[]
        ));

        // Pointed inside it: already covered by a root at or below it.
        assert!(!needs_derived_data_root(
            &[dd.join("App-abc")],
            &dd,
            library_only
        ));

        // An unrelated root does not drag DerivedData in.
        assert!(!needs_derived_data_root(
            &[PathBuf::from("/Users/someone/projects")],
            &dd,
            library_only
        ));
    }

    #[test]
    fn dir_size_ignores_symlinks() {
        let td = TempDir::new().unwrap();
        write(&td.path().join("real/a.bin"), 5000);
        #[cfg(unix)]
        std::os::unix::fs::symlink(td.path().join("real"), td.path().join("link")).unwrap();
        let (bytes, files) = dir_size(td.path());
        assert_eq!(bytes, 5000);
        assert_eq!(files, 1);
    }

    #[test]
    fn scan_finds_artifacts_and_does_not_descend_into_them() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        write(&root.join("app/package.json"), 10);
        write(&root.join("app/src/index.js"), 10);
        write(&root.join("app/node_modules/left-pad/index.js"), 4096);
        // A nested node_modules inside a match must not be reported separately.
        write(&root.join("app/node_modules/pkg/package.json"), 10);
        write(
            &root.join("app/node_modules/pkg/node_modules/dep/x.js"),
            2048,
        );
        write(&root.join("rs/Cargo.toml"), 10);
        write(&root.join("rs/target/debug/app"), 8192);
        write(&root.join("plain/README.md"), 10);

        let res = scan(
            &ScanOptions {
                roots: vec![root.to_path_buf()],
                max_depth: None,
                include_derived_data: false,
            },
            None,
        );
        let kinds: Vec<&str> = res.hits.iter().map(|h| h.kind.as_str()).collect();
        assert_eq!(res.hits.len(), 2, "got {kinds:?}");
        assert!(kinds.contains(&"node_modules"));
        assert!(kinds.contains(&"cargo_target"));

        let nm = res.hits.iter().find(|h| h.kind == "node_modules").unwrap();
        // The nested dependency bytes are counted, but only once, by the parent.
        assert_eq!(nm.size_bytes, 4096 + 10 + 2048);
        assert_eq!(nm.file_count, 3);
        assert!(res.dirs_scanned > 0);
    }

    #[test]
    fn scan_respects_max_depth() {
        let td = TempDir::new().unwrap();
        write(&td.path().join("a/b/c/app/package.json"), 10);
        write(&td.path().join("a/b/c/app/node_modules/x.js"), 100);
        let shallow = scan(
            &ScanOptions {
                roots: vec![td.path().to_path_buf()],
                max_depth: Some(2),
                include_derived_data: false,
            },
            None,
        );
        assert!(shallow.hits.is_empty());
        let deep = scan(
            &ScanOptions {
                roots: vec![td.path().to_path_buf()],
                max_depth: Some(8),
                include_derived_data: false,
            },
            None,
        );
        assert_eq!(deep.hits.len(), 1);
    }

    #[test]
    fn staleness_ignores_artifact_dirs_but_sees_source() {
        let td = TempDir::new().unwrap();
        let proj = td.path().join("app");
        write(&proj.join("package.json"), 10);
        write(&proj.join("src/index.js"), 10);
        write(&proj.join("node_modules/x.js"), 10);
        let t = project_last_used(&proj).expect("some mtime");
        let now = util::now_secs();
        assert!(
            (now - t).abs() < 120,
            "expected a recent mtime, got {t} vs {now}"
        );

        // Empty project directory: nothing to date it by.
        let empty = td.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert!(project_last_used(&empty).is_none());
    }

    #[test]
    fn git_refs_count_as_activity() {
        let td = TempDir::new().unwrap();
        let proj = td.path().join("repo");
        write(&proj.join(".git/HEAD"), 40);
        let t = project_last_used(&proj);
        assert!(t.is_some(), "the .git/HEAD mtime should date the project");
    }

    #[test]
    fn archive_sentinel_dates_are_not_evidence_of_use() {
        let td = TempDir::new().unwrap();
        let proj = td.path().join("pkg");
        write(&proj.join("package.json"), 10);
        // npm and many tarballs stamp every extracted file with 1985-10-26.
        let ok = std::process::Command::new("touch")
            .args(["-t", "198510260000"])
            .arg(proj.join("package.json"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return; // no `touch` available; nothing to assert
        }
        assert_eq!(
            project_last_used(&proj),
            None,
            "a 1985 timestamp must read as unknown, not as '40 y ago'"
        );

        // A single real file in the same project restores a usable date.
        write(&proj.join("index.js"), 10);
        assert!(project_last_used(&proj).is_some());
    }

    #[test]
    fn an_unknown_age_never_satisfies_older_than() {
        let hit = Hit {
            path: PathBuf::from("/tmp/x/node_modules"),
            kind: "node_modules".to_string(),
            size_bytes: 1024,
            file_count: 1,
            last_used: None,
            project_root: PathBuf::from("/tmp/x"),
        };
        let f = Filter {
            older_than_days: Some(90),
            ..Default::default()
        };
        assert!(!f.matches(&hit), "undatable projects must not look stale");
        // With no age filter it is still reported.
        assert!(Filter::default().matches(&hit));
    }

    #[test]
    fn tool_installs_are_pruned_but_an_explicit_root_wins() {
        let Some(home) = dirs::home_dir() else { return };
        let skips = skip_prefixes();
        for d in [".npm", ".vscode", ".cargo", ".local/share"] {
            assert!(
                skips.contains(&home.join(d)),
                "{d} should be pruned by default"
            );
        }
        assert!(skips.contains(&home.join("Library")));
        // Every tool dir is home-relative, never absolute.
        assert!(TOOL_DIRS.iter().all(|d| !d.starts_with('/')));
    }

    #[test]
    fn nested_roots_are_collapsed() {
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join("a/b")).unwrap();
        let roots = normalize_roots(&[
            td.path().join("a"),
            td.path().join("a/b"),
            td.path().join("a"),
            td.path().join("does-not-exist"),
        ]);
        assert_eq!(roots.len(), 1);
        assert!(roots[0].ends_with("a"));
    }

    #[test]
    fn filters_select_by_kind_age_and_size() {
        let now = util::now_secs();
        let mk = |kind: &str, size: u64, days_old: i64| Hit {
            path: PathBuf::from("/tmp/x"),
            kind: kind.to_string(),
            size_bytes: size,
            file_count: 1,
            last_used: Some(now - days_old * 86400),
            project_root: PathBuf::from("/tmp"),
        };
        let hits = vec![
            mk("node_modules", 100 * 1024 * 1024, 200),
            mk("cargo_target", 5 * 1024 * 1024, 2),
            mk("venv", 50 * 1024 * 1024, 100),
        ];
        let f = Filter {
            kinds: Some(vec!["node_modules".into(), "venv".into()]),
            older_than_days: Some(90),
            min_size: Some(10 * 1024 * 1024),
        };
        let got = f.apply(&hits);
        assert_eq!(got.len(), 2);
        let f = Filter {
            older_than_days: Some(90),
            ..Default::default()
        };
        assert_eq!(f.apply(&hits).len(), 2);
        let f = Filter {
            min_size: Some(60 * 1024 * 1024),
            ..Default::default()
        };
        assert_eq!(f.apply(&hits).len(), 1);
    }

    #[test]
    fn sorting_orders_as_documented() {
        let now = util::now_secs();
        let mk = |kind: &str, size: u64, days: i64, path: &str| Hit {
            path: PathBuf::from(path),
            kind: kind.to_string(),
            size_bytes: size,
            file_count: 1,
            last_used: Some(now - days * 86400),
            project_root: PathBuf::from("/tmp"),
        };
        let mut hits = vec![
            mk("venv", 10, 5, "/b"),
            mk("cargo_target", 300, 100, "/a"),
            mk("node_modules", 200, 50, "/c"),
        ];
        sort_hits(&mut hits, SortKey::Size);
        assert_eq!(hits[0].size_bytes, 300);
        sort_hits(&mut hits, SortKey::LastUsed);
        assert_eq!(hits[0].kind, "cargo_target", "oldest first");
        sort_hits(&mut hits, SortKey::Path);
        assert_eq!(hits[0].path, PathBuf::from("/a"));
        sort_hits(&mut hits, SortKey::Kind);
        assert_eq!(hits[0].kind, "cargo_target");
        assert_eq!(SortKey::Size.next(), SortKey::LastUsed);
        assert_eq!(SortKey::parse("age"), Some(SortKey::LastUsed));
    }

    #[test]
    fn totals_by_kind_are_ranked() {
        let mk = |kind: &str, size: u64| Hit {
            path: PathBuf::from("/tmp/x"),
            kind: kind.to_string(),
            size_bytes: size,
            file_count: 1,
            last_used: None,
            project_root: PathBuf::from("/tmp"),
        };
        let hits = vec![
            mk("venv", 10),
            mk("node_modules", 100),
            mk("node_modules", 50),
        ];
        let totals = totals_by_kind(&hits);
        assert_eq!(totals[0].0, "node_modules");
        assert_eq!(totals[0].1, 150);
        assert_eq!(totals[0].2, 2);
    }

    #[test]
    fn progress_events_reach_the_channel() {
        let td = TempDir::new().unwrap();
        write(&td.path().join("app/package.json"), 10);
        write(&td.path().join("app/node_modules/x.js"), 1024);
        let (tx, rx) = crossbeam_channel::unbounded();
        let res = scan(
            &ScanOptions {
                roots: vec![td.path().to_path_buf()],
                max_depth: None,
                include_derived_data: false,
            },
            Some(tx),
        );
        assert_eq!(res.hits.len(), 1);
        let events: Vec<Event> = rx.into_iter().collect();
        assert!(events.iter().any(|e| matches!(e, Event::Hit(_))));
        assert!(events.iter().any(|e| matches!(e, Event::Done { .. })));
    }
}
