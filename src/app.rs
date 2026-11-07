//! The panel: state, keyboard/mouse input, and drawing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::git::{self, FileDiff, Job, Msg, RowKind, Snapshot, Staged};
use crate::theme::{self, Palette};
use crate::tree::{self, Item, TreeRow};
use crate::wrap::{wrap_row, WrapCache};

const CONTEXT: u32 = 3;
const FULL_CONTEXT: u32 = 1_000_000;
/// Wait after a change on disk before reloading (coalesces bursts of events).
const FS_DEBOUNCE: Duration = Duration::from_millis(120);
/// Safety reload, in case a filesystem event gets lost.
const FALLBACK_REFRESH: Duration = Duration::from_secs(10);
const THEME_EVERY: Duration = Duration::from_secs(4);

#[derive(Default, Clone, Copy)]
struct Areas {
    side: bool,
    tree: Rect,
    /// Stacked: the hr row (it and the blank rows around it drag the divider).
    /// Side by side: the divider column.
    splitter: u16,
    /// File title, right above the diff.
    title: Rect,
    diff: Rect,
}

/// Stacked layout fixed rows: header, rule, margin, hr, margin, title, footer.
const CHROME_ROWS: u16 = 7;
/// From this width on (a full-screen or zoomed pane) the tree goes on the
/// left and the diff on the right.
const SIDE_MIN_WIDTH: u16 = 160;

struct App {
    jobs: Sender<Job>,
    pal: Palette,
    detected: Option<theme::Detected>,

    snapshot: Option<Result<Snapshot, String>>,
    collapsed: HashSet<String>,
    rows: Vec<TreeRow>,
    cursor: usize,
    tree_off: usize,
    tree_pref: Option<u16>,
    tree_pref_w: Option<u16>,

    active: Option<String>,
    diff: Option<FileDiff>,
    /// Scroll position in visual lines (after wrapping).
    diff_scroll: usize,
    diff_gen: u64,
    wrap: WrapCache,
    full_context: bool,

    areas: Areas,
    dragging: bool,
    /// A short message in the footer (errors, mostly) and when it goes away.
    flash: Option<(String, Instant)>,
    quit: bool,
}

pub fn run(cwd: PathBuf) -> std::io::Result<()> {
    let (job_tx, job_rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    {
        let cwd = cwd.clone();
        std::thread::spawn(move || git::run_worker(cwd, job_rx, msg_tx));
    }
    let _ = job_tx.send(Job::Refresh);

    let (fs_tx, fs_rx) = mpsc::channel();
    let _watcher = watch(&cwd, fs_tx);

    let mut terminal = ratatui::init();
    log_panics();
    let detected = theme::query(Duration::from_millis(400));
    execute!(std::io::stdout(), EnableMouseCapture)?;

    let mut app = App {
        jobs: job_tx,
        pal: Palette::from(detected),
        detected,
        snapshot: None,
        collapsed: HashSet::new(),
        rows: Vec::new(),
        cursor: 0,
        tree_off: 0,
        tree_pref: None,
        tree_pref_w: None,
        active: None,
        diff: None,
        diff_scroll: 0,
        diff_gen: 0,
        wrap: WrapCache::default(),
        full_context: false,
        areas: Areas::default(),
        dragging: false,
        flash: None,
        quit: false,
    };

    let res = app.event_loop(&mut terminal, &msg_rx, &fs_rx);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    res
}

/// Writes panics to `~/.local/state/rexdiff/panic.log`: the pane closes along
/// with the program, so the message on screen would be lost.
fn log_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let path = crate::state_dir().join("panic.log");
        let bt = std::backtrace::Backtrace::force_capture();
        let _ = std::fs::create_dir_all(crate::state_dir());
        let _ = std::fs::write(&path, format!("{info}\n\n{bt}\n"));
        previous(info);
    }));
}

/// Watches the repository folder and signals when something relevant changes.
fn watch(cwd: &Path, tx: Sender<()>) -> Option<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};
    let repo = git2::Repository::discover(cwd).ok()?;
    let root = repo.workdir()?.to_path_buf();
    let root_cb = root.clone();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        let relevant = ev.paths.iter().any(|p| {
            let Ok(rel) = p.strip_prefix(&root_cb) else { return true };
            let mut comps = rel.components();
            if comps.next().is_some_and(|c| c.as_os_str() == ".git") {
                // Inside .git only the index, HEAD, and refs matter.
                let rest = comps.as_path();
                return rest == Path::new("index") || rest == Path::new("HEAD") || rest.starts_with("refs");
            }
            !repo.is_path_ignored(rel).unwrap_or(false)
        });
        if relevant {
            let _ = tx.send(());
        }
    })
    .ok()?;
    w.watch(&root, RecursiveMode::Recursive).ok()?;
    Some(w)
}

