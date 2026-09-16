//! Non-interactive command line: `sweep scan` and `sweep clean`.

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::io::IsTerminal;
use std::path::PathBuf;

use crate::remove;
use crate::rules;
use crate::scan::{self, Filter, Hit, ScanOptions, ScanResult, SortKey};
use crate::theme::{self, Swatch};
use crate::util::{elide_middle, human_ago, human_bytes, human_count, shorten_home};

#[derive(Parser, Debug)]
#[command(
    name = "sweep",
    version,
    about = "Find and reclaim stale dev caches: node_modules, target/, .venv, DerivedData and friends",
    long_about = None,
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    /// Directories to scan. Defaults to your home directory.
    #[arg(value_name = "ROOT")]
    pub roots: Vec<PathBuf>,

    /// Maximum directory depth below each root.
    #[arg(long, value_name = "N")]
    pub max_depth: Option<usize>,

    /// Delete permanently instead of moving to the Trash (the TUI says so in red).
    #[arg(long)]
    pub permanent: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Scan and print what could be reclaimed. Never deletes anything.
    Scan {
        /// Directories to scan. Defaults to your home directory.
        #[arg(value_name = "ROOT")]
        roots: Vec<PathBuf>,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
        /// Only these kinds, comma separated (see --help for the list).
        #[arg(long, value_name = "K1,K2", value_delimiter = ',')]
        kind: Vec<String>,
        /// Only projects untouched for at least this long: 90d, 6mo, 1y.
        #[arg(long, value_name = "DURATION")]
        older_than: Option<String>,
        /// Only artifacts at least this big: 50MB, 1.5g.
        #[arg(long, value_name = "SIZE")]
        min_size: Option<String>,
        /// size | age | kind | path
        #[arg(long, default_value = "size")]
        sort: String,
        #[arg(long, value_name = "N")]
        max_depth: Option<usize>,
        /// Show every row instead of the top 40.
        #[arg(long)]
        all: bool,
    },
    /// Reclaim space. Dry run unless you pass --yes.
    Clean {
        /// Directories to scan. Defaults to your home directory.
        #[arg(value_name = "ROOT")]
        roots: Vec<PathBuf>,
        #[arg(long, value_name = "K1,K2", value_delimiter = ',')]
        kind: Vec<String>,
        #[arg(long, value_name = "DURATION")]
        older_than: Option<String>,
        #[arg(long, value_name = "SIZE")]
        min_size: Option<String>,
        /// Print what would be removed and exit. This is the default.
        #[arg(long, conflicts_with = "yes")]
        dry_run: bool,
        /// Actually reclaim the space.
        #[arg(long)]
        yes: bool,
        /// Skip the Trash and delete permanently.
        #[arg(long)]
        permanent: bool,
        #[arg(long, value_name = "N")]
        max_depth: Option<usize>,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// List the detection rules and the marker each one requires.
    Rules,
}

/// Parse args and run. Returns the process exit code.
pub fn main() -> Result<i32> {
    let cli = Cli::parse();
    match cli.command {
        None => {
            let opts = ScanOptions {
                roots: roots_or_home(cli.roots),
                max_depth: cli.max_depth,
                include_derived_data: true,
            };
            crate::tui::run(opts, cli.permanent)?;
            Ok(0)
        }
        Some(Command::Rules) => {
            print_rules();
            Ok(0)
        }
        Some(Command::Scan {
            roots,
            json,
            kind,
            older_than,
            min_size,
            sort,
            max_depth,
            all,
        }) => {
            let filter = build_filter(&kind, older_than.as_deref(), min_size.as_deref())?;
            let sort = SortKey::parse(&sort)
                .ok_or_else(|| anyhow::anyhow!("unknown sort key: {sort} (size|age|kind|path)"))?;
            let res = run_scan(roots, max_depth)?;
            let mut hits = filter.apply(&res.hits);
            scan::sort_hits(&mut hits, sort);
            if json {
                print_json(&res, &hits)?;
            } else {
                print_table(&res, &hits, if all { usize::MAX } else { 40 });
            }
            Ok(0)
        }
        Some(Command::Clean {
            roots,
            kind,
            older_than,
            min_size,
            dry_run,
            yes,
            permanent,
            max_depth,
            json,
        }) => {
            let _ = dry_run;
            let filter = build_filter(&kind, older_than.as_deref(), min_size.as_deref())?;
            let res = run_scan(roots, max_depth)?;
            let mut hits = filter.apply(&res.hits);
            scan::sort_hits(&mut hits, SortKey::Size);
            clean(&res, &hits, yes, permanent, json)
        }
    }
}

