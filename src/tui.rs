//! The interactive terminal UI: a live-updating, animated table of everything
//! `sweep` can reclaim.
//!
//! Rendering runs on a fixed ~33ms frame budget. Every number the user sees is
//! *eased* toward its real value rather than snapped, which is what makes a
//! scan that discovers 40 GB over eight seconds feel like a dial spinning up
//! instead of a log scrolling past.

use anyhow::Result;
use crossbeam_channel::{unbounded, Receiver};
use crossterm::event::{self, Event as CEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::{HashMap, HashSet};
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::remove::{self, Outcome};
use crate::rules;
use crate::scan::{self, Event as ScanEvent, Filter, Hit, ScanOptions, SortKey};
use crate::theme::{self, Theme};
use crate::util::{elide_middle, human_ago, human_bytes, human_count, shorten_home};

const FRAME: Duration = Duration::from_millis(33);
/// Rows flash this long after they are discovered.
const FLASH_MS: u128 = 700;
/// The select/deselect "pop" lasts this long.
const POP_MS: u128 = 220;
const AGE_STEPS: [(&str, Option<i64>); 4] = [
    ("any", None),
    ("30d+", Some(30)),
    ("90d+", Some(90)),
    ("180d+", Some(180)),
];

/// What the screen is showing.
enum Mode {
    Table,
    Details {
        hit: usize,
        rows: Option<Vec<(String, u64)>>,
        rx: Receiver<Vec<(String, u64)>>,
        since: Instant,
    },
    Confirm {
        since: Instant,
    },
    Deleting {
        total: usize,
        done: usize,
        current: String,
        reclaimed: u64,
        failures: Vec<String>,
        rx: Receiver<DeleteEvent>,
        removed: Vec<PathBuf>,
    },
    Summary {
        reclaimed: u64,
        count: usize,
        failures: Vec<String>,
        since: Instant,
    },
}

enum DeleteEvent {
    Start(String),
    Finished(PathBuf, Outcome),
    AllDone,
}

/// Values that chase their targets frame by frame.
#[derive(Default)]
struct Anim {
    total: f64,
    selected: f64,
    count: f64,
    per_kind: HashMap<String, f64>,
    delete_ratio: f64,
}

fn ease(current: f64, target: f64, dt: f64, speed: f64) -> f64 {
    if (target - current).abs() < 0.5 {
        return target;
    }
    let k = 1.0 - (-dt * speed).exp();
    current + (target - current) * k
}

struct App {
    theme: Theme,
    permanent: bool,
    hits: Vec<Hit>,
    discovered: Vec<Instant>,
    view: Vec<usize>,
    selected: HashSet<usize>,
    pops: HashMap<usize, Instant>,
    cursor: usize,
    offset: usize,
    rows_visible: usize,
    sort: SortKey,
    kind_filter: Option<String>,
    age_step: usize,
    scanning: bool,
    dirs: u64,
    started: Instant,
    scan_elapsed: Option<Duration>,
    spinner: usize,
    last_tick: Instant,
    anim: Anim,
    status: Option<(String, Instant, bool)>,
    mode: Mode,
    roots: Vec<PathBuf>,
    dirty: bool,
    quit: bool,
}

impl App {
    fn new(theme: Theme, permanent: bool, roots: Vec<PathBuf>) -> Self {
        App {
            theme,
            permanent,
            hits: Vec::new(),
            discovered: Vec::new(),
            view: Vec::new(),
            selected: HashSet::new(),
            pops: HashMap::new(),
            cursor: 0,
            offset: 0,
            rows_visible: 10,
            sort: SortKey::Size,
            kind_filter: None,
            age_step: 0,
            scanning: true,
            dirs: 0,
            started: Instant::now(),
            scan_elapsed: None,
            spinner: 0,
            last_tick: Instant::now(),
            anim: Anim::default(),
            status: None,
            mode: Mode::Table,
            roots,
            dirty: true,
            quit: false,
        }
    }

    fn filter(&self) -> Filter {
        Filter {
            kinds: self.kind_filter.as_ref().map(|k| vec![k.clone()]),
            older_than_days: AGE_STEPS[self.age_step].1,
            min_size: None,
        }
    }

    /// Rebuild the visible index list, preserving the cursor's hit where possible.
    fn rebuild_view(&mut self) {
        let anchor = self.view.get(self.cursor).copied();
        let filter = self.filter();
        let mut idx: Vec<usize> = (0..self.hits.len())
            .filter(|i| filter.matches(&self.hits[*i]))
            .collect();
        let hits = &self.hits;
        let sort = self.sort;
        idx.sort_by(|a, b| {
            let (x, y) = (&hits[*a], &hits[*b]);
            match sort {
                SortKey::Size => y.size_bytes.cmp(&x.size_bytes),
                SortKey::LastUsed => x
                    .last_used
                    .unwrap_or(0)
                    .cmp(&y.last_used.unwrap_or(0))
                    .then(y.size_bytes.cmp(&x.size_bytes)),
                SortKey::Kind => x.kind.cmp(&y.kind).then(y.size_bytes.cmp(&x.size_bytes)),
                SortKey::Path => x.path.cmp(&y.path),
            }
        });
        self.view = idx;
        if let Some(a) = anchor {
            if let Some(pos) = self.view.iter().position(|i| *i == a) {
                self.cursor = pos;
            }
        }
        if self.cursor >= self.view.len() {
            self.cursor = self.view.len().saturating_sub(1);
        }
        self.dirty = false;
    }

    fn visible_bytes(&self) -> u64 {
        self.view.iter().map(|i| self.hits[*i].size_bytes).sum()
    }

    fn selected_bytes(&self) -> u64 {
        self.selected.iter().map(|i| self.hits[*i].size_bytes).sum()
    }

    fn kinds_present(&self) -> Vec<String> {
        scan::totals_by_kind(&self.hits)
            .into_iter()
            .map(|(k, _, _)| k)
            .collect()
    }

    fn cycle_kind(&mut self) {
        let kinds = self.kinds_present();
        if kinds.is_empty() {
            return;
        }
        self.kind_filter = match &self.kind_filter {
            None => Some(kinds[0].clone()),
            Some(cur) => match kinds.iter().position(|k| k == cur) {
                Some(i) if i + 1 < kinds.len() => Some(kinds[i + 1].clone()),
                _ => None,
            },
        };
        self.dirty = true;
    }

    fn note(&mut self, msg: impl Into<String>, warn: bool) {
        self.status = Some((msg.into(), Instant::now(), warn));
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.view.is_empty() {
            return;
        }
        let max = self.view.len() as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, max) as usize;
    }

    fn toggle_selection(&mut self) {
        if let Some(i) = self.view.get(self.cursor).copied() {
            if !self.selected.remove(&i) {
                self.selected.insert(i);
            }
            self.pops.insert(i, Instant::now());
        }
    }

    fn select_all_visible(&mut self) {
        let all_selected =
            !self.view.is_empty() && self.view.iter().all(|i| self.selected.contains(i));
        let now = Instant::now();
        for i in self.view.clone() {
            if all_selected {
                self.selected.remove(&i);
            } else {
                self.selected.insert(i);
            }
            self.pops.insert(i, now);
        }
        if all_selected {
            self.note("cleared selection", false);
        } else {
            self.note(format!("selected {} visible", self.view.len()), false);
        }
    }

    fn selected_hits(&self) -> Vec<(usize, Hit)> {
        let mut v: Vec<(usize, Hit)> = self
            .selected
            .iter()
            .filter_map(|i| self.hits.get(*i).map(|h| (*i, h.clone())))
            .collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.1.size_bytes));
        v
    }
}