impl App {
    fn event_loop(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        msgs: &Receiver<Msg>,
        fs: &Receiver<()>,
    ) -> std::io::Result<()> {
        let mut dirty = true;
        let mut refresh_due: Option<Instant> = None;
        let mut last_refresh = Instant::now();
        let mut last_theme = Instant::now();
        let mut last_input = Instant::now();

        while !self.quit {
            if dirty {
                terminal.draw(|f| self.draw(f))?;
                dirty = false;
            }

            if event::poll(Duration::from_millis(40))? {
                loop {
                    match event::read()? {
                        Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
                        Event::Mouse(m) => self.on_mouse(m),
                        _ => {}
                    }
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
                last_input = Instant::now();
                dirty = true;
            }

            for msg in msgs.try_iter() {
                self.on_msg(msg);
                dirty = true;
            }
            if self.flash.as_ref().is_some_and(|(_, until)| Instant::now() >= *until) {
                self.flash = None;
                dirty = true;
            }

            if fs.try_iter().count() > 0 && refresh_due.is_none() {
                refresh_due = Some(Instant::now() + FS_DEBOUNCE);
            }
            let now = Instant::now();
            if refresh_due.is_some_and(|t| now >= t) || now - last_refresh >= FALLBACK_REFRESH {
                let _ = self.jobs.send(Job::Refresh);
                refresh_due = None;
                last_refresh = now;
            }

            // Every so often, check whether the Rex theme changed. Only while the
            // panel is idle, so a keypress doesn't get swallowed mid-reply.
            if now - last_theme >= THEME_EVERY && now - last_input >= Duration::from_secs(2) {
                last_theme = now;
                if let Some(d) = theme::query(Duration::from_millis(200))
                    && Some(d) != self.detected
                {
                    self.detected = Some(d);
                    self.pal = Palette::from(Some(d));
                    terminal.clear()?;
                    dirty = true;
                }
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- data

    fn files(&self) -> &[git::FileEntry] {
        match &self.snapshot {
            Some(Ok(s)) => &s.files,
            _ => &[],
        }
    }

    fn context(&self) -> u32 {
        if self.full_context { FULL_CONTEXT } else { CONTEXT }
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Snapshot(snap) => {
                let cursor_key = self.rows.get(self.cursor).map(|r| r.key(self.files()).to_string());
                self.snapshot = Some(snap);
                self.rebuild(cursor_key.as_deref());

                let files = self.files();
                let still_there = self.active.as_ref().is_some_and(|a| files.iter().any(|f| &f.path == a));
                if !still_there {
                    self.active = None;
                    self.diff = None;
                    // Start on the first file in the tree.
                    if let Some(i) = self.rows.iter().position(|r| matches!(r.item, Item::File { .. })) {
                        self.cursor = i;
                    }
                    self.sync_active_from_cursor();
                } else {
                    // The content may have changed: request the diff again, keeping the scroll.
                    self.request_diff();
                }
            }
            Msg::Error(e) => self.flash = Some((e, Instant::now() + Duration::from_secs(6))),
            Msg::Diff(d) => {
                if Some(&d.path) == self.active.as_ref() && d.context == self.context() {
                    let same_file = self.diff.as_ref().is_some_and(|old| old.path == d.path);
                    if !same_file {
                        self.diff_scroll = 0;
                    }
                    self.diff = Some(d);
                    self.diff_gen += 1;
                }
            }
        }
    }

    fn rebuild(&mut self, cursor_key: Option<&str>) {
        let rows = tree::build(self.files(), &self.collapsed);
        let pos = cursor_key.and_then(|k| rows.iter().position(|r| r.key(self.files()) == k));
        self.rows = rows;
        self.cursor = pos.unwrap_or(self.cursor).min(self.rows.len().saturating_sub(1));
    }

    fn request_diff(&self) {
        if let Some(path) = &self.active {
            let _ = self.jobs.send(Job::Diff { path: path.clone(), context: self.context() });
        }
    }

    /// With the cursor on a file, show it; on a folder, show its first file
    /// (in tree order, even when the folder is collapsed).
    fn sync_active_from_cursor(&mut self) {
        let path = match self.rows.get(self.cursor).map(|r| &r.item) {
            Some(Item::File { idx, .. }) => self.files()[*idx].path.clone(),
            Some(Item::Dir { path, .. }) => match self.first_file_in(path) {
                Some(p) => p,
                None => return,
            },
            None => return,
        };
        self.set_active(path);
    }

    fn first_file_in(&self, dir: &str) -> Option<String> {
        let files = self.files();
        let prefix = format!("{dir}/");
        tree::build(files, &HashSet::new()).iter().find_map(|r| match r.item {
            Item::File { idx, .. } if files[idx].path.starts_with(&prefix) => Some(files[idx].path.clone()),
            _ => None,
        })
    }

    fn set_active(&mut self, path: String) {
        if self.active.as_ref() != Some(&path) {
            self.active = Some(path);
            self.request_diff();
        }
    }

    // ---------------------------------------------------------------- input

    /// One row up or down the tree, through folders and files.
    fn move_row(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let max = self.rows.len() as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, max) as usize;
        self.sync_active_from_cursor();
    }

    /// Moves to the next (or previous) folder. An open folder puts the cursor
    /// on its first visible entry (its first file, or a collapsed subfolder);
    /// a collapsed folder keeps the cursor on the folder itself.
    fn step_folder(&mut self, forward: bool) {
        let candidates: Box<dyn Iterator<Item = usize>> = if forward {
            Box::new(self.cursor + 1..self.rows.len())
        } else {
            Box::new((0..self.cursor).rev())
        };
        for i in candidates {
            if !matches!(self.rows[i].item, Item::Dir { .. }) {
                continue;
            }
            let land = self.landing(i);
            // Skip folders whose landing spot is where we already are (or behind us),
            // e.g. the folder that holds the file the cursor is on.
            if (forward && land > self.cursor) || (!forward && land < self.cursor) {
                self.cursor = land;
                self.sync_active_from_cursor();
                return;
            }
        }
    }

    /// Where the cursor lands for the folder at `row`: down through open folders
    /// until it reaches a file or a collapsed folder.
    fn landing(&self, mut row: usize) -> usize {
        while let Some(Item::Dir { collapsed: false, .. }) = self.rows.get(row).map(|r| &r.item) {
            if row + 1 >= self.rows.len() {
                break;
            }
            row += 1;
        }
        row
    }

    /// Next file, wrapping around (Tab).
    fn jump_file(&mut self, forward: bool) {
        let n = self.rows.len();
        for step in 1..=n {
            let i = if forward { (self.cursor + step) % n } else { (self.cursor + n - step) % n };
            if matches!(self.rows[i].item, Item::File { .. }) {
                self.cursor = i;
                self.sync_active_from_cursor();
                return;
            }
        }
    }

    fn toggle_dir(&mut self, row: usize) {
        let Some(TreeRow { item: Item::Dir { path, .. }, .. }) = self.rows.get(row) else { return };
        let path = path.clone();
        if !self.collapsed.remove(&path) {
            self.collapsed.insert(path.clone());
        }
        self.rebuild(Some(&path));
    }

    /// Space: on a folder, open/close it; on a file, stage it (or unstage it
    /// if it's fully staged), to mark it as reviewed.
    fn toggle_here(&mut self) {
        match self.rows.get(self.cursor).map(|r| &r.item) {
            Some(Item::Dir { .. }) => self.toggle_dir(self.cursor),
            Some(Item::File { idx, .. }) => self.toggle_stage(*idx),
            None => {}
        }
    }

    fn toggle_stage(&mut self, idx: usize) {
        let Some(f) = self.files().get(idx) else { return };
        let add = f.staged != Staged::Fully;
        let _ = self.jobs.send(Job::Stage { path: f.path.clone(), add });
    }

    fn scroll_diff(&mut self, delta: isize) {
        let max = self.wrap.total().saturating_sub(self.areas.diff.height as usize);
        self.diff_scroll = (self.diff_scroll as isize + delta).clamp(0, max as isize) as usize;
    }

    fn on_key(&mut self, k: KeyEvent) {
        let page = self.areas.diff.height.max(2) as isize - 1;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('d') if ctrl => self.scroll_diff(page / 2),
            KeyCode::Char('u') if ctrl => self.scroll_diff(-page / 2),
            KeyCode::Up | KeyCode::Char('k') => self.move_row(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_row(1),
            KeyCode::Left | KeyCode::Char('h') => self.step_folder(false),
            KeyCode::Right | KeyCode::Char('l') => self.step_folder(true),
            KeyCode::Char(' ') | KeyCode::Enter => self.toggle_here(),
            KeyCode::Tab => self.jump_file(true),
            KeyCode::BackTab => self.jump_file(false),
            KeyCode::Char('J') => self.scroll_diff(1),
            KeyCode::Char('K') => self.scroll_diff(-1),
            KeyCode::Char('f') | KeyCode::PageDown => self.scroll_diff(page),
            KeyCode::Char('b') | KeyCode::PageUp => self.scroll_diff(-page),
            KeyCode::Char('g') | KeyCode::Home => self.diff_scroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.scroll_diff(isize::MAX / 2),
            KeyCode::Char('x') => {
                self.full_context = !self.full_context;
                self.diff = None;
                self.request_diff();
            }
            KeyCode::Char('+') | KeyCode::Char('=') if self.areas.side => {
                self.tree_pref_w = Some(self.areas.tree.width + 2);
            }
            KeyCode::Char('-') if self.areas.side => {
                self.tree_pref_w = Some(self.areas.tree.width.saturating_sub(2));
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.tree_pref = Some(self.areas.tree.height + 1);
            }
            KeyCode::Char('-') => {
                self.tree_pref = Some(self.areas.tree.height.saturating_sub(1).max(1));
            }
            KeyCode::Char('r') => {
                let _ = self.jobs.send(Job::Refresh);
            }
            _ => {}
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        let a = self.areas;
        let inside = |r: Rect| m.column >= r.x && m.column < r.x + r.width && m.row >= r.y && m.row < r.y + r.height;
        let in_tree = inside(a.tree);
        let on_splitter = if a.side {
            m.column + 1 >= a.splitter && m.column <= a.splitter + 1 && m.row >= a.tree.y
        } else {
            a.splitter > 0 && m.row + 1 >= a.splitter && m.row <= a.splitter + 1
        };
        match m.kind {
            MouseEventKind::ScrollDown if in_tree => {
                let max = self.rows.len().saturating_sub(a.tree.height as usize);
                self.tree_off = (self.tree_off + 1).min(max);
            }
            MouseEventKind::ScrollUp if in_tree => self.tree_off = self.tree_off.saturating_sub(1),
            MouseEventKind::ScrollDown => self.scroll_diff(3),
            MouseEventKind::ScrollUp => self.scroll_diff(-3),
            MouseEventKind::Down(MouseButton::Left) if on_splitter => self.dragging = true,
            MouseEventKind::Down(MouseButton::Left) if in_tree => {
                let row = self.tree_off + (m.row - a.tree.y) as usize;
                if row < self.rows.len() {
                    self.cursor = row;
                    match self.rows[row].item {
                        Item::Dir { .. } => self.toggle_dir(row),
                        Item::File { idx, .. } => {
                            self.sync_active_from_cursor();
                            // A click right on the checkbox stages/unstages.
                            let checkbox = a.tree.x + 1 + self.rows[row].guide.width() as u16;
                            if m.column == checkbox {
                                self.toggle_stage(idx);
                            }
                        }
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging && a.side => {
                // The divider sits one column after the end of the tree.
                self.tree_pref_w = Some(m.column.saturating_sub(a.tree.x + 1));
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                // The pointer sits on the hr, one row below the end of the tree.
                self.tree_pref = Some(m.row.saturating_sub(a.tree.y + 1).max(1));
            }
            MouseEventKind::Up(MouseButton::Left) => self.dragging = false,
            _ => {}
        }
    }

    // ---------------------------------------------------------------- drawing

    fn layout(&mut self, area: Rect) {
        self.areas = if area.width >= SIDE_MIN_WIDTH { self.side_layout(area) } else { self.stacked_layout(area) };
        let tree_h = self.areas.tree.height;

        // Keep the cursor visible in the tree.
        let h = tree_h as usize;
        if self.cursor < self.tree_off {
            self.tree_off = self.cursor;
        } else if h > 0 && self.cursor >= self.tree_off + h {
            self.tree_off = self.cursor + 1 - h;
        }
        self.tree_off = self.tree_off.min(self.rows.len().saturating_sub(h));

        // Wrap the diff to the pane width; only redone when the diff or the width changes.
        match self.diff.as_ref().filter(|d| Some(&d.path) == self.active.as_ref()) {
            Some(d) => {
                let g = Geom::new(d, self.areas.diff);
                self.wrap.rebuild(&d.rows, g.text_w, self.diff_gen);
            }
            None => self.wrap = WrapCache::default(),
        }
        self.scroll_diff(0);
    }

    /// Tree on top, diff below.
    fn stacked_layout(&self, area: Rect) -> Areas {
        // Header, rule, [tree], margin, hr, margin, title, [diff], footer.
        let avail = area.height.saturating_sub(CHROME_ROWS);
        let want = self.rows.len() as u16;
        // The tree takes up to ~32% of the free rows (the rest goes to the diff).
        let auto = want.min((avail * 8 / 25).max(3));
        let max_tree = avail.saturating_sub(3).max(1);
        let tree_h = self.tree_pref.unwrap_or(auto).clamp(1, max_tree).min(avail);
        let tree = Rect::new(area.x, area.y + 2, area.width, tree_h);
        let splitter = tree.y + tree_h + 1;
        let title = Rect::new(area.x, splitter + 2, area.width, 1);
        let diff_h = area.height.saturating_sub(tree_h + CHROME_ROWS);
        let diff = Rect::new(area.x, title.y + 1, area.width, diff_h);
        Areas { side: false, tree, splitter, title, diff }
    }

    /// Tree on the left, a vertical divider, diff on the right.
    fn side_layout(&self, area: Rect) -> Areas {
        // Header, rule, [tree | divider | title + diff], footer.
        let body_y = area.y + 2;
        let body_h = area.height.saturating_sub(3);
        // As wide as the longest row needs, within 28 columns and 35% of the pane.
        let files = self.files();
        let needed = self
            .rows
            .iter()
            .map(|r| {
                let (label, adds, dels) = match &r.item {
                    Item::Dir { label, adds, dels, .. } => (label.width() + 3, *adds, *dels),
                    Item::File { idx, name } => (name.width() + 2, files[*idx].adds, files[*idx].dels),
                };
                let counts: usize = counts_text(adds, dels).iter().map(|(s, _)| s.width()).sum();
                (1 + r.guide.width() + label + 3 + counts + 1) as u16
            })
            .max()
            .unwrap_or(0);
        let auto = needed.clamp(28, (area.width * 35 / 100).max(28));
        let tree_w = self.tree_pref_w.unwrap_or(auto).clamp(20, area.width / 2);
        let tree = Rect::new(area.x, body_y, tree_w, body_h);
        let splitter = area.x + tree_w + 1;
        let dx = splitter + 2;
        let dw = (area.x + area.width).saturating_sub(dx);
        let title = Rect::new(dx, body_y, dw, 1);
        let diff = Rect::new(dx, body_y + 1, dw, body_h.saturating_sub(1));
        Areas { side: true, tree, splitter, title, diff }
    }

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        if area.width < 10 || area.height < 6 {
            return;
        }
        self.layout(area);
        let buf = f.buffer_mut();

        let files = self.files();
        let p = &self.pal;
        let snapshot = match &self.snapshot {
            None => return center(buf, area, "Reading the repository…", p.faint),
            Some(Err(e)) => return center(buf, area, e, p.muted),
            Some(Ok(s)) => s,
        };

        self.draw_header(buf, area, snapshot);
        hline(buf, area.x, area.y + 1, area.width, p.rule);

        if files.is_empty() {
            let rest = Rect::new(area.x, area.y + 2, area.width, area.height - 2);
            center(buf, rest, "No changes ✓  working tree clean", p.faint);
            return;
        }

        self.draw_tree(buf);
        self.draw_splitter(buf, area);
        self.draw_diff(buf);
        self.draw_footer(buf, area);
    }

    fn draw_header(&self, buf: &mut Buffer, area: Rect, s: &Snapshot) {
        let p = &self.pal;
        let y = area.y;
        let (adds, dels) = s.files.iter().fold((0, 0), |(a, d), f| (a + f.adds, d + f.dels));
        let right = area.x + area.width - 1;
        let totals_w = format!("+{adds} −{dels}").width() as u16;
        let rx = right.saturating_sub(totals_w);
        let mut x = rx;
        put(buf, &mut x, y, right, &format!("+{adds}"), Style::new().fg(p.green));
        put(buf, &mut x, y, right, &format!(" −{dels}"), Style::new().fg(p.red));

        let lim = rx.saturating_sub(1);
        let mut x = area.x + 1;
        put(buf, &mut x, y, lim, "Changes", Style::new().add_modifier(Modifier::BOLD));
        x += 1;
        let n = s.files.len();
        let pill = format!(" {n} {} ", if n == 1 { "file" } else { "files" });
        put(buf, &mut x, y, lim, &pill, Style::new().fg(p.muted).bg(p.pill_bg));
        put(buf, &mut x, y, lim, &format!("  ⎇ {}", s.branch), Style::new().fg(p.muted));
        let done = s.files.iter().filter(|f| f.staged == Staged::Fully).count();
        let st = Style::new().fg(if done == n { p.green } else { p.muted });
        put(buf, &mut x, y, lim, &format!("   ✓ {done}/{n} staged"), st);
    }

    fn draw_tree(&self, buf: &mut Buffer) {
        let p = &self.pal;
        let files = self.files();
        let a = self.areas.tree;
        for (i, row) in self.rows.iter().enumerate().skip(self.tree_off).take(a.height as usize) {
            let y = a.y + (i - self.tree_off) as u16;
            let selected = i == self.cursor;
            let base = if selected { Style::new().bg(p.sel_bg) } else { Style::new() };
            buf.set_style(Rect::new(a.x, y, a.width, 1), base);

            let right = a.x + a.width - 1;
            let (adds, dels, faint_counts) = match &row.item {
                Item::Dir { adds, dels, .. } => (*adds, *dels, true),
                Item::File { idx, .. } => (files[*idx].adds, files[*idx].dels, false),
            };
            let counts = counts_text(adds, dels);
            let cw: u16 = counts.iter().map(|(s, _)| s.width() as u16).sum();
            let mut x = right.saturating_sub(cw);
            let lim = x.saturating_sub(1);
            for (s, green) in &counts {
                let c = match (faint_counts, green) {
                    (true, _) => p.faint,
                    (false, true) => p.green,
                    (false, false) => p.red,
                };
                put(buf, &mut x, y, right, s, base.fg(c));
            }

            let mut x = a.x + 1;
            put(buf, &mut x, y, lim, &row.guide, base.fg(p.rule));
            match &row.item {
                Item::Dir { label, collapsed, all_staged, .. } => {
                    put(buf, &mut x, y, lim, if *collapsed { "▸ " } else { "▾ " }, base.fg(p.faint));
                    put(buf, &mut x, y, lim, label, base.fg(p.muted));
                    put(buf, &mut x, y, lim, "/", base.fg(p.faint));
                    if *all_staged {
                        put(buf, &mut x, y, lim, " ✓", base.fg(p.green));
                    }
                }
                Item::File { idx, name } => {
                    let f = &files[*idx];
                    let active = self.active.as_deref() == Some(f.path.as_str());
                    if active {
                        buf.set_string(a.x, y, "▎", base.fg(p.blue));
                    }
                    let sc = match f.status {
                        'A' => p.green,
                        'D' => p.red,
                        'M' => p.yellow,
                        _ => p.blue,
                    };
                    // The checkbox: staged = reviewed.
                    let (mark, mark_fg) = match f.staged {
                        Staged::Fully => ("✓", p.green),
                        Staged::Partly => ("◐", p.yellow),
                        Staged::No => ("○", p.faint),
                    };
                    put(buf, &mut x, y, lim, mark, base.fg(mark_fg).add_modifier(Modifier::BOLD));
                    x += 1;
                    put(buf, &mut x, y, lim, &f.status.to_string(), base.fg(sc).add_modifier(Modifier::BOLD));
                    x += 1;
                    let mut name_style = base;
                    if f.staged == Staged::Fully {
                        name_style = name_style.fg(p.muted);
                    }
                    if active {
                        name_style = name_style.add_modifier(Modifier::BOLD);
                    }
                    if f.status == 'D' {
                        name_style = name_style.fg(p.muted).add_modifier(Modifier::CROSSED_OUT);
                    }
                    put(buf, &mut x, y, lim, name, name_style);
                    if f.binary {
                        put(buf, &mut x, y, lim, "  binary", base.fg(p.faint));
                    }
                }
            }
        }
    }

    fn draw_splitter(&self, buf: &mut Buffer, area: Rect) {
        let p = &self.pal;
        let color = if self.dragging { p.blue } else { p.rule };
        if self.areas.side {
            // A vertical line between the tree and the diff.
            let t = self.areas.tree;
            for y in t.y..t.y + t.height {
                buf.set_string(self.areas.splitter, y, "│", Style::new().fg(color));
            }
        } else {
            // The hr, inset one column on each side.
            hline(buf, area.x + 1, self.areas.splitter, area.width.saturating_sub(2), color);
        }

        // File title: path on the left, position in the diff on the right.
        let t = self.areas.title;
        let y = t.y;
        let right = t.x + t.width - 1;
        let mut lim = right;
        // Only once the active file's diff is wrapped (a new one may still be loading).
        if let Some(d) = self.diff.as_ref().filter(|d| Some(&d.path) == self.active.as_ref())
            && self.wrap.total() > 0
        {
            let total = d.rows.len();
            let visual = self.wrap.total();
            let (top, _) = self.wrap.locate(self.diff_scroll.min(visual.saturating_sub(1)));
            let bottom_v = (self.diff_scroll + self.areas.diff.height as usize).min(visual).saturating_sub(1);
            let (bottom, _) = self.wrap.locate(bottom_v);
            let first = (top + 1).min(total);
            let last = (bottom + 1).min(total);
            let mut info = format!("{first}–{last} of {total}");
            if self.full_context {
                info = format!("whole file · {info}");
            }
            let w = info.width() as u16;
            let mut x = right.saturating_sub(w);
            lim = x.saturating_sub(1);
            put(buf, &mut x, y, right, &info, Style::new().fg(p.faint));
        }
        if let Some(path) = &self.active {
            let (dir, name) = match path.rsplit_once('/') {
                Some((d, n)) => (format!("{d}/"), n),
                None => (String::new(), path.as_str()),
            };
            let mut x = t.x + 1;
            put(buf, &mut x, y, lim, &dir, Style::new().fg(p.muted));
            put(buf, &mut x, y, lim, name, Style::new().add_modifier(Modifier::BOLD));
        }
    }

    fn draw_diff(&self, buf: &mut Buffer) {
        let p = &self.pal;
        let a = self.areas.diff;
        let Some(d) = self.diff.as_ref().filter(|d| Some(&d.path) == self.active.as_ref()) else {
            center(buf, a, "Loading…", p.faint);
            return;
        };
        let g = Geom::new(d, a);
        let gw = g.num_w as usize;
        let text_x = a.x + g.text_off;
        let right = text_x + g.text_w;

        for line in 0..a.height as usize {
            let v = self.diff_scroll + line;
            if v >= self.wrap.total() {
                break;
            }
            let (r, sub) = self.wrap.locate(v);
            let row = &d.rows[r];
            let y = a.y + line as u16;
            let (row_bg, gutter_bg, emph_bg, sign, sign_fg) = match row.kind {
                RowKind::Add => (Some(p.add_bg), Some(p.add_gutter), Some(p.add_emph), "+", p.green),
                RowKind::Del => (Some(p.del_bg), Some(p.del_gutter), Some(p.del_emph), "−", p.red),
                RowKind::Hunk => (Some(p.hunk_bg), Some(p.hunk_bg), None, " ", p.faint),
                _ => (None, None, None, " ", p.faint),
            };
            let base = row_bg.map_or(Style::new(), |c| Style::new().bg(c));
            buf.set_style(Rect::new(a.x, y, a.width, 1), base);
            let gutter = gutter_bg.map_or(Style::new(), |c| Style::new().bg(c));
            buf.set_style(Rect::new(a.x, y, g.gutter_w, 1), gutter);

            // Number and sign only on the first visual line; continuations stay clean.
            if sub == 0 {
                let num = match row.kind {
                    RowKind::Del => row.old,
                    _ => row.new,
                };
                let num_txt = match (row.kind, num) {
                    (RowKind::Hunk, _) => format!("{:>gw$}", "⋯"),
                    (_, Some(n)) => format!("{n:>gw$}"),
                    _ => String::new(),
                };
                let num_fg = if matches!(row.kind, RowKind::Add | RowKind::Del) { p.muted } else { p.faint };
                buf.set_stringn(a.x + 1, y, &num_txt, gw, gutter.fg(num_fg));
                buf.set_string(a.x + g.gutter_w, y, sign, base.fg(sign_fg).add_modifier(Modifier::BOLD));
            }

            match row.kind {
                RowKind::Hunk => {
                    let (hdr, func) = split_hunk(&row.segs[0].text);
                    let mut x = text_x;
                    put(buf, &mut x, y, right, hdr, base.fg(p.faint));
                    put(buf, &mut x, y, right, func, base.fg(p.muted));
                }
                RowKind::Meta => {
                    let mut x = text_x;
                    let st = base.fg(p.faint).add_modifier(Modifier::ITALIC);
                    put(buf, &mut x, y, right, &row.segs[0].text, st);
                }
                _ => {
                    let w = wrap_row(row, g.text_w as usize);
                    let from = w.starts[sub];
                    let to = w.starts.get(sub + 1).copied().unwrap_or(usize::MAX);
                    let mut x = text_x + if sub > 0 { w.indent as u16 } else { 0 };
                    let mut at = 0; // char index at the start of the segment
                    for seg in &row.segs {
                        let n = seg.text.chars().count();
                        let (lo, hi) = (from.max(at), to.min(at + n));
                        if lo < hi {
                            let piece: String = seg.text.chars().skip(lo - at).take(hi - lo).collect();
                            let mut st = base.fg(seg.fg.map_or(p.fg, Color::Indexed));
                            if seg.bold {
                                st = st.add_modifier(Modifier::BOLD);
                            }
                            if seg.emph
                                && let Some(bg) = emph_bg
                            {
                                st = st.bg(bg);
                            }
                            put(buf, &mut x, y, right, &piece, st);
                        }
                        at += n;
                        if at >= to {
                            break;
                        }
                    }
                }
            }
        }
    }

    fn draw_footer(&self, buf: &mut Buffer, area: Rect) {
        let p = &self.pal;
        let y = area.y + area.height - 1;
        let hints = [
            ("↑↓", "navigate"),
            ("←→", "folders"),
            ("space", "stage/fold"),
            ("x", "whole file"),
            ("q", "quit"),
        ];
        let mut x = area.x + 1;
        let right = area.x + area.width;
        if let Some((msg, until)) = &self.flash
            && Instant::now() < *until
        {
            put(buf, &mut x, y, right, msg, Style::new().fg(p.red));
            return;
        }
        for (key, what) in hints {
            put(buf, &mut x, y, right, key, Style::new().fg(p.muted));
            put(buf, &mut x, y, right, &format!(" {what}   "), Style::new().fg(p.faint));
        }
    }
}

/// Diff columns: line number, sign, and text.
struct Geom {
    num_w: u16,
    gutter_w: u16,
    text_off: u16,
    text_w: u16,
}

impl Geom {
    fn new(d: &FileDiff, area: Rect) -> Self {
        let num_w = digits(d.max_lineno).max(2) as u16;
        let gutter_w = num_w + 2;
        let text_off = gutter_w + 2;
        // One free column on the right, so text doesn't touch the edge.
        let text_w = area.width.saturating_sub(text_off + 1).max(1);
        Self { num_w, gutter_w, text_off, text_w }
    }
}

// ---------------------------------------------------------------- helpers

fn put(buf: &mut Buffer, x: &mut u16, y: u16, max_x: u16, s: &str, style: Style) {
    if *x >= max_x || s.is_empty() {
        return;
    }
    let (nx, _) = buf.set_stringn(*x, y, s, (max_x - *x) as usize, style);
    *x = nx;
}

fn hline(buf: &mut Buffer, x: u16, y: u16, w: u16, color: Color) {
    buf.set_string(x, y, "─".repeat(w as usize), Style::new().fg(color));
}

fn center(buf: &mut Buffer, area: Rect, text: &str, color: Color) {
    let w = (text.width() as u16).min(area.width);
    let x = area.x + (area.width - w) / 2;
    let y = area.y + area.height / 2;
    buf.set_stringn(x, y, text, w as usize, Style::new().fg(color));
}

fn counts_text(adds: usize, dels: usize) -> Vec<(String, bool)> {
    let mut v = Vec::new();
    if adds > 0 {
        v.push((format!("+{adds}"), true));
    }
    if dels > 0 {
        let sep = if adds > 0 { " " } else { "" };
        v.push((format!("{sep}−{dels}"), false));
    }
    v
}

fn digits(n: u32) -> usize {
    n.max(1).to_string().len()
}

/// `@@ -1,3 +1,4 @@ fn foo` → (`@@ -1,3 +1,4 @@`, ` fn foo`).
fn split_hunk(h: &str) -> (&str, &str) {
    match h.get(2..).and_then(|r| r.find("@@")) {
        Some(i) => h.split_at(i + 4),
        None => (h, ""),
    }
}