fn roots_or_home(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    if roots.is_empty() {
        scan::default_roots()
    } else {
        roots
    }
}

fn build_filter(
    kinds: &[String],
    older_than: Option<&str>,
    min_size: Option<&str>,
) -> Result<Filter> {
    for k in kinds {
        if rules::rule_for_kind(k).is_none() {
            bail!(
                "unknown kind `{k}`. known kinds: {}",
                rules::all_kinds().join(", ")
            );
        }
    }
    Ok(Filter {
        kinds: if kinds.is_empty() {
            None
        } else {
            Some(kinds.to_vec())
        },
        older_than_days: older_than
            .map(crate::util::parse_days)
            .transpose()
            .map_err(anyhow::Error::msg)?,
        min_size: min_size
            .map(crate::util::parse_size)
            .transpose()
            .map_err(anyhow::Error::msg)?,
    })
}

fn run_scan(roots: Vec<PathBuf>, max_depth: Option<usize>) -> Result<ScanResult> {
    let opts = ScanOptions {
        roots: roots_or_home(roots),
        max_depth,
        include_derived_data: true,
    };
    if scan::normalize_roots(&opts.roots).is_empty() {
        bail!("no readable directories to scan");
    }
    Ok(scan::scan(&opts, None))
}

// ------------------------------------------------------------------ output

fn color_enabled() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// Wrap `s` in a truecolor escape when the terminal will render it.
fn paint(s: &str, sw: Swatch, bold: bool) -> String {
    if !color_enabled() {
        return s.to_string();
    }
    let (r, g, b) = sw.rgb;
    let b0 = if bold { "\x1b[1m" } else { "" };
    format!("{b0}\x1b[38;2;{r};{g};{b}m{s}\x1b[0m")
}

fn print_rules() {
    println!("{}", paint("sweep detection rules", theme::BRAND, true));
    println!(
        "{}",
        paint(
            "each artifact must be vouched for by a project marker\n",
            theme::DIM,
            false
        )
    );
    for r in rules::RULES {
        let marker = match r.marker {
            rules::Marker::Sibling(f) => format!("sibling {}", f.join(" or ")),
            rules::Marker::SiblingExt(e) => format!("a sibling *.{e} file"),
            rules::Marker::SiblingIgnored(f) => {
                format!("sibling {} AND gitignored", f.join(" or "))
            }
            rules::Marker::Contains(f) => format!("contains {f}"),
            rules::Marker::NameOnly => "name is unambiguous".to_string(),
            rules::Marker::XcodeDerivedData => {
                "inside ~/Library/Developer/Xcode/DerivedData".to_string()
            }
        };
        let names = if r.dirs.is_empty() {
            "<any>".to_string()
        } else {
            r.dirs.join(", ")
        };
        println!(
            "  {:<14} {}",
            paint(r.kind, theme::kind_swatch(r.kind), true),
            paint(&names, theme::TEXT, false)
        );
        println!("                 {}", paint(&marker, theme::WARN, false));
        println!(
            "                 {}",
            paint(r.description, theme::DIM, false)
        );
    }
}

#[derive(Serialize)]
struct JsonKind {
    kind: String,
    bytes: u64,
    count: usize,
}

#[derive(Serialize)]
struct JsonOut<'a> {
    roots: &'a [PathBuf],
    scanned_dirs: u64,
    elapsed_ms: u64,
    count: usize,
    total_bytes: u64,
    total_files: u64,
    by_kind: Vec<JsonKind>,
    hits: &'a [Hit],
}