/// Run the TUI end to end: set up the terminal, scan in the background, render,
/// and always restore the terminal even on error or panic-free early exit.
pub fn run(opts: ScanOptions, permanent: bool) -> Result<()> {
    let roots = scan::normalize_roots(&opts.roots);
    if roots.is_empty() {
        anyhow::bail!("no readable directories to scan");
    }
    let theme = Theme::detect();
    let mut terminal = setup_terminal()?;
    let result = event_loop(&mut terminal, opts, roots, theme, permanent);
    restore_terminal(&mut terminal)?;
    result
}

type Term = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, crossterm::cursor::Hide)?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn event_loop(
    terminal: &mut Term,
    opts: ScanOptions,
    roots: Vec<PathBuf>,
    theme: Theme,
    permanent: bool,
) -> Result<()> {
    let (tx, rx) = unbounded::<ScanEvent>();
    let scan_opts = ScanOptions {
        roots: roots.clone(),
        ..opts
    };
    std::thread::spawn(move || {
        scan::scan(&scan_opts, Some(tx));
    });

    let mut app = App::new(theme, permanent, roots);

    while !app.quit {
        let frame_start = Instant::now();

        drain_scan(&mut app, &rx);
        pump_mode(&mut app);
        if app.dirty {
            app.rebuild_view();
        }
        tick_animations(&mut app);

        terminal.draw(|f| draw(f, &mut app))?;

        // Use the remaining frame budget as the input timeout: no busy loop,
        // and a keypress still lands within a frame.
        while frame_start.elapsed() < FRAME {
            let left = FRAME.saturating_sub(frame_start.elapsed());
            if event::poll(left)? {
                if let CEvent::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        handle_key(&mut app, key);
                    }
                }
            } else {
                break;
            }
        }
    }
    Ok(())
}

fn drain_scan(app: &mut App, rx: &Receiver<ScanEvent>) {
    // Bounded per frame so a fast scan cannot starve rendering.
    for _ in 0..2048 {
        match rx.try_recv() {
            Ok(ScanEvent::Progress { dirs, .. }) => app.dirs = dirs,
            Ok(ScanEvent::Hit(h)) => {
                app.hits.push(*h);
                app.discovered.push(Instant::now());
                app.dirty = true;
            }
            Ok(ScanEvent::Done { dirs, elapsed_ms }) => {
                app.dirs = dirs;
                app.scanning = false;
                app.scan_elapsed = Some(Duration::from_millis(elapsed_ms));
                app.dirty = true;
            }
            Err(_) => break,
        }
    }
}

/// Advance whatever asynchronous work the current mode is waiting on.
fn pump_mode(app: &mut App) {
    let mut finished: Option<Mode> = None;
    match &mut app.mode {
        Mode::Details { rows, rx, .. } => {
            if rows.is_none() {
                if let Ok(r) = rx.try_recv() {
                    *rows = Some(r);
                }
            }
        }
        Mode::Deleting {
            total: _,
            done,
            current,
            reclaimed,
            failures,
            rx,
            removed,
        } => {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    DeleteEvent::Start(name) => *current = name,
                    DeleteEvent::Finished(path, outcome) => {
                        *done += 1;
                        if outcome.is_ok() {
                            *reclaimed += outcome.bytes();
                            removed.push(path);
                        } else {
                            failures.push(format!(
                                "{}: {}",
                                shorten_home(&path),
                                outcome.describe()
                            ));
                        }
                    }
                    DeleteEvent::AllDone => {
                        finished = Some(Mode::Summary {
                            reclaimed: *reclaimed,
                            count: removed.len(),
                            failures: std::mem::take(failures),
                            since: Instant::now(),
                        });
                    }
                }
            }
            if finished.is_some() {
                let gone: HashSet<PathBuf> = removed.iter().cloned().collect();
                app.hits.retain(|h| !gone.contains(&h.path));
                app.discovered.truncate(app.hits.len());
                app.selected.clear();
                app.pops.clear();
                app.dirty = true;
            }
        }
        _ => {}
    }
    if let Some(m) = finished {
        app.mode = m;
    }
}

fn tick_animations(app: &mut App) {
    let now = Instant::now();
    let dt = now.duration_since(app.last_tick).as_secs_f64().min(0.25);
    app.last_tick = now;
    app.spinner =
        ((now.duration_since(app.started).as_millis() / 90) as usize) % theme::SPINNER.len();

    let total = app.visible_bytes() as f64;
    let sel = app.selected_bytes() as f64;
    app.anim.total = ease(app.anim.total, total, dt, 7.0);
    app.anim.selected = ease(app.anim.selected, sel, dt, 10.0);
    app.anim.count = ease(app.anim.count, app.view.len() as f64, dt, 9.0);

    for (kind, bytes, _) in scan::totals_by_kind(&app.hits) {
        let e = app.anim.per_kind.entry(kind).or_insert(0.0);
        *e = ease(*e, bytes as f64, dt, 6.0);
    }

    if let Mode::Deleting { done, total, .. } = &app.mode {
        let target = if *total == 0 {
            1.0
        } else {
            *done as f64 / *total as f64
        };
        app.anim.delete_ratio = ease(app.anim.delete_ratio, target, dt, 12.0);
    } else {
        app.anim.delete_ratio = 0.0;
    }

    if let Some((_, at, _)) = &app.status {
        if at.elapsed() > Duration::from_millis(2600) {
            app.status = None;
        }
    }
    app.pops
        .retain(|_, at| at.elapsed().as_millis() < POP_MS + 40);
}

fn handle_key(app: &mut App, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        app.quit = true;
        return;
    }
    match &app.mode {
        Mode::Table => handle_table_key(app, key),
        Mode::Details { .. } => {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char(' ')
            ) {
                app.mode = Mode::Table;
            }
        }
        Mode::Confirm { .. } => match key.code {
            KeyCode::Char('y') | KeyCode::Enter | KeyCode::Char('d') => start_delete(app),
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                app.mode = Mode::Table;
                app.note("cancelled", false);
            }
            _ => {}
        },
        Mode::Deleting { .. } => {}
        Mode::Summary { .. } => {
            app.mode = Mode::Table;
        }
    }
}

fn handle_table_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Down | KeyCode::Char('j') => app.move_cursor(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_cursor(-1),
        KeyCode::PageDown => app.move_cursor(app.rows_visible as isize),
        KeyCode::PageUp => app.move_cursor(-(app.rows_visible as isize)),
        KeyCode::Home | KeyCode::Char('g') => app.cursor = 0,
        KeyCode::End | KeyCode::Char('G') => app.cursor = app.view.len().saturating_sub(1),
        KeyCode::Char(' ') => app.toggle_selection(),
        KeyCode::Char('a') => app.select_all_visible(),
        KeyCode::Char('s') => {
            app.sort = app.sort.next();
            app.dirty = true;
            app.note(format!("sorted by {}", app.sort.label()), false);
        }
        KeyCode::Char('f') => {
            app.cycle_kind();
            let label = app
                .kind_filter
                .clone()
                .unwrap_or_else(|| "all kinds".into());
            app.note(format!("filter: {label}"), false);
        }
        KeyCode::Char('o') => {
            app.age_step = (app.age_step + 1) % AGE_STEPS.len();
            app.dirty = true;
            app.note(format!("age filter: {}", AGE_STEPS[app.age_step].0), false);
        }
        KeyCode::Enter => open_details(app),
        KeyCode::Char('d') => {
            if app.selected.is_empty() {
                app.note("nothing selected — press space to pick rows", true);
            } else {
                app.mode = Mode::Confirm {
                    since: Instant::now(),
                };
            }
        }
        _ => {}
    }
}

fn open_details(app: &mut App) {
    let Some(i) = app.view.get(app.cursor).copied() else {
        return;
    };
    let path = app.hits[i].path.clone();
    let (tx, rx) = unbounded();
    std::thread::spawn(move || {
        let _ = tx.send(scan::top_level_breakdown(&path, 14));
    });
    app.mode = Mode::Details {
        hit: i,
        rows: None,
        rx,
        since: Instant::now(),
    };
}

fn start_delete(app: &mut App) {
    let targets = app.selected_hits();
    let roots = app.roots.clone();
    let permanent = app.permanent;
    let (tx, rx) = unbounded();
    let total = targets.len();
    std::thread::spawn(move || {
        for (_, hit) in targets {
            let _ = tx.send(DeleteEvent::Start(shorten_home(&hit.path)));
            let outcome = remove::delete_hit(&hit, &roots, permanent);
            let _ = tx.send(DeleteEvent::Finished(hit.path.clone(), outcome));
        }
        let _ = tx.send(DeleteEvent::AllDone);
    });
    app.anim.delete_ratio = 0.0;
    app.mode = Mode::Deleting {
        total,
        done: 0,
        current: String::new(),
        reclaimed: 0,
        failures: Vec::new(),
        rx,
        removed: Vec::new(),
    };
}