fn print_json(res: &ScanResult, hits: &[Hit]) -> Result<()> {
    let out = JsonOut {
        roots: &res.roots,
        scanned_dirs: res.dirs_scanned,
        elapsed_ms: res.elapsed_ms,
        count: hits.len(),
        total_bytes: hits.iter().map(|h| h.size_bytes).sum(),
        total_files: hits.iter().map(|h| h.file_count).sum(),
        by_kind: scan::totals_by_kind(hits)
            .into_iter()
            .map(|(kind, bytes, count)| JsonKind { kind, bytes, count })
            .collect(),
        hits,
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn term_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(100)
        .clamp(60, 160)
}

/// The designed non-interactive table: aligned columns, per-kind colour,
/// a per-kind summary bar chart and a totals line.
fn print_table(res: &ScanResult, hits: &[Hit], limit: usize) {
    let width = term_width();
    let fixed = 14 + 11 + 9 + 13 + 4;
    let path_w = width.saturating_sub(fixed).max(20);

    if hits.is_empty() {
        println!(
            "{}",
            paint(
                "nothing reclaimable found — your disk is already clean",
                theme::OK,
                true
            )
        );
        println!(
            "{}",
            paint(
                &format!(
                    "scanned {} dirs in {:.1}s",
                    human_count(res.dirs_scanned),
                    res.elapsed_ms as f64 / 1000.0
                ),
                theme::DIM,
                false
            )
        );
        return;
    }

    println!(
        "  {:<14}{:<path_w$} {:>10} {:>8} {:>12}",
        paint("KIND", theme::DIM, false),
        paint("PATH", theme::DIM, false),
        paint("SIZE", theme::DIM, false),
        paint("FILES", theme::DIM, false),
        paint("LAST USED", theme::DIM, false),
    );
    println!(
        "  {}",
        paint(&"─".repeat(width.saturating_sub(4)), theme::FAINT, false)
    );

    for hit in hits.iter().take(limit) {
        let kind_sw = theme::kind_swatch(&hit.kind);
        let size_sw = if hit.size_bytes >= 1024 * 1024 * 1024 {
            theme::WARN
        } else {
            theme::TEXT
        };
        let age_sw = match hit.age_days() {
            Some(d) if d >= 180 => theme::DANGER,
            Some(d) if d >= 90 => theme::WARN,
            _ => theme::DIM,
        };
        // Colour codes are invisible but count toward `format!` widths, so each
        // field is padded first and painted afterwards.
        println!(
            "  {}{} {} {} {}",
            paint(
                &format!("{:<14}", elide_middle(&hit.kind, 13)),
                kind_sw,
                false
            ),
            paint(
                &format!(
                    "{:<path_w$}",
                    elide_middle(&shorten_home(&hit.path), path_w)
                ),
                theme::TEXT,
                false
            ),
            paint(
                &format!("{:>9}", human_bytes(hit.size_bytes)),
                size_sw,
                true
            ),
            paint(
                &format!("{:>8}", human_count(hit.file_count)),
                theme::DIM,
                false
            ),
            paint(&format!("{:>12}", human_ago(hit.last_used)), age_sw, false),
        );
    }
    if hits.len() > limit {
        println!(
            "  {}",
            paint(
                &format!("… and {} more (pass --all)", hits.len() - limit),
                theme::DIM,
                false
            )
        );
    }

    let totals = scan::totals_by_kind(hits);
    let max = totals.first().map(|t| t.1).unwrap_or(1).max(1) as f64;
    let bar_w = 24usize;
    println!();
    for (kind, bytes, count) in totals.iter().take(8) {
        let sw = theme::kind_swatch(kind);
        let filled = theme::bar(*bytes as f64 / max, bar_w);
        let pad = bar_w.saturating_sub(filled.chars().count());
        println!(
            "  {}{}{} {} {}",
            paint(&format!("{kind:<14}"), sw, false),
            paint(&filled, sw, false),
            paint(&"·".repeat(pad), theme::FAINT, false),
            paint(&format!("{:>9}", human_bytes(*bytes)), theme::TEXT, true),
            paint(&format!("({count})"), theme::DIM, false),
        );
    }

    let total: u64 = hits.iter().map(|h| h.size_bytes).sum();
    let files: u64 = hits.iter().map(|h| h.file_count).sum();
    println!();
    println!(
        "  {} {} {} {}",
        paint("Reclaimable", theme::DIM, false),
        paint(&human_bytes(total), theme::OK, true),
        paint(
            &format!("across {} dirs / {} files", hits.len(), human_count(files)),
            theme::DIM,
            false
        ),
        paint(
            &format!(
                "· scanned {} dirs in {:.1}s",
                human_count(res.dirs_scanned),
                res.elapsed_ms as f64 / 1000.0
            ),
            theme::FAINT,
            false
        ),
    );
}

#[derive(Serialize)]
struct CleanJson {
    dry_run: bool,
    permanent: bool,
    count: usize,
    total_bytes: u64,
    reclaimed_bytes: u64,
    removed: Vec<PathBuf>,
    refused: Vec<String>,
}

fn clean(res: &ScanResult, hits: &[Hit], yes: bool, permanent: bool, json: bool) -> Result<i32> {
    let total: u64 = hits.iter().map(|h| h.size_bytes).sum();

    if !yes {
        if json {
            let out = CleanJson {
                dry_run: true,
                permanent,
                count: hits.len(),
                total_bytes: total,
                reclaimed_bytes: 0,
                removed: Vec::new(),
                refused: Vec::new(),
            };
            println!("{}", serde_json::to_string_pretty(&out)?);
            return Ok(0);
        }
        println!(
            "{}",
            paint(
                "DRY RUN — nothing will be removed. Add --yes to reclaim.",
                theme::WARN,
                true
            )
        );
        println!();
        print_table(res, hits, usize::MAX);
        println!();
        println!(
            "  {} {} {}",
            paint("Would", theme::DIM, false),
            paint(
                if permanent {
                    "permanently delete"
                } else {
                    "move to Trash"
                },
                if permanent {
                    theme::DANGER
                } else {
                    theme::BRAND
                },
                true
            ),
            paint(
                &format!("{} dirs, {}", hits.len(), human_bytes(total)),
                theme::TEXT,
                true
            ),
        );
        return Ok(0);
    }

    if hits.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&CleanJson {
                    dry_run: false,
                    permanent,
                    count: 0,
                    total_bytes: 0,
                    reclaimed_bytes: 0,
                    removed: vec![],
                    refused: vec![],
                })?
            );
        } else {
            println!("{}", paint("nothing to reclaim", theme::DIM, false));
        }
        return Ok(0);
    }

    let mut reclaimed = 0u64;
    let mut removed = Vec::new();
    let mut refused = Vec::new();
    for hit in hits {
        let outcome = remove::delete_hit(hit, &res.roots, permanent);
        if outcome.is_ok() {
            reclaimed += outcome.bytes();
            removed.push(hit.path.clone());
            if !json {
                println!(
                    "  {} {} {}",
                    paint("✓", theme::OK, true),
                    paint(&shorten_home(&hit.path), theme::TEXT, false),
                    paint(
                        &format!("({})", human_bytes(hit.size_bytes)),
                        theme::DIM,
                        false
                    ),
                );
            }
        } else {
            let msg = format!("{}: {}", shorten_home(&hit.path), outcome.describe());
            if !json {
                println!(
                    "  {} {}",
                    paint("!", theme::WARN, true),
                    paint(&msg, theme::WARN, false)
                );
            }
            refused.push(msg);
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&CleanJson {
                dry_run: false,
                permanent,
                count: removed.len(),
                total_bytes: total,
                reclaimed_bytes: reclaimed,
                removed,
                refused,
            })?
        );
    } else {
        println!();
        println!(
            "  {} {} {}",
            paint("✓ Reclaimed", theme::OK, true),
            paint(&human_bytes(reclaimed), theme::OK, true),
            paint(
                &format!(
                    "from {} dirs ({})",
                    removed.len(),
                    if permanent {
                        "deleted"
                    } else {
                        "moved to Trash"
                    }
                ),
                theme::DIM,
                false
            ),
        );
        if !refused.is_empty() {
            println!(
                "  {}",
                paint(
                    &format!("{} refused or failed", refused.len()),
                    theme::WARN,
                    false
                )
            );
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn bare_roots_parse_without_a_subcommand() {
        let cli = Cli::parse_from(["sweep", "/tmp", "/var"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.roots.len(), 2);
    }

    #[test]
    fn scan_flags_parse() {
        let cli = Cli::parse_from([
            "sweep",
            "scan",
            "/tmp",
            "--json",
            "--kind",
            "node_modules,venv",
            "--older-than",
            "90d",
            "--min-size",
            "50MB",
            "--sort",
            "age",
        ]);
        match cli.command {
            Some(Command::Scan {
                json, kind, sort, ..
            }) => {
                assert!(json);
                assert_eq!(kind, vec!["node_modules", "venv"]);
                assert_eq!(SortKey::parse(&sort), Some(SortKey::LastUsed));
            }
            _ => panic!("expected scan"),
        }
    }

    #[test]
    fn clean_defaults_to_a_dry_run() {
        let cli = Cli::parse_from(["sweep", "clean", "/tmp"]);
        match cli.command {
            Some(Command::Clean { yes, permanent, .. }) => {
                assert!(!yes, "clean must never delete without --yes");
                assert!(!permanent);
            }
            _ => panic!("expected clean"),
        }
    }

    #[test]
    fn dry_run_and_yes_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["sweep", "clean", "--dry-run", "--yes"]).is_err());
    }

    #[test]
    fn unknown_kinds_are_rejected() {
        assert!(build_filter(&["node_modules".into()], None, None).is_ok());
        let err = build_filter(&["nonsense".into()], None, None).unwrap_err();
        assert!(err.to_string().contains("unknown kind"));
    }

    #[test]
    fn filters_convert_human_units() {
        let f = build_filter(&[], Some("6mo"), Some("50MB")).unwrap();
        assert_eq!(f.older_than_days, Some(180));
        assert_eq!(f.min_size, Some(50 * 1024 * 1024));
    }
}