// ---------------------------------------------------------------- rendering

fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let kinds = scan::totals_by_kind(&app.hits);
    let bars = kinds.len().clamp(1, 4);
    let header_h = (bars + 2) as u16;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_h),
            Constraint::Min(4),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(f, app, chunks[0], &kinds);
    draw_table(f, app, chunks[1]);
    draw_totals(f, app, chunks[2]);
    draw_hints(f, app, chunks[3]);

    match &app.mode {
        Mode::Details { .. } => draw_details(f, app, area),
        Mode::Confirm { .. } => draw_confirm(f, app, area),
        Mode::Deleting { .. } => draw_deleting(f, app, area),
        Mode::Summary { .. } => draw_summary(f, app, area),
        Mode::Table => {}
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect, kinds: &[(String, u64, usize)]) {
    let th = &app.theme;
    let elapsed = app
        .scan_elapsed
        .unwrap_or_else(|| app.started.elapsed())
        .as_secs_f64();
    let rate = if elapsed > 0.05 {
        app.dirs as f64 / elapsed
    } else {
        0.0
    };

    let mut title: Vec<Span> = vec![
        Span::styled(
            " sweep ",
            Style::default()
                .fg(th.c(theme::BRAND))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("· reclaim dev disk ", Style::default().fg(th.c(theme::DIM))),
    ];
    if app.permanent {
        title.push(Span::styled(
            " PERMANENT ",
            Style::default()
                .fg(th.c(theme::DANGER))
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ));
        title.push(Span::raw(" "));
    }

    let status: Vec<Span> = if app.scanning {
        vec![
            Span::styled(
                format!(" {} ", theme::SPINNER[app.spinner]),
                Style::default().fg(th.c(theme::BRAND)),
            ),
            Span::styled("scanning ", Style::default().fg(th.c(theme::TEXT))),
            Span::styled(
                format!("{} dirs", human_count(app.dirs)),
                Style::default().fg(th.c(theme::DIM)),
            ),
            Span::styled(
                format!(" · {:.0}/s · {:.1}s ", rate, elapsed),
                Style::default().fg(th.c(theme::FAINT)),
            ),
        ]
    } else {
        vec![
            Span::styled(" ✓ ", Style::default().fg(th.c(theme::OK))),
            Span::styled(
                format!("scanned {} dirs in {:.1}s ", human_count(app.dirs), elapsed),
                Style::default().fg(th.c(theme::DIM)),
            ),
        ]
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.c(theme::FAINT)))
        .title(Line::from(title))
        .title_top(Line::from(status).right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height == 0 {
        return;
    }
    let max = kinds.first().map(|k| k.1).unwrap_or(1).max(1) as f64;
    let label_w = 14usize;
    let value_w = 18usize;
    let bar_w = (inner.width as usize)
        .saturating_sub(label_w + value_w + 2)
        .max(4);

    for (row, (kind, bytes, count)) in kinds.iter().take(inner.height as usize).enumerate() {
        let shown = app
            .anim
            .per_kind
            .get(kind)
            .copied()
            .unwrap_or(*bytes as f64);
        let frac = shown / max;
        let filled = theme::bar(frac, bar_w);
        let pad = bar_w.saturating_sub(filled.chars().count());
        let color = th.kind(kind);
        let line = Line::from(vec![
            Span::styled(
                format!("{:<label_w$}", elide_middle(kind, label_w - 1)),
                Style::default().fg(color),
            ),
            Span::styled(filled, Style::default().fg(color)),
            Span::styled("·".repeat(pad), Style::default().fg(th.c(theme::FAINT))),
            Span::styled(
                format!(" {:>9}", human_bytes(shown as u64)),
                Style::default()
                    .fg(th.c(theme::TEXT))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" ({count})"), Style::default().fg(th.c(theme::DIM))),
        ]);
        let r = Rect {
            x: inner.x,
            y: inner.y + row as u16,
            width: inner.width,
            height: 1,
        };
        f.render_widget(Paragraph::new(line), r);
    }

    if kinds.is_empty() && inner.height > 0 {
        let msg = if app.scanning {
            "looking for build artifacts…"
        } else {
            "nothing reclaimable found — your disk is already clean"
        };
        f.render_widget(
            Paragraph::new(Span::styled(msg, Style::default().fg(th.c(theme::DIM)))),
            Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 1,
            },
        );
    }
}

fn segmented(th: &Theme, label: &str, value: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(format!(" {label} "), Style::default().fg(th.c(theme::DIM))),
        Span::styled(
            format!("{value} "),
            Style::default()
                .fg(th.c(theme::BRAND))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("▸ ", Style::default().fg(th.c(theme::FAINT))),
    ]
}

fn draw_table(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;
    let mut title: Vec<Span> = vec![
        Span::styled(
            format!(" {} artifacts ", app.view.len()),
            Style::default()
                .fg(th.c(theme::TEXT))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("│", Style::default().fg(th.c(theme::FAINT))),
    ];
    title.extend(segmented(&th, "sort", app.sort.label()));
    title.push(Span::styled("│", Style::default().fg(th.c(theme::FAINT))));
    title.extend(segmented(
        &th,
        "kind",
        app.kind_filter.as_deref().unwrap_or("all"),
    ));
    title.push(Span::styled("│", Style::default().fg(th.c(theme::FAINT))));
    title.extend(segmented(&th, "older", AGE_STEPS[app.age_step].0));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.c(theme::FAINT)))
        .title(Line::from(title));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 2 || inner.width < 30 {
        return;
    }
    let rows_visible = (inner.height - 1) as usize;
    app.rows_visible = rows_visible;
    if app.cursor < app.offset {
        app.offset = app.cursor;
    } else if app.cursor >= app.offset + rows_visible {
        app.offset = app.cursor + 1 - rows_visible;
    }
    if app.offset + rows_visible > app.view.len() {
        app.offset = app.view.len().saturating_sub(rows_visible);
    }

    const FIXED: usize = 1 + 2 + 14 + 10 + 9 + 12 + 6;
    let path_w = (inner.width as usize).saturating_sub(FIXED).max(12);

    let header = Row::new(vec![
        Cell::from(""),
        Cell::from(""),
        Cell::from(Span::styled("KIND", Style::default().fg(th.c(theme::DIM)))),
        Cell::from(Span::styled("PATH", Style::default().fg(th.c(theme::DIM)))),
        Cell::from(
            Line::from(Span::styled("SIZE", Style::default().fg(th.c(theme::DIM)))).right_aligned(),
        ),
        Cell::from(
            Line::from(Span::styled("FILES", Style::default().fg(th.c(theme::DIM))))
                .right_aligned(),
        ),
        Cell::from(
            Line::from(Span::styled(
                "LAST USED",
                Style::default().fg(th.c(theme::DIM)),
            ))
            .right_aligned(),
        ),
    ]);

    let mut rows: Vec<Row> = Vec::with_capacity(rows_visible);
    for (n, vi) in app
        .view
        .iter()
        .enumerate()
        .skip(app.offset)
        .take(rows_visible)
    {
        let i = *vi;
        let hit = &app.hits[i];
        let is_cursor = n == app.cursor;
        let is_sel = app.selected.contains(&i);
        let flash = app
            .discovered
            .get(i)
            .map(|t| t.elapsed().as_millis())
            .unwrap_or(u128::MAX);
        let pop = app.pops.get(&i).map(|t| t.elapsed().as_millis());

        let bg = if is_cursor {
            th.c(theme::ROW_SEL)
        } else if flash < FLASH_MS {
            // Fade the discovery highlight out over its lifetime.
            let t = flash as f32 / FLASH_MS as f32;
            th.blend(theme::FLASH, theme::ZEBRA, t)
        } else if !n.is_multiple_of(2) {
            th.c(theme::ZEBRA)
        } else {
            ratatui::style::Color::Reset
        };

        let accent = if is_cursor {
            Span::styled("▌", Style::default().fg(th.c(theme::BRAND)))
        } else if is_sel {
            Span::styled("▌", Style::default().fg(th.c(theme::OK)))
        } else {
            Span::raw(" ")
        };

        let check = match (is_sel, pop) {
            (true, Some(ms)) if ms < POP_MS => Span::styled(
                "●",
                Style::default()
                    .fg(th.c(theme::OK))
                    .add_modifier(Modifier::BOLD),
            ),
            (true, _) => Span::styled("✓", Style::default().fg(th.c(theme::OK))),
            (false, Some(ms)) if ms < POP_MS => {
                Span::styled("·", Style::default().fg(th.c(theme::DIM)))
            }
            (false, _) => Span::styled("·", Style::default().fg(th.c(theme::FAINT))),
        };

        let kind_color = th.kind(&hit.kind);
        let size_style = if hit.size_bytes >= 1024 * 1024 * 1024 {
            Style::default()
                .fg(th.c(theme::WARN))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(th.c(theme::TEXT))
        };
        let age_color = match hit.age_days() {
            Some(d) if d >= 180 => th.c(theme::DANGER),
            Some(d) if d >= 90 => th.c(theme::WARN),
            Some(d) if d >= 30 => th.c(theme::TEXT),
            _ => th.c(theme::DIM),
        };

        let row = Row::new(vec![
            Cell::from(accent),
            Cell::from(Line::from(check).centered()),
            Cell::from(Span::styled(
                format!(" {:<12}", elide_middle(&hit.kind, 12)),
                Style::default().fg(kind_color),
            )),
            Cell::from(Span::styled(
                elide_middle(&shorten_home(&hit.path), path_w),
                Style::default().fg(th.c(theme::TEXT)),
            )),
            Cell::from(
                Line::from(Span::styled(human_bytes(hit.size_bytes), size_style)).right_aligned(),
            ),
            Cell::from(
                Line::from(Span::styled(
                    human_count(hit.file_count),
                    Style::default().fg(th.c(theme::DIM)),
                ))
                .right_aligned(),
            ),
            Cell::from(
                Line::from(Span::styled(
                    human_ago(hit.last_used),
                    Style::default().fg(age_color),
                ))
                .right_aligned(),
            ),
        ])
        .style(Style::default().bg(bg));
        rows.push(row);
    }

    let widths = [
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(14),
        Constraint::Length(path_w as u16),
        Constraint::Length(10),
        Constraint::Length(9),
        Constraint::Length(12),
    ];
    let table = Table::new(rows, widths).header(header).column_spacing(1);
    f.render_widget(table, inner);

    if app.view.is_empty() {
        let msg = if app.scanning {
            "scanning…"
        } else {
            "no artifacts match the current filters"
        };
        f.render_widget(
            Paragraph::new(Span::styled(msg, Style::default().fg(th.c(theme::DIM))))
                .alignment(Alignment::Center),
            Rect {
                x: inner.x,
                y: inner.y + inner.height / 2,
                width: inner.width,
                height: 1,
            },
        );
    }
}

fn draw_totals(f: &mut Frame, app: &App, area: Rect) {
    let th = &app.theme;
    let total = app.anim.total as u64;
    let sel_bytes = app.anim.selected as u64;
    let sel_n = app.selected.len();
    let mut spans = vec![
        Span::styled(" Reclaimable ", Style::default().fg(th.c(theme::DIM))),
        Span::styled(
            human_bytes(total),
            Style::default()
                .fg(th.c(theme::OK))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" across {} dirs", app.anim.count.round() as u64),
            Style::default().fg(th.c(theme::DIM)),
        ),
    ];
    if sel_n > 0 {
        spans.push(Span::styled(
            "  ·  ",
            Style::default().fg(th.c(theme::FAINT)),
        ));
        spans.push(Span::styled(
            "selected ",
            Style::default().fg(th.c(theme::DIM)),
        ));
        spans.push(Span::styled(
            human_bytes(sel_bytes),
            Style::default()
                .fg(th.c(theme::BRAND))
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" ({sel_n})"),
            Style::default().fg(th.c(theme::DIM)),
        ));
    }
    if let Some((msg, _, warn)) = &app.status {
        spans.push(Span::styled(
            "  ·  ",
            Style::default().fg(th.c(theme::FAINT)),
        ));
        spans.push(Span::styled(
            msg.clone(),
            Style::default().fg(if *warn {
                th.c(theme::WARN)
            } else {
                th.c(theme::DIM)
            }),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn pill<'a>(th: &Theme, key: &'a str, label: &'a str) -> Vec<Span<'a>> {
    vec![
        Span::styled(
            format!(" {key} "),
            Style::default()
                .fg(th.c(theme::TEXT))
                .bg(th.c(theme::FAINT))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {label}  "), Style::default().fg(th.c(theme::DIM))),
    ]
}

fn draw_hints(f: &mut Frame, app: &App, area: Rect) {
    let th = &app.theme;
    let mut spans = vec![Span::raw(" ")];
    for (k, l) in [
        ("↑↓/jk", "move"),
        ("space", "select"),
        ("a", "all"),
        ("s", "sort"),
        ("f", "kind"),
        ("o", "older"),
        ("↵", "details"),
        ("d", "delete"),
        ("q", "quit"),
    ] {
        spans.extend(pill(th, k, l));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Dim everything already drawn, so a modal reads as being in front.
/// `t` runs 0→1 as the modal animates in, which is a real fade: each cell's
/// foreground is blended toward the faint swatch.
fn dim_backdrop(f: &mut Frame, app: &App, area: Rect, t: f32) {
    let color = app.theme.blend(theme::TEXT, theme::FAINT, t);
    f.buffer_mut().set_style(area, Style::default().fg(color));
}

/// A centred rect that grows to its final size as `t` goes 0→1.
fn modal_rect(area: Rect, w: u16, h: u16, t: f32) -> Rect {
    let t = t.clamp(0.15, 1.0);
    let w = ((w as f32) * (0.85 + 0.15 * t)).round() as u16;
    let h = ((h as f32) * t).round().max(3.0) as u16;
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn ease_in(since: Instant, ms: u128) -> f32 {
    let e = since.elapsed().as_millis().min(ms) as f32 / ms as f32;
    // ease-out cubic
    1.0 - (1.0 - e).powi(3)
}

fn draw_details(f: &mut Frame, app: &App, area: Rect) {
    let Mode::Details {
        hit, rows, since, ..
    } = &app.mode
    else {
        return;
    };
    let th = &app.theme;
    let t = ease_in(*since, 160);
    dim_backdrop(f, app, area, t);
    let hit = &app.hits[*hit];
    // Grow to fit the breakdown rather than leaving a half-empty box.
    let content_h = rows.as_ref().map(|r| r.len()).unwrap_or(1).clamp(1, 14) + 7;
    let rect = modal_rect(
        area,
        area.width.saturating_sub(10).min(96),
        content_h as u16,
        t,
    );
    f.render_widget(Clear, rect);
    let inner_w = rect.width.saturating_sub(4) as usize;

    let rule = rules::rule_for_kind(&hit.kind);
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(
                format!(" {} ", hit.kind),
                Style::default()
                    .fg(th.kind(&hit.kind))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                rule.map(|r| r.description).unwrap_or(""),
                Style::default().fg(th.c(theme::DIM)),
            ),
        ]),
        Line::from(Span::styled(
            format!(" {}", elide_middle(&shorten_home(&hit.path), inner_w)),
            Style::default().fg(th.c(theme::TEXT)),
        )),
        Line::from(vec![
            Span::styled(" size ", Style::default().fg(th.c(theme::DIM))),
            Span::styled(
                human_bytes(hit.size_bytes),
                Style::default()
                    .fg(th.c(theme::OK))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  files ", Style::default().fg(th.c(theme::DIM))),
            Span::styled(
                human_count(hit.file_count),
                Style::default().fg(th.c(theme::TEXT)),
            ),
            Span::styled(
                "  project last used ",
                Style::default().fg(th.c(theme::DIM)),
            ),
            Span::styled(
                human_ago(hit.last_used),
                Style::default().fg(th.c(theme::WARN)),
            ),
        ]),
        Line::from(""),
    ];

    match rows {
        None => lines.push(Line::from(Span::styled(
            format!(" {} measuring contents…", theme::SPINNER[app.spinner]),
            Style::default().fg(th.c(theme::DIM)),
        ))),
        Some(rows) if rows.is_empty() => lines.push(Line::from(Span::styled(
            " (empty)",
            Style::default().fg(th.c(theme::DIM)),
        ))),
        Some(rows) => {
            let max = rows.first().map(|r| r.1).unwrap_or(1).max(1) as f64;
            let bar_w = (rect.width as usize).saturating_sub(46).max(6);
            for (name, size) in rows {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!(" {:<28}", elide_middle(name, 27)),
                        Style::default().fg(th.c(theme::TEXT)),
                    ),
                    Span::styled(
                        format!("{:>9} ", human_bytes(*size)),
                        Style::default().fg(th.c(theme::DIM)),
                    ),
                    Span::styled(
                        theme::bar(*size as f64 / max, bar_w),
                        Style::default().fg(th.kind(&hit.kind)),
                    ),
                ]));
            }
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.kind(&hit.kind)))
        .title(Span::styled(
            " details ",
            Style::default()
                .fg(th.c(theme::TEXT))
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(
            " esc to close ",
            Style::default().fg(th.c(theme::DIM)),
        ));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn draw_confirm(f: &mut Frame, app: &App, area: Rect) {
    let Mode::Confirm { since } = &app.mode else {
        return;
    };
    let th = &app.theme;
    let t = ease_in(*since, 150);
    dim_backdrop(f, app, area, t);

    let targets = app.selected_hits();
    let bytes: u64 = targets.iter().map(|(_, h)| h.size_bytes).sum();
    let list_n = targets.len().min(6);
    let rect = modal_rect(area, 78, (10 + list_n) as u16, t);
    f.render_widget(Clear, rect);

    let (verb, color, note) = if app.permanent {
        (
            "PERMANENTLY DELETE",
            th.c(theme::DANGER),
            "This cannot be undone. Files will NOT go to the Trash.",
        )
    } else {
        (
            "Move to Trash",
            th.c(theme::BRAND),
            "Recoverable: everything goes to the system Trash.",
        )
    };

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(
                format!(" {verb} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} directories", targets.len()),
                Style::default().fg(th.c(theme::TEXT)),
            ),
            Span::styled(" totalling ", Style::default().fg(th.c(theme::DIM))),
            Span::styled(
                human_bytes(bytes),
                Style::default()
                    .fg(th.c(theme::OK))
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            format!(" {note}"),
            Style::default().fg(if app.permanent {
                th.c(theme::DANGER)
            } else {
                th.c(theme::DIM)
            }),
        )),
        Line::from(""),
    ];
    for (_, h) in targets.iter().take(list_n) {
        lines.push(Line::from(vec![
            Span::styled("   • ", Style::default().fg(th.kind(&h.kind))),
            Span::styled(
                elide_middle(
                    &shorten_home(&h.path),
                    rect.width.saturating_sub(22) as usize,
                ),
                Style::default().fg(th.c(theme::TEXT)),
            ),
            Span::styled(
                format!("  {}", human_bytes(h.size_bytes)),
                Style::default().fg(th.c(theme::DIM)),
            ),
        ]));
    }
    if targets.len() > list_n {
        lines.push(Line::from(Span::styled(
            format!("   … and {} more", targets.len() - list_n),
            Style::default().fg(th.c(theme::DIM)),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(
            " y ",
            Style::default()
                .fg(th.c(theme::TEXT))
                .bg(color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" confirm    ", Style::default().fg(th.c(theme::DIM))),
        Span::styled(
            " n ",
            Style::default()
                .fg(th.c(theme::TEXT))
                .bg(th.c(theme::FAINT))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" cancel", Style::default().fg(th.c(theme::DIM))),
    ]));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .title(Span::styled(
            if app.permanent {
                " danger "
            } else {
                " confirm "
            },
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn draw_deleting(f: &mut Frame, app: &App, area: Rect) {
    let Mode::Deleting {
        total,
        done,
        current,
        reclaimed,
        ..
    } = &app.mode
    else {
        return;
    };
    let th = &app.theme;
    dim_backdrop(f, app, area, 1.0);
    let rect = modal_rect(area, 74, 9, 1.0);
    f.render_widget(Clear, rect);

    let bar_w = rect.width.saturating_sub(6) as usize;
    let ratio = app.anim.delete_ratio;
    let filled = theme::bar(ratio, bar_w);
    let pad = bar_w.saturating_sub(filled.chars().count());

    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!(" {} ", theme::SPINNER[app.spinner]),
                Style::default().fg(th.c(theme::BRAND)),
            ),
            Span::styled(
                if app.permanent {
                    "deleting "
                } else {
                    "moving to Trash "
                },
                Style::default()
                    .fg(th.c(theme::TEXT))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{done}/{total}"),
                Style::default().fg(th.c(theme::DIM)),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(filled, Style::default().fg(th.c(theme::OK))),
            Span::styled("·".repeat(pad), Style::default().fg(th.c(theme::FAINT))),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                elide_middle(current, rect.width.saturating_sub(6) as usize),
                Style::default().fg(th.c(theme::DIM)),
            ),
        ]),
        Line::from(vec![
            Span::styled("  reclaimed ", Style::default().fg(th.c(theme::DIM))),
            Span::styled(
                human_bytes(*reclaimed),
                Style::default()
                    .fg(th.c(theme::OK))
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.c(theme::BRAND)));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn draw_summary(f: &mut Frame, app: &App, area: Rect) {
    let Mode::Summary {
        reclaimed,
        count,
        failures,
        since,
    } = &app.mode
    else {
        return;
    };
    let th = &app.theme;
    let t = ease_in(*since, 220);
    dim_backdrop(f, app, area, 1.0);
    let h = 8 + failures.len().min(4) as u16;
    let rect = modal_rect(area, 66, h, t);
    f.render_widget(Clear, rect);

    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  ✓  Reclaimed {}", human_bytes(*reclaimed)),
            Style::default()
                .fg(th.c(theme::OK))
                .add_modifier(Modifier::BOLD),
        ))
        .centered(),
        Line::from(Span::styled(
            format!(
                "{count} directories {}",
                if app.permanent {
                    "deleted"
                } else {
                    "moved to Trash"
                }
            ),
            Style::default().fg(th.c(theme::DIM)),
        ))
        .centered(),
    ];
    if !failures.is_empty() {
        lines.push(Line::from(""));
        for fmsg in failures.iter().take(4) {
            lines.push(Line::from(Span::styled(
                format!(
                    " ! {}",
                    elide_middle(fmsg, rect.width.saturating_sub(6) as usize)
                ),
                Style::default().fg(th.c(theme::WARN)),
            )));
        }
    }
    lines.push(Line::from(""));
    lines.push(
        Line::from(Span::styled(
            "press any key",
            Style::default().fg(th.c(theme::FAINT)),
        ))
        .centered(),
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.c(theme::OK)));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn hit(kind: &str, size: u64, days: i64, path: &str) -> Hit {
        Hit {
            path: PathBuf::from(path),
            kind: kind.to_string(),
            size_bytes: size,
            file_count: 10,
            last_used: Some(crate::util::now_secs() - days * 86400),
            project_root: PathBuf::from("/tmp"),
        }
    }

    fn app_with(hits: Vec<Hit>) -> App {
        let mut app = App::new(
            Theme { truecolor: true },
            false,
            vec![PathBuf::from("/tmp")],
        );
        for h in hits {
            app.hits.push(h);
            app.discovered.push(Instant::now());
        }
        app.rebuild_view();
        app
    }

    #[test]
    fn filters_and_sorts_drive_the_view() {
        let mut app = app_with(vec![
            hit("node_modules", 900, 200, "/tmp/a/node_modules"),
            hit("cargo_target", 100, 2, "/tmp/b/target"),
            hit("venv", 500, 120, "/tmp/c/.venv"),
        ]);
        assert_eq!(app.view.len(), 3);
        assert_eq!(app.hits[app.view[0]].size_bytes, 900);

        app.age_step = 2; // 90d+
        app.rebuild_view();
        assert_eq!(app.view.len(), 2, "the fresh cargo target is filtered out");

        app.age_step = 0;
        app.kind_filter = Some("venv".into());
        app.rebuild_view();
        assert_eq!(app.view.len(), 1);
        assert_eq!(app.hits[app.view[0]].kind, "venv");

        app.kind_filter = None;
        app.sort = SortKey::LastUsed;
        app.rebuild_view();
        assert_eq!(app.hits[app.view[0]].kind, "node_modules", "oldest first");
    }

    #[test]
    fn cycling_kinds_walks_every_kind_then_returns_to_all() {
        let mut app = app_with(vec![
            hit("node_modules", 900, 10, "/tmp/a/node_modules"),
            hit("venv", 500, 10, "/tmp/c/.venv"),
        ]);
        assert!(app.kind_filter.is_none());
        app.cycle_kind();
        assert_eq!(app.kind_filter.as_deref(), Some("node_modules"));
        app.cycle_kind();
        assert_eq!(app.kind_filter.as_deref(), Some("venv"));
        app.cycle_kind();
        assert!(app.kind_filter.is_none());
    }

    #[test]
    fn selection_totals_track_the_selected_rows() {
        let mut app = app_with(vec![
            hit("node_modules", 900, 10, "/tmp/a/node_modules"),
            hit("venv", 500, 10, "/tmp/c/.venv"),
        ]);
        assert_eq!(app.selected_bytes(), 0);
        app.toggle_selection();
        assert_eq!(app.selected_bytes(), 900);
        app.move_cursor(1);
        app.toggle_selection();
        assert_eq!(app.selected_bytes(), 1400);
        app.toggle_selection();
        assert_eq!(app.selected_bytes(), 900);
        app.select_all_visible();
        assert_eq!(app.selected.len(), 2);
        app.select_all_visible();
        assert!(
            app.selected.is_empty(),
            "a second press clears the selection"
        );
    }

    #[test]
    fn cursor_stays_in_bounds() {
        let mut app = app_with(vec![hit("venv", 1, 1, "/tmp/c/.venv")]);
        app.move_cursor(-5);
        assert_eq!(app.cursor, 0);
        app.move_cursor(50);
        assert_eq!(app.cursor, 0);
        let mut empty = app_with(vec![]);
        empty.move_cursor(1);
        assert_eq!(empty.cursor, 0);
        empty.toggle_selection();
        assert!(empty.selected.is_empty());
    }

    #[test]
    fn easing_converges_and_never_overshoots() {
        let mut v = 0.0;
        for _ in 0..200 {
            v = ease(v, 1000.0, 0.033, 7.0);
            assert!(v <= 1000.0);
        }
        assert_eq!(v, 1000.0);
    }

    #[test]
    fn modal_rect_fits_inside_the_screen() {
        let area = Rect::new(0, 0, 80, 24);
        for t in [0.0f32, 0.3, 1.0] {
            let r = modal_rect(area, 78, 20, t);
            assert!(r.x + r.width <= area.width, "t={t}");
            assert!(r.y + r.height <= area.height, "t={t}");
        }
        // Tiny terminals must not produce an out-of-bounds rect.
        let tiny = Rect::new(0, 0, 20, 6);
        let r = modal_rect(tiny, 78, 20, 1.0);
        assert!(r.x + r.width <= tiny.width);
        assert!(r.y + r.height <= tiny.height);
    }
}
