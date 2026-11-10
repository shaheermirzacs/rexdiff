//! The panel: state, keyboard/mouse input, and drawing.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    KeyboardEnhancementFlags, PushKeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::annotations::{self, Agent, Position};
use crate::git::{self, BranchInfo, CommitInfo, FileDiff, Job, Msg, RowKind, Snapshot, Staged};
use crate::keymap::{self, Action, Key, Scope};
use crate::reader::{Motion, Reader};
use crate::theme::{self, Palette};
use crate::tree::{self, Item, TreeRow};

const CONTEXT: u32 = 3;
pub(crate) const FULL_CONTEXT: u32 = 1_000_000;
/// Wait after a change on disk before reloading (coalesces bursts of events).
const FS_DEBOUNCE: Duration = Duration::from_millis(120);
/// Safety reload, in case a filesystem event gets lost.
const FALLBACK_REFRESH: Duration = Duration::from_secs(10);
const THEME_EVERY: Duration = Duration::from_secs(4);

#[derive(Default, Clone, Copy)]
struct Areas {
    side: bool,
    tree_frame: Rect,
    file_frame: Rect,
    tree: Rect,
    /// Stacked: the hr row (it and the blank rows around it drag the divider).
    /// Side by side: the divider column.
    splitter: u16,
    /// File title, right above the diff.
    title: Rect,
    diff: Rect,
    /// The notes dock, while open.
    notes: Rect,
}

/// Stacked layout fixed rows: header, rule, margin, hr, margin, title, footer.
const CHROME_ROWS: u16 = 7;
/// From this width on (a full-screen or zoomed pane) the tree goes on the
/// left and the diff on the right.
const SIDE_MIN_WIDTH: u16 = 160;

/// The two screens: changed files with their diff, and the local branches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Changes,
    Branches,
    /// The commit history of one branch, opened from the Branches screen.
    History,
}

/// The review pane that receives keys on the Changes screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Tree,
    Reader,
    Notes,
}

/// Where the reader goes once the requested file and revision load. Applied
/// once, by cached and async loads alike; selecting another file cancels it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PendingSeek {
    FirstHunk,
    Source { line: u32, old_side: bool },
    Annotation { note_id: u64 },
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Pending {
    path: String,
    revision: u64,
    seek: PendingSeek,
}

struct App {
    reader: Reader,
    root: PathBuf,
    picker: Option<crate::project::Picker>,
    inventory: Option<crate::project::Browser>,
    other_collapsed: HashSet<String>,
    catalog: Vec<git::FileEntry>,
    all_files: bool,
    sidebar: bool,
    notes_open: bool,
    note_sel: usize,
    /// The pane focus goes back to when the notes close.
    notes_return: Focus,
    editing_note: Option<usize>,
    /// The command line (`:…`) or search prompt (`/…`), while open.
    command: Option<String>,
    /// Tab completion: the typed prefix and the candidate shown.
    completion: Option<(String, usize)>,
    /// The help overlay and its scroll offset.
    help: Option<usize>,
    /// A count typed in the reader (`12j`).
    count: Option<usize>,
    /// `Space` or `z` waiting for its second key.
    prefix: Option<Action>,
    focus: Focus,
    pending: Option<Pending>,
    notes: Vec<annotations::Note>,
    next_note: u64,
    document_revision: u64,
    cached: Vec<(u64, FileDiff)>,
    note_box: Option<(String, String)>,
    note_cursor: usize,
    draft_note: Option<annotations::Note>,
    agent_picker: Option<(Vec<Agent>, usize)>,
    annotation_rx: Option<Receiver<AnnotationResult>>,
    jobs: Sender<Job>,
    view: View,
    /// Column ranges of the "Changes" and "Branches" tabs in the header, for clicks.
    tabs: Cell<[(u16, u16); 2]>,

    branches: Option<Result<Vec<BranchInfo>, String>>,
    branch_cursor: usize,
    /// First visible branch row (updated while drawing).
    branch_off: Cell<usize>,

    hist_branch: String,
    history: Option<Result<Vec<CommitInfo>, String>>,
    comparison: Option<git::Comparison>,
    review_request: Option<u64>,
    review_sequence: u64,
    review_base: String,
    hist_cursor: usize,
    hist_off: Cell<usize>,
    pal: Palette,
    detected: Option<theme::Detected>,

    snapshot: Option<Result<Snapshot, String>>,
    collapsed: HashSet<String>,
    rows: Vec<TreeRow>,
    cursor: usize,
    tree_off: usize,
    tree_scrolling: bool,
    tree_pref: Option<u16>,
    tree_pref_w: Option<u16>,

    active: Option<String>,
    diff: Option<FileDiff>,
    diff_gen: u64,
    full_context: bool,

    areas: Areas,
    dragging: bool,
    /// A short message in the footer (errors, mostly) and when it goes away.
    flash: Option<Flash>,
    /// The commit message being typed (`c`), while the box is open.
    commit_box: Option<String>,
    /// A discard (`d`) waiting for `y`.
    confirm: Option<Confirm>,
    /// A commit in progress, and what it's doing.
    busy: Option<String>,
    quit: bool,
}

enum AnnotationResult {
    Agents(Result<Vec<Agent>, String>),
    Sent(Result<(), String>),
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
    execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste,
        // Event types let held keys arrive as repeats, so only movement repeats.
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES | KeyboardEnhancementFlags::REPORT_EVENT_TYPES))?;

    let mut app = App::new(job_tx, cwd, detected);

    let res = app.event_loop(&mut terminal, &msg_rx, &fs_rx);
    let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags, DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    res
}

impl App {
    fn toggle_diff_layout(&mut self) {
        self.reader.toggle_split();
    }

    /// The active document, once the reader shows it.
    fn doc(&self) -> Option<&FileDiff> {
        self.diff.as_ref().filter(|d| Some(&d.path) == self.active.as_ref() && self.reader.loaded(self.diff_gen))
    }

    /// `x`: the reader keeps its source row across the reload.
    fn toggle_context(&mut self) {
        self.full_context = !self.full_context;
        let revision = self.active.as_ref().map(|p| self.revision(p));
        if let Some((_, d)) = self.cached.iter().find(|(rev,d)| Some(*rev) == revision && Some(&d.path) == self.active.as_ref() && d.context == self.context()) {
            let d = d.clone();
            self.install(d);
        } else { self.request_diff(); }
    }

    fn new(jobs: Sender<Job>, cwd: PathBuf, detected: Option<theme::Detected>) -> Self {
        Self {
            reader: Reader::default(),
            picker: None,
            inventory: None,
            other_collapsed: HashSet::new(),
            catalog: Vec::new(),
            all_files: false,
            sidebar: true,
            notes_open: false,
            note_sel: 0,
            notes_return: Focus::Tree,
            editing_note: None,
            command: None,
            completion: None,
            help: None,
            count: None,
            prefix: None,
            root: git2::Repository::discover(&cwd).ok().and_then(|r| r.workdir().map(Path::to_path_buf)).unwrap_or(cwd),
            focus: Focus::Tree,
            pending: None,
            notes: Vec::new(),
            next_note: 1,
            document_revision: 0,
            cached: Vec::new(),
            note_box: None,
            note_cursor: 0,
            draft_note: None,
            agent_picker: None,
            annotation_rx: None,
            jobs,
            view: View::Changes,
            tabs: Cell::new([(0, 0); 2]),
            branches: None,
            branch_cursor: 0,
            branch_off: Cell::new(0),
            hist_branch: String::new(),
            history: None,
            comparison: None,
            review_request: None,
            review_sequence: 0,
            review_base: String::new(),
            hist_cursor: 0,
            hist_off: Cell::new(0),
            pal: Palette::from(detected),
            detected,
            snapshot: None,
            collapsed: HashSet::new(),
            rows: Vec::new(),
            cursor: 0,
            tree_off: 0,
            tree_scrolling: false,
            tree_pref: None,
            tree_pref_w: None,
            active: None,
            diff: None,
            diff_gen: 0,
            full_context: false,
            areas: Areas::default(),
            dragging: false,
            flash: None,
            commit_box: None,
            confirm: None,
            busy: None,
            quit: false,
        }
    }
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
            if let Some(picker) = &mut self.picker { picker.poll(); dirty = true; }
            if let Some(scan) = &mut self.inventory && scan.poll() {
                    let key = self.rows.get(self.cursor).map(|r| r.key(self.files()).to_string());
                    self.update_catalog();
                    self.rebuild(key.as_deref());
                    dirty = true;
            }
            if dirty {
                terminal.draw(|f| self.draw(f))?;
                dirty = false;
            }

            if event::poll(Duration::from_millis(40))? {
                loop {
                    match event::read()? {
                        Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
                        Event::Mouse(m) => self.on_mouse(m),
                        Event::Paste(text) => {
                            if let Some(picker) = self.picker.as_mut() {
                                picker.query.push_str(&text.replace(['\n','\r','\x1b'], " ")); picker.update();
                            } else if let Some(command) = self.command.as_mut() {
                                command.push_str(&text.replace(['\n','\r','\x1b'], " "));
                            } else if let Some((_, msg)) = self.note_box.as_mut() {
                                let text = text.replace("\r\n", "\n").replace('\r', "\n").replace('\x1b', "");
                                msg.insert_str(self.note_cursor, &text);
                                self.note_cursor += text.len();
                            } else if let Some(msg) = self.commit_box.as_mut() {
                                msg.push_str(&text.replace(['\n', '\r'], " "));
                            }
                        }
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
            if let Some(result) = self.annotation_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
                self.annotation_rx = None;
                match result {
                    AnnotationResult::Agents(Ok(mut agents)) if agents.len() == 1 => self.send_notes(agents.remove(0)),
                    AnnotationResult::Agents(Ok(agents)) if !agents.is_empty() => self.agent_picker = Some((agents, 0)),
                    AnnotationResult::Agents(Ok(_)) => {
                        self.flash = Some(Flash::new("No agents found in this tab; annotations kept".into(), false))
                    }
                    AnnotationResult::Sent(Ok(())) => {
                        self.notes.clear();
                        self.flash =
                            Some(Flash::new("Annotations pasted into agent input (not submitted)".into(), true));
                    }
                    AnnotationResult::Agents(Err(e)) | AnnotationResult::Sent(Err(e)) => {
                        self.flash = Some(Flash::new(e, false))
                    }
                }
                dirty = true;
            }
            if self.flash.as_ref().is_some_and(|f| Instant::now() >= f.until) {
                self.flash = None;
                dirty = true;
            }

            if fs.try_iter().count() > 0 && refresh_due.is_none() {
                refresh_due = Some(Instant::now() + FS_DEBOUNCE);
            }
            let now = Instant::now();
            if refresh_due.is_some_and(|t| now >= t) || now - last_refresh >= FALLBACK_REFRESH {
                if let Some(scan) = &self.inventory { scan.refresh(); }
                let _ = self.jobs.send(Job::Refresh);
                match self.view {
                    View::Branches => {
                        let _ = self.jobs.send(Job::Branches);
                    }
                    View::History => {
                        let _ = self.jobs.send(Job::History { branch: self.hist_branch.clone() });
                    }
                    View::Changes => {}
                }
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
        if self.all_files { return &self.catalog; }
        match &self.snapshot {
            Some(Ok(s)) => &s.files,
            _ => &[],
        }
    }

    fn context(&self) -> u32 {
        if self.full_context { FULL_CONTEXT } else { CONTEXT }
    }

    fn open_path(&mut self, path: &str) {
        if self.comparison.is_some() { self.flash = Some(Flash::new("Press W for working-tree files".into(), false)); return; }
        match crate::project::contained(&self.root, path) {
            Ok(file) => {
                let path = file.strip_prefix(&self.root).unwrap_or(&file).to_string_lossy().into_owned();
                self.activate(path);
            }
            Err(e) => self.flash = Some(Flash::new(e, false)),
        }
    }

    fn update_catalog(&mut self) {
        self.catalog = self.snapshot.as_ref().and_then(|s| s.as_ref().ok()).map(|s| s.files.clone()).unwrap_or_default();
        if let Some(scan) = &self.inventory {
            let mut known: HashSet<String> = self.catalog.iter().map(|f| f.path.clone()).collect();
            for entry in scan.entries.values().flatten() {
                if known.insert(entry.path.clone()) {
                    if entry.status == '\0' && !scan.entries.contains_key(&entry.path) {
                        let expanded = self.catalog.iter().any(|f| f.status != ' ' && f.status != '\0' && f.path.starts_with(&format!("{}/",entry.path)));
                        if !expanded { if self.all_files { self.collapsed.insert(entry.path.clone()); } else { self.other_collapsed.insert(entry.path.clone()); } }
                    }
                    self.catalog.push(entry.clone());
                }
            }
        }
        self.catalog.sort_by(|a,b| a.path.cmp(&b.path));
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Review { request, result } => {
                if self.review_request != Some(request) { return; }
                self.review_request = None;
                match result {
                    Ok((comparison, snapshot)) => {
                        self.comparison = Some(comparison);
                        self.reset_diff_view();
                        self.apply_snapshot(Ok(snapshot));
                        self.view = View::Changes;
                    }
                    Err(e) => self.flash = Some(Flash::new(e, false)),
                }
            }
            Msg::Snapshot(snap) => {
                if self.comparison.is_none() && self.review_request.is_none() { self.apply_snapshot(snap); }
            }
            Msg::Error(e) => self.flash = Some(Flash::new(e, false)),
            Msg::Branches(list) => {
                // Keep the cursor on the same branch; the first time, on the current one.
                let keep = self.selected_branch().map(|b| b.name.clone());
                let first_load = self.branches.is_none();
                self.branches = Some(list);
                if let Some(Ok(l)) = &self.branches {
                    let pos = if first_load {
                        l.iter().position(|b| b.current)
                    } else {
                        keep.and_then(|n| l.iter().position(|b| b.name == n))
                    };
                    self.branch_cursor = pos.unwrap_or(self.branch_cursor).min(l.len().saturating_sub(1));
                }
            }
            Msg::History { branch, commits } => {
                if branch == self.hist_branch {
                    self.history = Some(commits);
                    let len = self.history_len();
                    self.hist_cursor = self.hist_cursor.min(len.saturating_sub(1));
                }
            }
            Msg::Progress(p) => self.busy = Some(p),
            Msg::Done(result) => {
                self.busy = None;
                self.flash = Some(match result {
                    Ok(summary) => Flash::new(format!("✓ {summary}"), true),
                    Err(e) => Flash::new(e, false),
                });
            }
            Msg::Diff(d) => {
                if Some(&d.path) == self.active.as_ref() && d.context == self.context()
                    && self.review_request.is_none() && d.revision == self.revision(&d.path) {
                    self.install(d);
                }
            }
        }
    }

    fn revision(&self, path: &str) -> u64 {
        self.comparison.as_ref().map_or_else(|| annotations::revision(&self.root, path), git::Comparison::revision)
    }

    fn apply_snapshot(&mut self, snap: Result<Snapshot, String>) {
        let key = self.rows.get(self.cursor).map(|r| r.key(self.files()).to_string());
        self.snapshot = Some(snap);
        self.update_catalog();
        self.rebuild(key.as_deref());
        let still_there = self.active.as_ref().is_some_and(|a| self.files().iter().any(|f| &f.path == a)
            || (self.comparison.is_none() && self.root.join(a).is_file()));
        if !still_there {
            self.active = None;
            self.diff = None;
            if let Some(i) = self.rows.iter().position(|r| matches!(r.item, Item::File { .. })) { self.cursor = i; }
            self.sync_active_from_cursor();
        } else { self.request_diff(); }
    }

    fn reset_diff_view(&mut self) {
        self.active = None;
        self.diff = None;
        self.cached.clear();
        self.pending = None;
        self.cursor = 0;
        self.tree_off = 0;
        self.tree_scrolling = false;
        self.all_files = false;
        self.collapsed.clear();
        self.set_focus(Focus::Tree);
    }

    fn open_review(&mut self, commit: Option<String>) {
        self.review_sequence += 1;
        self.review_request = Some(self.review_sequence);
        let _ = self.jobs.send(Job::Review { request: self.review_sequence, commit, base: self.review_base.clone() });
        self.flash = Some(Flash::new("Loading comparison…".into(), true));
    }

    fn working_tree(&mut self) {
        self.review_request = None;
        self.comparison = None;
        self.reset_diff_view();
        self.snapshot = None;
        self.rows.clear();
        self.view = View::Changes;
        let _ = self.jobs.send(Job::Refresh);
    }

    fn rebuild(&mut self, cursor_key: Option<&str>) {
        let rows = tree::build(self.files(), &self.collapsed);
        let pos = cursor_key.and_then(|k| rows.iter().position(|r| r.key(self.files()) == k));
        self.rows = rows;
        self.cursor = pos.unwrap_or(self.cursor).min(self.rows.len().saturating_sub(1));
    }

    fn request_diff(&self) {
        if let Some(path) = &self.active {
            let job = match &self.comparison {
                Some(comparison) => Job::ReviewDiff { comparison: comparison.clone(), path: path.clone(), context: self.context() },
                None => Job::Diff { path: path.clone(), context: self.context() },
            };
            let _ = self.jobs.send(job);
        }
    }

    /// With the cursor on a file, preview it; on a folder, its first file
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
        self.preview(path);
    }

    fn first_file_in(&self, dir: &str) -> Option<String> {
        let files = self.files();
        let prefix = format!("{dir}/");
        tree::build(files, &HashSet::new()).iter().find_map(|r| match r.item {
            Item::File { idx, .. } if files[idx].path.starts_with(&prefix) => Some(files[idx].path.clone()),
            _ => None,
        })
    }

    /// Loads `path` from the cache or the worker. A pending seek for another
    /// file is cancelled: the newest selection wins.
    fn set_active(&mut self, path: String) {
        if self.pending.as_ref().is_some_and(|p| p.path != path) { self.pending = None; }
        if self.active.as_ref() != Some(&path) {
            let revision = self.revision(&path);
            let cached = self.cached.iter().find(|(rev,d)| *rev == revision && d.path == path && d.context == self.context())
                .map(|(_,d)| d.clone());
            self.active = Some(path);
            match cached {
                Some(diff) => self.install(diff),
                None => self.request_diff(),
            }
        }
    }

    /// Shows a newly loaded document. Refreshes and highlighting keep the
    /// reader's place and focus; only a pending seek moves it.
    fn install(&mut self, d: FileDiff) {
        if self.diff.as_ref() != Some(&d) {
            let revision = self.revision(&d.path);
            self.cached.retain(|(rev, old)| old.path != d.path || (*rev == revision && old.context != d.context));
            self.cached.push((revision, d.clone()));
            if self.cached.len() > 16 { self.cached.remove(0); }
            self.document_revision = revision;
            self.diff_gen += 1;
            self.reader.load(&d, self.diff_gen, None);
            self.diff = Some(d);
        }
        self.apply_pending();
    }

    /// Asks for `path` with `seek`, applied now if it's already showing.
    fn seek_to(&mut self, path: String, seek: PendingSeek) {
        self.pending = Some(Pending { revision: self.revision(&path), path: path.clone(), seek });
        if self.doc().is_some_and(|d| d.path == path) { self.apply_pending(); } else { self.set_active(path); }
    }

    /// Tree movement: show the file without taking focus. A new file opens
    /// at its first hunk; the current one keeps its place.
    fn preview(&mut self, path: String) {
        if self.active.as_ref() == Some(&path) { return; }
        self.seek_to(path, PendingSeek::FirstHunk);
    }

    /// Enter, Tab, clicks, the picker, and `{` `}`: focus the reader at the
    /// first hunk, even when the file is already showing.
    fn activate(&mut self, path: String) {
        self.set_focus(Focus::Reader);
        self.seek_to(path, PendingSeek::FirstHunk);
    }

    fn apply_pending(&mut self) {
        let Some(d) = self.diff.as_ref().filter(|d| Some(&d.path) == self.active.as_ref() && self.reader.loaded(self.diff_gen)) else { return };
        let Some(pending) = self.pending.clone().filter(|p| p.path == d.path) else { return };
        if pending.revision != d.revision { self.pending = None; return; }
        let (row, old_side) = match pending.seek {
            PendingSeek::FirstHunk => {
                self.reader.first_hunk(d);
                self.pending = None;
                return;
            }
            PendingSeek::Source { line, old_side } => (self.reader.source_row(line, old_side), Some(old_side)),
            PendingSeek::Annotation { note_id } => {
                let Some(note) = self.notes.iter().find(|n| n.id == note_id) else { self.pending = None; return };
                let row = match note.locations.first() {
                    Some(location) => d.rows.iter().position(|r| (r.old, r.new) == *location),
                    None => Some(0),
                };
                (row, note.line.map(|_| note.old_side))
            }
        };
        match row {
            Some(row) => {
                self.pending = None;
                self.reader.load(d, self.diff_gen, Some(Position { row, col: 0 }));
                if let Some(old_side) = old_side { self.reader.set_side(d, old_side); }
            }
            // Outside the shown hunks: load the whole file and seek again.
            None if !self.full_context => { self.full_context = true; self.request_diff(); }
            None => {
                self.pending = None;
                if let PendingSeek::Source { line, .. } = pending.seek {
                    self.flash = Some(Flash::new(format!("Line {line} does not exist or exceeds the preview limit"), false));
                }
            }
        }
    }

    fn set_focus(&mut self, focus: Focus) {
        self.focus = focus;
        self.prefix = None;
        self.count = None;
    }

    // ---------------------------------------------------------------- input

    /// One row up or down the tree, through folders and files.
    fn move_row(&mut self, delta: isize) {
        self.tree_scrolling = false;
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
    fn tree_motion(&mut self, forward: bool) {
        self.tree_scrolling = false;
        if let Some(Item::Dir { collapsed, .. }) = self.rows.get(self.cursor).map(|r| &r.item)
            && *collapsed == forward {
            self.toggle_dir(self.cursor); return;
        }
        if forward { self.move_row(1); }
        else if let Some(row) = self.rows.get(self.cursor) {
            let path = row.key(self.files()).to_string();
            if let Some(i) = (0..self.cursor).rev().find(|&i| match &self.rows[i].item {
                Item::Dir { path: parent, .. } => path.starts_with(&format!("{parent}/")), _ => false,
            }) { self.cursor = i; }
        }
    }

    fn toggle_dir(&mut self, row: usize) {
        self.tree_scrolling = false;
        let Some(TreeRow { item: Item::Dir { path, .. }, .. }) = self.rows.get(row) else { return };
        let path = path.clone();
        if !self.collapsed.remove(&path) {
            self.collapsed.insert(path.clone());
        } else if self.all_files && let Some(scan) = &self.inventory {
            scan.request(&path);
        }
        self.rebuild(Some(&path));
    }

    /// `z`: on a folder, open/close it; on a file, stage it (or unstage it
    /// if it's fully staged), to mark it as reviewed.
    fn toggle_here(&mut self) {
        match self.rows.get(self.cursor).map(|r| &r.item) {
            Some(Item::Dir { .. }) => self.toggle_dir(self.cursor),
            Some(Item::File { idx, .. }) => self.toggle_stage(*idx),
            None => {}
        }
    }

    fn toggle_stage(&mut self, idx: usize) {
        if self.comparison.is_some() { return; }
        let Some(f) = self.files().get(idx) else { return };
        if f.status == ' ' { return; }
        let add = f.staged != Staged::Fully;
        let _ = self.jobs.send(Job::Stage { path: f.path.clone(), add });
    }

    /// Runs `f` on the reader with the document it shows.
    fn with_doc<R>(&mut self, f: impl FnOnce(&mut Reader, &FileDiff) -> R) -> Option<R> {
        let d = self.diff.as_ref().filter(|d| Some(&d.path) == self.active.as_ref() && self.reader.loaded(self.diff_gen))?;
        Some(f(&mut self.reader, d))
    }

    /// Tree `J` / `K` and the mouse wheel: scroll the reader without focusing it.
    fn scroll_diff(&mut self, delta: isize) {
        let motion = if delta < 0 { Motion::ScrollUp } else { Motion::ScrollDown };
        self.with_doc(|r, d| r.motion(d, motion, delta.unsigned_abs()));
    }

    fn open_note(&mut self) {
        self.editing_note = None;
        if self.annotation_rx.is_some() {
            return;
        }
        let Some(d) = self.doc() else {
            return;
        };
        let path = self.root.join(&d.path).display().to_string();
        let path = self.comparison.as_ref().map_or(path.clone(), |c| format!("{path} [commit {} vs {}]", c.tip, c.base.map_or("empty tree".into(), |id| id.to_string())));
        let target = format!("{path} (whole file)");
        self.draft_note = Some(annotations::Note {
            id: self.next_note, path: d.path.clone(), line: None, old_side: false,
            locations: Vec::new(),
            revision: self.document_revision, target: target.clone(), comment: String::new(),
            preview: Vec::new(),
            historical: self.comparison.is_some(),
        });
        self.note_box = Some((target, String::new()));
        self.note_cursor = 0;
    }

    fn tree_visible(&self) -> bool {
        self.sidebar && (!self.notes_open || self.areas.side)
    }

    /// `L`: show the notes and focus them; again while focused hides them.
    fn toggle_notes(&mut self) {
        if self.focus == Focus::Notes { return self.close_notes(); }
        self.notes_return = self.focus;
        self.set_focus(Focus::Notes);
        if !self.notes_open {
            self.notes_open = true;
            self.note_sel = self.note_sel.min(self.notes.len().saturating_sub(1));
            self.preview_note();
        }
    }

    fn close_notes(&mut self) {
        self.notes_open = false;
        let back = if self.notes_return == Focus::Tree && !self.sidebar { Focus::Reader } else { self.notes_return };
        self.set_focus(back);
    }

    /// Shows the selected note at its anchor, not at the file's first hunk.
    fn preview_note(&mut self) {
        let Some(note) = self.notes.get(self.note_sel) else { return };
        if self.revision(&note.path) != note.revision { return; }
        let seek = PendingSeek::Annotation { note_id: note.id };
        self.seek_to(note.path.clone(), seek);
    }

    fn on_note_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some((_, msg)) = self.note_box.as_mut() else {
            return;
        };
        match k.code {
            KeyCode::Esc => self.note_box = None,
            KeyCode::Char('c') if ctrl => self.note_box = None,
            KeyCode::Enter if k.modifiers.contains(KeyModifiers::SHIFT) => {
                msg.insert(self.note_cursor, '\n');
                self.note_cursor += 1;
            }
            KeyCode::Enter if !msg.trim().is_empty() => {
                let (_, msg) = self.note_box.take().unwrap();
                if let Some(i) = self.editing_note.take() { self.notes[i].comment = msg.trim().into(); }
                else {
                    let Some(mut note) = self.draft_note.take() else { return };
                    note.comment = msg.trim().into();
                    self.notes.push(note);
                    self.next_note += 1;
                    self.notes.sort_by(|a,b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)).then(a.id.cmp(&b.id)));
                }
                self.flash = Some(Flash::new(format!("{} annotation(s) queued · s to send", self.notes.len()), true));
            }
            KeyCode::Left => self.note_cursor = msg[..self.note_cursor].char_indices().next_back().map_or(0, |(i,_)| i),
            KeyCode::Right => self.note_cursor += msg[self.note_cursor..].chars().next().map_or(0, char::len_utf8),
            KeyCode::Home => self.note_cursor = msg[..self.note_cursor].rfind('\n').map_or(0, |i| i + 1),
            KeyCode::End => self.note_cursor += msg[self.note_cursor..].find('\n').unwrap_or(msg.len() - self.note_cursor),
            KeyCode::Up | KeyCode::Down => {
                let start = msg[..self.note_cursor].rfind('\n').map_or(0, |i| i + 1);
                let col = msg[start..self.note_cursor].chars().count();
                let line = if k.code == KeyCode::Up {
                    (start > 0).then(|| {
                        let end = start - 1;
                        (msg[..end].rfind('\n').map_or(0, |i| i + 1), end)
                    })
                } else {
                    msg[self.note_cursor..].find('\n').map(|i| {
                        let start = self.note_cursor + i + 1;
                        (start, start + msg[start..].find('\n').unwrap_or(msg.len() - start))
                    })
                };
                if let Some((start, end)) = line {
                    self.note_cursor = start + msg[start..end].char_indices().nth(col).map_or(end - start, |(i,_)| i);
                }
            }
            KeyCode::Backspace if k.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) => {
                let suffix = msg.split_off(self.note_cursor);
                delete_word(msg);
                self.note_cursor = msg.len();
                msg.push_str(&suffix);
            }
            KeyCode::Backspace => {
                if self.note_cursor > 0 {
                    let start = msg[..self.note_cursor].char_indices().next_back().unwrap().0;
                    msg.drain(start..self.note_cursor);
                    self.note_cursor = start;
                }
            }
            KeyCode::Delete => {
                if self.note_cursor < msg.len() { msg.remove(self.note_cursor); }
            }
            KeyCode::Char('u') if ctrl => { msg.clear(); self.note_cursor = 0; }
            KeyCode::Char('w') if ctrl => {
                let suffix = msg.split_off(self.note_cursor);
                delete_word(msg);
                self.note_cursor = msg.len();
                msg.push_str(&suffix);
            }
            KeyCode::Char(c) if !ctrl && !c.is_control() => {
                msg.insert(self.note_cursor, c);
                self.note_cursor += c.len_utf8();
            }
            _ => {}
        }
    }

    fn find_agents(&mut self) {
        if self.notes.is_empty() || self.annotation_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.annotation_rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(AnnotationResult::Agents(annotations::agents()));
        });
    }

    /// `NG`, `:N`, `:Source N`: a source line on the given side, loading the
    /// whole file when the shown hunks don't include it.
    fn goto_line(&mut self, line: u32, old_side: bool) {
        let Some(path) = self.active.clone() else { return };
        self.seek_to(path, PendingSeek::Source { line, old_side });
    }

    fn send_notes(&mut self, agent: Agent) {
        let notes: Vec<_> = self.notes.iter().map(annotations::Note::format).collect();
        let (tx, rx) = mpsc::channel();
        self.annotation_rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(AnnotationResult::Sent(annotations::send(&agent, &notes)));
        });
    }

    fn on_agent_key(&mut self, k: KeyEvent) {
        let Some((agents, cursor)) = self.agent_picker.as_mut() else {
            return;
        };
        match k.code {
            KeyCode::Esc => self.agent_picker = None,
            KeyCode::Up | KeyCode::Char('k') => *cursor = cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => *cursor = (*cursor + 1).min(agents.len() - 1),
            KeyCode::Enter => {
                let (mut agents, cursor) = self.agent_picker.take().unwrap();
                self.send_notes(agents.remove(cursor));
            }
            _ => {}
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if let Some(mut picker) = self.picker.take() {
            if matches!(k.code, KeyCode::Up | KeyCode::Down) || (k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('n' | 'p'))) { picker.user_selected = true; }
            let items = picker.matches().to_vec();
            match k.code {
                KeyCode::Esc => return,
                KeyCode::Enter => {
                    if let Some(path) = items.get(picker.cursor) { self.open_path(path); }
                    return;
                }
                KeyCode::Down => picker.cursor = (picker.cursor + 1).min(items.len().saturating_sub(1)),
                KeyCode::Up => picker.cursor = picker.cursor.saturating_sub(1),
                KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => picker.cursor = (picker.cursor+1).min(items.len().saturating_sub(1)),
                KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => picker.cursor = picker.cursor.saturating_sub(1),
                KeyCode::Backspace => { picker.query.pop(); picker.update(); }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => { picker.query.push(c); picker.update(); }
                _ => {}
            }
            self.picker = Some(picker); return;
        }
        if self.command.is_some() { return self.on_command_key(k); }
        if let Some(offset) = self.help { return self.on_help_key(k, offset); }
        if self.note_box.is_some() { return self.on_note_key(k); }
        if self.agent_picker.is_some() { return self.on_agent_key(k); }
        if self.commit_box.is_some() { return self.on_commit_key(k); }
        // A pending confirmation: `y` goes ahead (Enter too, unless it can
        // delete files for good); any other key cancels.
        if let Some(c) = self.confirm.take() {
            let go = match k.code {
                KeyCode::Char('y' | 'Y') => true,
                KeyCode::Enter => c.enter_confirms,
                _ => false,
            };
            if go {
                self.busy = Some(c.busy);
                let _ = self.jobs.send(c.job);
            }
            return;
        }
        if let Some((action, count)) = self.resolve(k) { self.act(action, count); }
    }

    fn scope(&self) -> Scope {
        match (self.view, self.focus) {
            (View::Branches, _) => Scope::Branches,
            (View::History, _) => Scope::History,
            (View::Changes, Focus::Tree) => Scope::Tree,
            (View::Changes, Focus::Reader) => Scope::Reader,
            (View::Changes, Focus::Notes) => Scope::Notes,
        }
    }

    /// A key's action through the binding table, keeping `Space` / `z`
    /// prefixes and reader counts between keys.
    fn resolve(&mut self, k: KeyEvent) -> Option<(Action, Option<usize>)> {
        let key = match self.prefix.take() {
            Some(prefix) => match k.code {
                KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    if prefix == Action::Leader { Key::Leader(c) } else { Key::Z(c) }
                }
                _ => { self.count = None; return None; }
            },
            None => Key::from_event(&k),
        };
        let count = self.count.take();
        if count.is_some() && k.code == KeyCode::Esc { return None; }
        let binding = keymap::lookup(self.scope(), key)?;
        if k.kind == KeyEventKind::Repeat && !binding.action.repeats() { return None; }
        match binding.action {
            Action::Leader | Action::MotionPrefix => {
                self.prefix = Some(binding.action);
                self.count = count;
                None
            }
            Action::Count(digit) => {
                self.count = Some((count.unwrap_or(0) * 10 + usize::from(digit)).min(99_999));
                None
            }
            Action::Move(Motion::LineStart) if count.is_some() => {
                self.count = count.map(|c| (c * 10).min(99_999));
                None
            }
            action => Some((action, count)),
        }
    }

    /// The one dispatcher for keys and commands.
    fn act(&mut self, action: Action, count: Option<usize>) {
        let n = count.unwrap_or(1);
        match action {
            Action::Quit => self.quit = true,
            Action::Command => { self.command = Some(":".into()); self.completion = None; }
            Action::Help => self.help = Some(0),
            Action::Changes => self.show_view(View::Changes),
            Action::Branches => self.show_view(View::Branches),
            Action::WorkingTree => self.working_tree(),
            Action::Annotate => self.annotate(false),
            Action::AnnotateFile => self.annotate(true),
            Action::Commit => self.open_commit_box(),
            Action::Send => self.find_agents(),
            Action::Notes => self.toggle_notes(),
            Action::Context => self.toggle_context(),
            Action::Layout => self.toggle_diff_layout(),
            Action::Picker => self.open_picker(),
            Action::PrevHunk | Action::NextHunk => {
                let forward = action == Action::NextHunk;
                if let Some(Some(hint)) = self.with_doc(|r, d| r.hunk(d, forward, n)) {
                    self.flash = Some(Flash::new(hint.into(), true));
                }
            }
            Action::PrevFile => self.step_file(-(n as isize)),
            Action::NextFile => self.step_file(n as isize),
            Action::Bottom if count.is_some() && self.scope() == Scope::Reader => {
                let old_side = self.reader.old_side();
                self.goto_line(n as u32, old_side);
            }
            Action::Top | Action::Bottom | Action::Up | Action::Down
            | Action::PageUp | Action::PageDown | Action::HalfUp | Action::HalfDown => self.navigate(action, n),
            Action::FocusNext => self.cycle_focus(true),
            Action::FocusPrev => self.cycle_focus(false),
            Action::Leader | Action::MotionPrefix | Action::Count(_) => {}
            Action::Sidebar => {
                self.sidebar = !self.sidebar;
                if !self.sidebar && self.focus == Focus::Tree { self.set_focus(Focus::Reader); }
            }
            Action::OldSide | Action::NewSide => {
                let old = action == Action::OldSide;
                self.with_doc(|r, d| r.set_side(d, old));
            }
            Action::Back => self.back(),
            Action::Reload => self.reload(),
            Action::Left | Action::Right => self.tree_motion(action == Action::Right),
            Action::Stage => self.toggle_here(),
            Action::Activate => match self.rows.get(self.cursor).map(|r| &r.item) {
                Some(Item::File { idx, .. }) => {
                    let path = self.files()[*idx].path.clone();
                    self.activate(path);
                }
                Some(Item::Dir { .. }) => self.toggle_dir(self.cursor),
                None => {}
            },
            Action::Discard => self.ask_discard(),
            Action::History => {
                self.hist_branch = "HEAD".into();
                self.history = None;
                self.hist_cursor = 0;
                self.hist_off.set(0);
                self.view = View::History;
                let _ = self.jobs.send(Job::History { branch: self.hist_branch.clone() });
            }
            Action::BranchDiff => self.open_review(None),
            Action::Base => self.command = Some(format!(":base {}", self.review_base)),
            Action::AllFiles => self.toggle_all_files(),
            Action::ScrollDown => self.scroll_diff(n as isize),
            Action::ScrollUp => self.scroll_diff(-(n as isize)),
            Action::Grow | Action::Shrink => self.resize_tree(action == Action::Grow),
            Action::Move(motion) => { self.with_doc(|r, d| r.motion(d, motion, n)); }
            Action::Search => self.command = Some("/".into()),
            Action::SearchNext | Action::SearchPrev => {
                let backward = action == Action::SearchPrev;
                if let Some(Err(e)) = self.with_doc(|r, d| r.search_next(d, backward, n)) {
                    self.flash = Some(Flash::new(e, false));
                }
            }
            Action::VisualChar | Action::VisualLine => self.reader.visual(action == Action::VisualLine),
            Action::EditNote => self.edit_note(),
            Action::DeleteNote => self.delete_note(),
            Action::Checkout => self.checkout_selected(),
            Action::OpenHistory => self.open_history(),
            Action::Pull => self.pull_selected(),
            Action::Push => self.push_selected(),
            Action::Merge => self.merge_selected(),
            Action::DeleteBranch => self.ask_delete_branch(false),
            Action::ForceDeleteBranch => self.ask_delete_branch(true),
            Action::Fetch => self.fetch(),
            Action::OpenCommit => {
                if let Some(Ok(commits)) = &self.history && let Some(commit) = commits.get(self.hist_cursor) {
                    self.open_review(Some(commit.oid.clone()));
                }
            }
        }
    }

    /// Up/down, paging, and `g` / `G` move the focused list or the reader.
    fn navigate(&mut self, action: Action, n: usize) {
        let to = |at: usize, len: usize, page: usize| {
            let page = page.max(2) as isize;
            let delta = match action {
                Action::Top => return 0,
                Action::Bottom => return len.saturating_sub(1),
                Action::Up => -1,
                Action::Down => 1,
                Action::PageUp => -(page - 1),
                Action::PageDown => page - 1,
                Action::HalfUp => -(page / 2),
                _ => page / 2,
            };
            at.saturating_add_signed(delta * n as isize).min(len.saturating_sub(1))
        };
        match (self.view, self.focus) {
            (View::Branches, _) => {
                let len = self.branches.as_ref().and_then(|b| b.as_ref().ok()).map_or(0, Vec::len);
                self.branch_cursor = to(self.branch_cursor, len, 10);
            }
            (View::History, _) => {
                let page = (self.areas.diff.height as usize).max(10);
                self.hist_cursor = to(self.hist_cursor, self.history_len(), page + 1);
            }
            (View::Changes, Focus::Notes) => {
                self.note_sel = to(self.note_sel, self.notes.len(), 4);
                self.preview_note();
            }
            (View::Changes, Focus::Tree) => {
                let target = to(self.cursor, self.rows.len(), self.areas.tree.height as usize);
                self.move_row(target as isize - self.cursor as isize);
            }
            (View::Changes, Focus::Reader) => {
                let motion = match action {
                    Action::Top => Motion::Top,
                    Action::Bottom => Motion::Bottom,
                    Action::Up => Motion::Up,
                    Action::Down => Motion::Down,
                    Action::PageUp => Motion::PageUp,
                    Action::PageDown => Motion::PageDown,
                    Action::HalfUp => Motion::HalfUp,
                    _ => Motion::HalfDown,
                };
                self.with_doc(|r, d| r.motion(d, motion, n));
            }
        }
    }

    /// Tab / Shift-Tab through the visible panes. Tab on a file in the tree
    /// opens it at its first hunk.
    fn cycle_focus(&mut self, forward: bool) {
        if forward && self.focus == Focus::Tree
            && let Some(Item::File { idx, .. }) = self.rows.get(self.cursor).map(|r| &r.item) {
            let path = self.files()[*idx].path.clone();
            return self.activate(path);
        }
        let panes: Vec<Focus> = [(Focus::Tree, self.tree_visible()), (Focus::Reader, true), (Focus::Notes, self.notes_open)]
            .into_iter().filter(|(_, shown)| *shown).map(|(pane, _)| pane).collect();
        let i = panes.iter().position(|&p| p == self.focus).unwrap_or(0);
        let next = panes[(i + if forward { 1 } else { panes.len() - 1 }) % panes.len()];
        self.set_focus(next);
    }

    /// Esc: the reader clears its selection, then its search highlights;
    /// the notes close; otherwise back one screen.
    fn back(&mut self) {
        match (self.view, self.focus) {
            (View::Branches, _) => self.show_view(View::Changes),
            (View::History, _) => self.view = View::Branches,
            (View::Changes, Focus::Notes) => self.close_notes(),
            (View::Changes, Focus::Reader) if self.reader.escape() => {}
            (View::Changes, _) => match &self.comparison {
                Some(c) if c.is_commit => {
                    self.view = View::History;
                    if self.history.is_none() {
                        self.hist_branch = "HEAD".into();
                        let _ = self.jobs.send(Job::History { branch: self.hist_branch.clone() });
                    }
                }
                Some(_) => self.working_tree(),
                None => {}
            },
        }
    }

    fn reload(&mut self) {
        if self.view == View::Branches {
            let _ = self.jobs.send(Job::Branches);
            return;
        }
        self.cached.clear();
        if self.comparison.is_some() { self.request_diff(); } else { let _ = self.jobs.send(Job::Reload); }
    }

    fn open_picker(&mut self) {
        if self.comparison.is_none() { self.picker = Some(crate::project::Picker::new(self.root.clone())); }
        else { self.flash = Some(Flash::new("Press W for working-tree files".into(), false)); }
    }

    fn toggle_all_files(&mut self) {
        if self.comparison.is_some() { self.flash = Some(Flash::new("Press W for the project file tree".into(), false)); return; }
        self.all_files = !self.all_files;
        std::mem::swap(&mut self.collapsed, &mut self.other_collapsed);
        if self.inventory.is_none() {
            let browser = crate::project::Browser::new(self.root.clone());
            if let Some(Ok(snap)) = &self.snapshot {
                for file in &snap.files {
                    let mut parent = Path::new(&file.path).parent();
                    while let Some(p) = parent { if let Some(p) = p.to_str() { browser.request(p); } parent = p.parent(); }
                }
            }
            self.inventory = Some(browser);
        }
        self.update_catalog(); self.rebuild(None);
    }

    fn resize_tree(&mut self, grow: bool) {
        if self.areas.side {
            let w = self.areas.tree_frame.width;
            self.tree_pref_w = Some(if grow { w + 2 } else { w.saturating_sub(2) });
        } else {
            let h = self.areas.tree_frame.height;
            self.tree_pref = Some(if grow { h + 1 } else { h.saturating_sub(1).max(1) });
        }
    }

    /// `{` / `}`: the previous or next file in tree order (folded or not),
    /// opened at its first hunk with its folders expanded.
    fn step_file(&mut self, delta: isize) {
        let files = self.files();
        let order: Vec<String> = tree::build(files, &HashSet::new()).iter().filter_map(|r| match r.item {
            Item::File { idx, .. } => Some(files[idx].path.clone()),
            Item::Dir { .. } => None,
        }).collect();
        if order.is_empty() { return; }
        let last = order.len() - 1;
        let current = self.active.as_ref().and_then(|a| order.iter().position(|p| p == a));
        let i = match current {
            Some(i) => i.saturating_add_signed(delta).min(last),
            None if delta < 0 => last,
            None => 0,
        };
        if Some(i) == current {
            self.flash = Some(Flash::new(if delta < 0 { "First file" } else { "Last file" }.into(), true));
            return;
        }
        let path = order[i].clone();
        let mut parent = Path::new(&path).parent();
        while let Some(dir) = parent.filter(|p| !p.as_os_str().is_empty()) {
            self.collapsed.remove(dir.to_string_lossy().as_ref());
            parent = dir.parent();
        }
        self.rebuild(Some(&path));
        self.tree_scrolling = false;
        self.activate(path);
    }

    /// `a`: the reader's line or selection (the whole file on headers and
    /// notices, and from the tree); `A`: always the whole file.
    fn annotate(&mut self, whole: bool) {
        if self.annotation_rx.is_some() { return; }
        if self.doc().is_none() {
            self.flash = Some(Flash::new("No file to annotate".into(), false));
            return;
        }
        let capture = if self.focus == Focus::Tree {
            if !matches!(self.rows.get(self.cursor).map(|r| &r.item), Some(Item::File { .. })) {
                self.flash = Some(Flash::new("Select a file to annotate".into(), false));
                return;
            }
            None
        } else if whole { None } else { self.reader.capture() };
        if capture.as_ref().is_some_and(|(rows, _)| rows.is_empty()) {
            self.flash = Some(Flash::new("Select source text to annotate".into(), false));
            return;
        }
        self.open_note();
        self.reader.clear_selection();
        let (Some(draft), Some(diff), Some((selection, old_side))) = (self.draft_note.as_mut(), self.diff.as_ref(), capture) else { return };
        draft.target = draft.target.trim_end_matches(" (whole file)").to_string();
        for (r, text) in selection {
            let row = &diff.rows[r];
            let old_side = row.old.is_some() && (row.kind == RowKind::Del || old_side);
            let (side, number) = if old_side { ("old", row.old) } else { ("new", row.new) };
            draft.target.push_str(&format!("\n  {side}:{} | {text}", number.unwrap_or(0)));
            if draft.locations.is_empty() { draft.line = number; draft.old_side = old_side; }
            draft.locations.push((row.old, row.new));
            if draft.preview.len() < 3 {
                let mut preview = row.clone();
                preview.segs = vec![git::Seg { text, fg: None, bold: false, emph: false }];
                draft.preview.push(preview);
            }
        }
        if let Some((target, _)) = self.note_box.as_mut() { *target = draft.target.clone(); }
    }

    fn edit_note(&mut self) {
        if self.annotation_rx.is_some() { return; }
        let Some(note) = self.notes.get(self.note_sel) else { return };
        self.note_box = Some((note.target.clone(), note.comment.clone()));
        self.note_cursor = note.comment.len();
        self.editing_note = Some(self.note_sel);
    }

    fn delete_note(&mut self) {
        if self.annotation_rx.is_some() || self.notes.is_empty() { return; }
        self.notes.remove(self.note_sel);
        self.note_sel = self.note_sel.min(self.notes.len().saturating_sub(1));
        if self.notes.is_empty() { self.close_notes(); } else { self.preview_note(); }
    }

    fn on_command_key(&mut self, k: KeyEvent) {
        let Some(mut command) = self.command.take() else { return };
        let completing = matches!(k.code, KeyCode::Tab | KeyCode::BackTab);
        let completion = self.completion.take();
        match k.code {
            KeyCode::Esc => return,
            KeyCode::Enter => return self.run_command(command),
            _ if completing && command.starts_with(':') && !command.contains(' ') => {
                let forward = k.code == KeyCode::Tab;
                let (prefix, index) = match completion {
                    Some((prefix, i)) => {
                        let n = keymap::completions(&prefix).len().max(1);
                        let i = if forward { (i + 1) % n } else { (i + n - 1) % n };
                        (prefix, i)
                    }
                    None => (command[1..].to_string(), if forward { 0 } else { usize::MAX }),
                };
                let names = keymap::completions(&prefix);
                if !names.is_empty() {
                    let i = index.min(names.len() - 1);
                    command = format!(":{}", names[i]);
                    self.completion = Some((prefix, i));
                }
            }
            KeyCode::Backspace => {
                command.pop();
                if command.is_empty() { return; }
            }
            KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => command.push(c),
            _ => {}
        }
        self.command = Some(command);
    }

    /// Enter on the command line: each command runs the same action as its key.
    fn run_command(&mut self, command: String) {
        if let Some(pattern) = command.strip_prefix('/') {
            let result = self.with_doc(|r, d| r.search(d, pattern)).unwrap_or_else(|| Err("No file to search".into()));
            if let Err(e) = result { self.flash = Some(Flash::new(e, false)); }
            return;
        }
        match keymap::parse(command.trim_start_matches(':')) {
            Ok(keymap::Command::Run(action)) => self.act(action, None),
            Ok(keymap::Command::Source(line)) => {
                let old_side = self.reader.old_side();
                self.goto_line(line, old_side);
            }
            Ok(keymap::Command::Edit(path)) => self.open_path(&path),
            Ok(keymap::Command::Base(base)) => {
                self.review_base = base;
                self.open_review(None);
            }
            Err(e) => {
                self.flash = Some(Flash::new(e, false));
                self.command = Some(command);
            }
        }
    }

    fn on_help_key(&mut self, k: KeyEvent, offset: usize) {
        let len = keymap::help(self.scope()).len();
        self.help = match k.code {
            KeyCode::Esc | KeyCode::Char('q' | '?') => None,
            KeyCode::Char('j') | KeyCode::Down => Some((offset + 1).min(len.saturating_sub(1))),
            KeyCode::Char('k') | KeyCode::Up => Some(offset.saturating_sub(1)),
            _ => Some(offset),
        };
    }

    fn show_view(&mut self, view: View) {
        if view == View::Branches {
            let _ = self.jobs.send(Job::Branches);
        }
        self.view = view;
        self.prefix = None;
        self.count = None;
    }

    fn selected_branch(&self) -> Option<&BranchInfo> {
        match &self.branches {
            Some(Ok(l)) => l.get(self.branch_cursor),
            _ => None,
        }
    }

    fn open_history(&mut self) {
        let Some(b) = self.selected_branch() else { return };
        self.hist_branch = b.name.clone();
        self.history = None;
        self.hist_cursor = 0;
        self.hist_off.set(0);
        self.view = View::History;
        let _ = self.jobs.send(Job::History { branch: self.hist_branch.clone() });
    }

    fn history_len(&self) -> usize {
        match &self.history {
            Some(Ok(c)) => c.len(),
            _ => 0,
        }
    }

    fn checkout_selected(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let Some(b) = self.selected_branch() else { return };
        if b.current {
            self.flash = Some(Flash::new(format!("Already on {}", b.name), true));
            return;
        }
        if let Some(wt) = &b.worktree {
            let msg = format!("{} is open in worktree {}: switch there ({})", b.name, folder_name(wt), tilde(wt));
            self.flash = Some(Flash::new(msg, false));
            return;
        }
        let (name, remote) = (b.name.clone(), b.remote);
        self.busy = Some(format!("Switching to {name}…"));
        let _ = self.jobs.send(Job::Checkout { name, remote });
    }

    /// The selected branch, unless it's a remote one: those can only be checked
    /// out, merged, or browsed, so the other actions say so in the footer.
    fn selected_local_branch(&mut self) -> Option<String> {
        let b = self.selected_branch()?;
        if b.remote {
            let msg = format!("{} is a remote branch: check it out first (space)", b.name);
            self.flash = Some(Flash::new(msg, false));
            return None;
        }
        Some(b.name.clone())
    }

    fn fetch(&mut self) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some("Fetching…".into());
        let _ = self.jobs.send(Job::Fetch);
    }

    fn merge_selected(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let Some(b) = self.selected_branch() else { return };
        if b.current {
            self.flash = Some(Flash::new(format!("{} is the branch you're on: pick another one to merge", b.name), false));
            return;
        }
        let name = b.name.clone();
        self.busy = Some(format!("Merging {name}…"));
        let _ = self.jobs.send(Job::Merge { name });
    }

    fn pull_selected(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let Some(name) = self.selected_local_branch() else { return };
        self.busy = Some(format!("Pulling {name}…"));
        let _ = self.jobs.send(Job::Pull { name });
    }

    fn push_selected(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let Some(name) = self.selected_local_branch() else { return };
        self.busy = Some(format!("Pushing {name}…"));
        let _ = self.jobs.send(Job::Push { name });
    }

    /// `d`: get ready to throw away the changes of the file or folder under
    /// the cursor, and ask for confirmation in the footer.
    fn ask_discard(&mut self) {
        if self.comparison.is_some() { return; }
        if self.busy.is_some() {
            return;
        }
        let Some(row) = self.rows.get(self.cursor) else { return };
        let files = self.files();
        let (label, picked): (String, Vec<&git::FileEntry>) = match &row.item {
            Item::File { idx, .. } => (files[*idx].path.clone(), vec![&files[*idx]]),
            Item::Dir { path, .. } => {
                let prefix = format!("{path}/");
                (prefix.clone(), files.iter().filter(|f| f.status != ' ' && f.path.starts_with(&prefix)).collect())
            }
        };
        if picked.is_empty() {
            return;
        }
        if picked.iter().all(|f| f.status == ' ') { return; }
        let label = if picked.len() > 1 { format!("{label} ({} files)", picked.len()) } else { label };
        // New files aren't in HEAD: discarding them means deleting them.
        let (new, restore): (Vec<_>, Vec<_>) = picked.into_iter().partition(|f| f.status == 'A');
        let warning = match new.len() {
            0 => None,
            1 => Some("1 new file will be deleted for good.".to_string()),
            n => Some(format!("{n} new files will be deleted for good.")),
        };
        let confirm = Confirm {
            question: format!("Discard changes to {label}?"),
            warning,
            verb: "discard",
            enter_confirms: false,
            busy: format!("Discarding {label}…"),
            job: Job::Discard {
                restore: restore.iter().map(|f| f.path.clone()).collect(),
                new: new.iter().map(|f| f.path.clone()).collect(),
                label,
            },
        };
        self.flash = None;
        self.confirm = Some(confirm);
    }

    /// `d` / `D` on the Branches screen: ask before deleting the local branch.
    /// `d` lets git refuse a branch with unmerged commits; `D` deletes it anyway.
    fn ask_delete_branch(&mut self, force: bool) {
        if self.busy.is_some() {
            return;
        }
        let Some(b) = self.selected_branch() else { return };
        if b.current {
            self.flash = Some(Flash::new(format!("{} is the branch you're on: check out another one first", b.name), false));
            return;
        }
        if let Some(wt) = &b.worktree {
            let msg = format!("{} is open in worktree {}: remove it first ({})", b.name, folder_name(wt), tilde(wt));
            self.flash = Some(Flash::new(msg, false));
            return;
        }
        let Some(name) = self.selected_local_branch() else { return };
        let warning = force.then(|| "Commits that only exist on it will be lost.".to_string());
        self.flash = None;
        self.confirm = Some(Confirm {
            question: format!("{} branch {name}?", if force { "Force-delete" } else { "Delete" }),
            warning,
            verb: "delete",
            enter_confirms: true,
            busy: format!("Deleting {name}…"),
            job: Job::DeleteBranch { name, force },
        });
    }

    fn open_commit_box(&mut self) {
        if self.comparison.is_some() { return; }
        if self.busy.is_some() {
            return;
        }
        let staged = self.files().iter().filter(|f| f.staged != Staged::No).count();
        if staged == 0 {
            self.flash = Some(Flash::new("Nothing staged yet: press space on a file to stage it".into(), false));
            return;
        }
        self.flash = None;
        self.commit_box = Some(String::new());
    }

    /// Typing in the commit box. Enter commits, Esc cancels.
    fn on_commit_key(&mut self, k: KeyEvent) {
        let Some(msg) = self.commit_box.as_mut() else { return };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => self.commit_box = None,
            KeyCode::Char('c') if ctrl => self.commit_box = None,
            KeyCode::Enter => {
                let message = msg.trim().to_string();
                if message.is_empty() {
                    return;
                }
                self.commit_box = None;
                self.busy = Some("Committing…".into());
                let _ = self.jobs.send(Job::Commit { message });
            }
            KeyCode::Backspace if k.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) => {
                delete_word(msg);
            }
            KeyCode::Backspace => {
                msg.pop();
            }
            KeyCode::Char('u') if ctrl => msg.clear(),
            KeyCode::Char('w') if ctrl => delete_word(msg),
            KeyCode::Char(c) if !ctrl => msg.push(c),
            _ => {}
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        if self.picker.is_some() || self.command.is_some() || self.help.is_some() || self.note_box.is_some() || self.agent_picker.is_some() || self.commit_box.is_some() || self.confirm.is_some()
        {
            return;
        }
        if matches!(m.kind, MouseEventKind::Down(_)) {
            self.prefix = None;
            self.count = None;
        }
        if m.kind == MouseEventKind::Down(MouseButton::Left) && m.row == 0 {
            for (i, (x0, x1)) in self.tabs.get().iter().enumerate() {
                if m.column >= *x0 && m.column < *x1 {
                    return self.show_view(if i == 0 { View::Changes } else { View::Branches });
                }
            }
        }
        if self.view == View::History {
            let len = self.history_len();
            match m.kind {
                MouseEventKind::ScrollDown => self.hist_cursor = (self.hist_cursor + 1).min(len.saturating_sub(1)),
                MouseEventKind::ScrollUp => self.hist_cursor = self.hist_cursor.saturating_sub(1),
                // Row 2 is the title; commits start at row 3.
                MouseEventKind::Down(MouseButton::Left) if m.row >= 3 => {
                    let row = self.hist_off.get() + (m.row - 3) as usize;
                    if row < len {
                        self.hist_cursor = row;
                    }
                }
                _ => {}
            }
            return;
        }
        if self.view == View::Branches {
            let len = match &self.branches {
                Some(Ok(l)) => l.len(),
                _ => 0,
            };
            match m.kind {
                MouseEventKind::ScrollDown => self.branch_cursor = (self.branch_cursor + 1).min(len.saturating_sub(1)),
                MouseEventKind::ScrollUp => self.branch_cursor = self.branch_cursor.saturating_sub(1),
                MouseEventKind::Down(MouseButton::Left) if m.row >= 2 => {
                    let row = self.branch_off.get() + (m.row - 2) as usize;
                    if row < len {
                        self.branch_cursor = row;
                    }
                }
                _ => {}
            }
            return;
        }
        let a = self.areas;
        let inside = |r: Rect| m.column >= r.x && m.column < r.x + r.width && m.row >= r.y && m.row < r.y + r.height;
        let in_tree = inside(a.tree);
        let over_tree = inside(a.tree_frame);
        let on_splitter = if a.side {
            m.column + 1 >= a.splitter && m.column <= a.splitter + 1 && m.row >= a.tree.y
        } else {
            a.splitter > 0 && m.row + 1 >= a.splitter && m.row <= a.splitter + 1
        };
        if inside(a.notes) {
            if matches!(m.kind, MouseEventKind::Down(_)) { self.set_focus(Focus::Notes); }
            return;
        }
        let reader_drag = self.focus == Focus::Reader && matches!(m.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_));
        if (inside(a.diff) || reader_drag) && !self.dragging {
            let (x, y) = (m.column, m.row);
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    self.set_focus(Focus::Reader);
                    self.with_doc(|r, d| r.click(d, x, y));
                }
                MouseEventKind::Drag(MouseButton::Left) => { self.with_doc(|r, d| r.drag(d, x, y)); }
                MouseEventKind::Up(_) => self.reader.release(),
                MouseEventKind::ScrollDown => self.scroll_diff(3),
                MouseEventKind::ScrollUp => self.scroll_diff(-3),
                _ => {}
            }
            return;
        }
        match m.kind {
            MouseEventKind::ScrollDown if over_tree => {
                self.tree_scrolling = true;
                let max = self.rows.len().saturating_sub(a.tree.height as usize);
                self.tree_off = (self.tree_off + 1).min(max);
            }
            MouseEventKind::ScrollUp if over_tree => {
                self.tree_scrolling = true;
                self.tree_off = self.tree_off.saturating_sub(1);
            }
            MouseEventKind::ScrollDown => self.scroll_diff(3),
            MouseEventKind::ScrollUp => self.scroll_diff(-3),
            MouseEventKind::Down(MouseButton::Left) if on_splitter => self.dragging = true,
            MouseEventKind::Down(MouseButton::Left) if in_tree => {
                self.tree_scrolling = false;
                let row = self.tree_off + (m.row - a.tree.y) as usize;
                if row < self.rows.len() {
                    self.cursor = row;
                    match self.rows[row].item {
                        Item::Dir { .. } => {
                            self.set_focus(Focus::Tree);
                            self.toggle_dir(row);
                        }
                        // The checkbox stages/unstages without moving focus;
                        // the name opens the file.
                        Item::File { idx, .. } => {
                            let checkbox = a.tree.x + 1 + self.rows[row].guide.width() as u16;
                            let path = self.files()[idx].path.clone();
                            if m.column == checkbox {
                                self.preview(path);
                                self.toggle_stage(idx);
                            } else {
                                self.activate(path);
                            }
                        }
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging && a.side => {
                // The divider sits one column after the end of the tree.
                self.tree_pref_w = Some(m.column.saturating_sub(a.tree_frame.x + 1));
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                // The pointer sits on the hr, one row below the end of the tree.
                self.tree_pref = Some(m.row.saturating_sub(a.tree_frame.y + 1).max(1));
            }
            MouseEventKind::Up(MouseButton::Left) => self.dragging = false,
            _ => {}
        }
    }

    // ---------------------------------------------------------------- drawing

    fn note_list_area(&self, area: Rect) -> Rect {
        if !self.notes_open { return Rect::default(); }
        let wanted = (self.notes.len().min(4) as u16 * 2 + 2).max(6);
        let height = wanted.min((area.height / 3).max(4)).min(area.height.saturating_sub(8));
        Rect::new(area.x, area.bottom().saturating_sub(height), area.width, height)
    }

    fn layout(&mut self, mut area: Rect) {
        let dock = self.note_list_area(area);
        if !dock.is_empty() { area.height = area.height.saturating_sub(dock.height); }
        self.areas = if self.sidebar && !(self.notes_open && area.width < SIDE_MIN_WIDTH) {
            if area.width >= SIDE_MIN_WIDTH { self.side_layout(area) } else { self.stacked_layout(area) }
        } else {
            let mut a = self.stacked_layout(area);
            a.tree.height = 0;
            a.title = Rect::new(area.x, area.y + 2, area.width, 1);
            a.diff = Rect::new(area.x, area.y + 3, area.width, area.height.saturating_sub(4));
            a
        };
        use ratatui::widgets::Block;
        let body = Rect::new(area.x, area.y + 2, area.width, area.height.saturating_sub(3 + self.footer_gap()));
        self.areas.tree_frame = self.areas.tree.intersection(body);
        self.areas.file_frame = Rect::new(self.areas.title.x, self.areas.title.y,
            self.areas.title.width, self.areas.title.height + self.areas.diff.height).intersection(body);
        self.areas.tree = Block::bordered().inner(self.areas.tree_frame);
        let inner = Block::bordered().inner(self.areas.file_frame);
        self.areas.title = Rect::new(inner.x, inner.y, inner.width, inner.height.min(1));
        self.areas.diff = Rect::new(inner.x, inner.y + inner.height.min(1), inner.width, inner.height.saturating_sub(1));
        self.areas.notes = dock;
        self.reader.resize(self.areas.diff);
        let tree_h = self.areas.tree.height;

        // Keep the cursor visible in the tree.
        let h = tree_h as usize;
        if !self.tree_scrolling && self.cursor < self.tree_off {
            self.tree_off = self.cursor;
        } else if !self.tree_scrolling && h > 0 && self.cursor >= self.tree_off + h {
            self.tree_off = self.cursor + 1 - h;
        }
        self.tree_off = self.tree_off.min(self.rows.len().saturating_sub(h));

    }

    /// A blank row above the footer while the commit box is open, so it doesn't
    /// sit right against the diff.
    fn footer_gap(&self) -> u16 {
        u16::from(self.commit_box.is_some() || self.note_box.is_some())
    }

    /// Tree on top, diff below.
    fn stacked_layout(&self, area: Rect) -> Areas {
        // Header, rule, [tree], margin, hr, margin, title, [diff], footer.
        let avail = area.height.saturating_sub(CHROME_ROWS);
        let want = (self.rows.len() as u16).saturating_add(2);
        // The tree takes up to ~32% of the free rows (the rest goes to the diff).
        let auto = want.min((avail * 8 / 25).max(3));
        let max_tree = avail.saturating_sub(5).max(3);
        let tree_h = self.tree_pref.unwrap_or(auto).clamp(3, max_tree).min(avail);
        let tree = Rect::new(area.x, area.y + 2, area.width, tree_h);
        let splitter = tree.y + tree_h + 1;
        let title = Rect::new(area.x, splitter + 2, area.width, 1);
        let diff_h = area.height.saturating_sub(tree_h + CHROME_ROWS + self.footer_gap());
        let diff = Rect::new(area.x, title.y + 1, area.width, diff_h);
        Areas { side: false, tree, splitter, title, diff, ..Areas::default() }
    }

    /// Tree on the left, a vertical divider, diff on the right.
    fn side_layout(&self, area: Rect) -> Areas {
        // Header, rule, [tree | divider | title + diff], footer.
        let body_y = area.y + 2;
        let body_h = area.height.saturating_sub(3 + self.footer_gap());
        // As wide as the longest row needs, within 28 columns and 35% of the pane.
        let files = self.files();
        let needed = self
            .rows
            .iter()
            .map(|r| {
                let (label, adds, dels) = match &r.item {
                    Item::Dir { label, adds, dels, .. } => (label.width() + 3, *adds, *dels),
                    Item::File { idx, name } => (name.width() + 4, files[*idx].adds, files[*idx].dels),
                };
                let counts: usize = counts_text(adds, dels).iter().map(|(s, _)| s.width()).sum();
                (1 + r.guide.width() + label + 3 + counts + 1) as u16
            })
            .max()
            .unwrap_or(0);
        let auto = needed.clamp(28, (area.width * 35 / 100).max(28)).saturating_add(2);
        let tree_w = self.tree_pref_w.unwrap_or(auto).clamp(20, area.width / 2);
        let tree = Rect::new(area.x, body_y, tree_w, body_h);
        let splitter = area.x + tree_w + 1;
        let dx = splitter + 2;
        let dw = (area.x + area.width).saturating_sub(dx);
        let title = Rect::new(dx, body_y, dw, 1);
        let diff = Rect::new(dx, body_y + 1, dw, body_h.saturating_sub(1));
        Areas { side: true, tree, splitter, title, diff, ..Areas::default() }
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

        self.tabs.set(self.draw_header(buf, area, snapshot));
        hline(buf, area.x, area.y + 1, area.width, p.rule);

        match self.view {
            View::Branches => {
                self.draw_branches(buf, area);
                self.draw_footer(buf, area);
                self.draw_help(buf, area);
                return;
            }
            View::History => {
                self.draw_history(buf, area);
                self.draw_footer(buf, area);
                self.draw_help(buf, area);
                return;
            }
            View::Changes => {}
        }

        if files.is_empty() && self.active.is_none() {
            let rest = Rect::new(area.x, area.y + 2, area.width, area.height - 2);
            center(buf, rest, if self.comparison.is_some() { "No changes in this comparison" } else { "No changes ✓  working tree clean" }, p.faint);
            // Still show the footer: the result of a commit that took everything lands here.
            self.draw_footer(buf, area);
            self.draw_help(buf, area);
            return;
        }

        self.draw_tree(buf);
        self.draw_splitter(buf, area);
        self.draw_diff(buf);
        self.draw_pane_borders(buf);
        self.draw_note_list(buf, area, self.note_sel);
        let cursor = self.draw_footer(buf, area);
        self.draw_help(buf, area);
        if let Some(pos) = cursor {
            f.set_cursor_position(pos);
        }
    }

    /// `?`: the bindings of the current screen and focus, from the same
    /// table the dispatcher reads.
    fn draw_help(&self, buf: &mut Buffer, area: Rect) {
        use ratatui::widgets::{Block, BorderType, Clear, Widget};
        let Some(offset) = self.help else { return };
        let p = &self.pal;
        let scope = self.scope();
        let entries = keymap::help(scope);
        let width = area.width.saturating_sub(4).min(76);
        let height = (entries.len() as u16 + 2).min(area.height.saturating_sub(3));
        let popup = Rect::new(area.x + (area.width - width) / 2, area.y + 2, width, height);
        Clear.render(popup, buf);
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(p.blue))
            .title(format!(" Keys · {} ", scope.title())).title_bottom(" j/k scroll · ?/q/Esc close ");
        let inner = block.inner(popup);
        block.render(popup, buf);
        let keys_w = (inner.width / 3).max(8);
        for (i, (keys, help, _)) in entries.iter().skip(offset).take(inner.height as usize).enumerate() {
            let y = inner.y + i as u16;
            let label = keys.iter().map(|k| k.label()).collect::<Vec<_>>().join(" / ");
            let mut x = inner.x + 1;
            put(buf, &mut x, y, inner.x + keys_w, &label, Style::new().fg(p.fg).add_modifier(Modifier::BOLD));
            let mut x = inner.x + keys_w + 1;
            put(buf, &mut x, y, inner.right(), help, Style::new().fg(p.muted));
        }
    }

    fn draw_pane_borders(&self, buf: &mut Buffer) {
        use ratatui::widgets::{Block, BorderType, Widget};
        for (area, focused, title) in [
            (self.areas.tree_frame, self.focus == Focus::Tree, " Files "),
            (self.areas.file_frame, self.focus == Focus::Reader, " File "),
        ] {
            if area.is_empty() { continue; }
            Block::bordered().title(title)
                .border_type(if focused { BorderType::Thick } else { BorderType::Plain })
                .border_style(Style::new().fg(if focused { self.pal.blue } else { self.pal.rule }))
                .render(area, buf);
        }
    }

    /// Tabs, file count, branch, and totals. Returns the tabs' column ranges.
    fn draw_header(&self, buf: &mut Buffer, area: Rect, s: &Snapshot) -> [(u16, u16); 2] {
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
        let tab = |on: bool| {
            if on { Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED) } else { Style::new().fg(p.faint) }
        };
        let mut x = area.x + 1;
        let changes_x = x;
        put(buf, &mut x, y, lim, if self.comparison.is_some() { "Review" } else if self.all_files { "All files" } else { "Changes" }, tab(self.view == View::Changes));
        x += 1;
        let n = s.files.len();
        let pill = format!(" {n} {} ", if n == 1 { "file" } else { "files" });
        put(buf, &mut x, y, lim, &pill, Style::new().fg(p.muted).bg(p.pill_bg));
        let changes_end = x;
        x += 2;
        let branches_x = x;
        put(buf, &mut x, y, lim, "Branches", tab(matches!(self.view, View::Branches | View::History)));
        let branches_end = x;
        put(buf, &mut x, y, lim, &format!("   ⎇ {}", s.branch), Style::new().fg(p.muted));
        if self.view == View::Changes && self.comparison.is_none() && n > 0 {
            let done = s.files.iter().filter(|f| f.staged == Staged::Fully).count();
            let st = Style::new().fg(if done == n { p.green } else { p.muted });
            put(buf, &mut x, y, lim, &format!("   ✓ {done}/{n} staged"), st);
        }
        [(changes_x, changes_end), (branches_x, branches_end)]
    }

    /// The history screen: a title row, then one row per commit.
    fn draw_history(&self, buf: &mut Buffer, area: Rect) {
        let p = &self.pal;
        let right = area.x + area.width - 1;

        // Title: ‹ branch · N commits · K not pushed
        let ty = area.y + 2;
        let mut x = area.x + 1;
        put(buf, &mut x, ty, right, "‹ ", Style::new().fg(p.faint));
        put(buf, &mut x, ty, right, &self.hist_branch, Style::new().add_modifier(Modifier::BOLD));
        if let Some(Ok(commits)) = &self.history {
            let n = commits.len();
            let more = if n >= 1000 { "+" } else { "" };
            put(buf, &mut x, ty, right, &format!("  ·  {n}{more} commits"), Style::new().fg(p.muted));
            let unpushed = commits.iter().filter(|c| c.unpushed).count();
            if unpushed > 0 {
                put(buf, &mut x, ty, right, &format!("  ·  ↑{unpushed} not pushed"), Style::new().fg(p.green));
            }
        }

        let list_area = Rect::new(area.x, area.y + 3, area.width, area.height.saturating_sub(4));
        let commits = match &self.history {
            None => return center(buf, list_area, "Loading history…", p.faint),
            Some(Err(e)) => return center(buf, list_area, e, p.muted),
            Some(Ok(c)) if c.is_empty() => return center(buf, list_area, "No commits", p.faint),
            Some(Ok(c)) => c,
        };

        let h = list_area.height as usize;
        let mut off = self.hist_off.get();
        if self.hist_cursor < off {
            off = self.hist_cursor;
        } else if h > 0 && self.hist_cursor >= off + h {
            off = self.hist_cursor + 1 - h;
        }
        self.hist_off.set(off);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        // The author only when there's room for it next to the message.
        let author_w = if area.width >= 110 { commits.iter().map(|c| c.author.width()).max().unwrap_or(0).min(20) } else { 0 };
        for (i, c) in commits.iter().enumerate().skip(off).take(h) {
            let y = list_area.y + (i - off) as u16;
            let base = if i == self.hist_cursor { Style::new().bg(p.sel_bg) } else { Style::new() };
            buf.set_style(Rect::new(area.x, y, area.width, 1), base);

            // Author and time on the right.
            let when = relative_time(now - c.time);
            let when_w = 13; // "11 months ago"
            let mut author = c.author.clone();
            while author.width() > author_w {
                author.pop();
            }
            let right_w = (if author_w > 0 { author_w + 2 } else { 0 } + when_w) as u16;
            let rx = right.saturating_sub(right_w);
            let lim = rx.saturating_sub(2);
            let mut x = rx;
            put(buf, &mut x, y, right, &author, base.fg(p.muted));
            let mut x = right.saturating_sub(when.width() as u16);
            put(buf, &mut x, y, right, &when, base.fg(p.faint));

            let mut x = area.x + 1;
            if c.unpushed {
                put(buf, &mut x, y, lim, "↑ ", base.fg(p.green));
            } else {
                x += 2;
            }
            put(buf, &mut x, y, lim, &c.short, base.fg(p.yellow));
            x += 1;
            if !c.refs.is_empty() {
                put(buf, &mut x, y, lim, &format!("({}) ", c.refs.join(", ")), base.fg(p.blue));
            }
            let subject_style = if c.merge { base.fg(p.muted) } else { base };
            put(buf, &mut x, y, lim, &c.subject, subject_style);
        }
    }

    /// The Branches screen: one row per local branch.
    fn draw_branches(&self, buf: &mut Buffer, area: Rect) {
        let p = &self.pal;
        let list_area = Rect::new(area.x, area.y + 2, area.width, area.height.saturating_sub(3));
        let list = match &self.branches {
            None => return center(buf, list_area, "Loading branches…", p.faint),
            Some(Err(e)) => return center(buf, list_area, e, p.muted),
            Some(Ok(l)) if l.is_empty() => {
                return center(buf, list_area, "No branches yet: make the first commit", p.faint);
            }
            Some(Ok(l)) => l,
        };

        // Keep the cursor visible.
        let h = list_area.height as usize;
        let mut off = self.branch_off.get();
        if self.branch_cursor < off {
            off = self.branch_cursor;
        } else if h > 0 && self.branch_cursor >= off + h {
            off = self.branch_cursor + 1 - h;
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        // A branch open in another worktree shows its folder: `aux/worktree (sistema_2)`.
        let wt_suffix = |b: &BranchInfo| b.worktree.as_deref().map(|wt| format!(" ({})", folder_name(wt)));
        let name_w = list
            .iter()
            .map(|b| b.name.width() + wt_suffix(b).map_or(0, |s| s.width()))
            .max()
            .unwrap_or(0)
            .min(48) as u16;
        let sync = |b: &BranchInfo| -> Vec<(String, Color)> {
            if b.remote {
                return vec![("remote".into(), p.blue)];
            }
            match &b.upstream {
                None => vec![("not pushed".into(), p.yellow)],
                Some((u, 0, 0)) => vec![(format!("✓ {u}"), p.faint)],
                Some((u, ahead, behind)) => {
                    let mut v = Vec::new();
                    if *ahead > 0 {
                        v.push((format!("↑{ahead} "), p.green));
                    }
                    if *behind > 0 {
                        v.push((format!("↓{behind} "), p.red));
                    }
                    v.push((u.clone(), p.faint));
                    v
                }
            }
        };
        let sync_w = list
            .iter()
            .map(|b| sync(b).iter().map(|(t, _)| t.width()).sum::<usize>())
            .max()
            .unwrap_or(0) as u16;

        for (i, b) in list.iter().enumerate().skip(off).take(h) {
            let y = list_area.y + (i - off) as u16;
            let base = if i == self.branch_cursor { Style::new().bg(p.sel_bg) } else { Style::new() };
            buf.set_style(Rect::new(area.x, y, area.width, 1), base);
            let right = area.x + area.width - 1;

            // When, on the right.
            let when = relative_time(now - b.time);
            let mut x = right.saturating_sub(when.width() as u16);
            let lim = x.saturating_sub(2);
            put(buf, &mut x, y, right, &when, base.fg(p.faint));

            let mut x = area.x + 1;
            if b.current {
                put(buf, &mut x, y, lim, "● ", base.fg(p.green));
            } else if b.worktree.is_some() {
                put(buf, &mut x, y, lim, "◆ ", base.fg(p.blue));
            } else {
                x += 2;
            }
            let name_style = match (b.current, b.remote) {
                (true, _) => base.add_modifier(Modifier::BOLD),
                (_, true) => base.fg(p.muted),
                _ => base,
            };
            let start = x;
            put(buf, &mut x, y, (start + name_w).min(lim), &b.name, name_style);
            if let Some(suffix) = wt_suffix(b) {
                put(buf, &mut x, y, (start + name_w).min(lim), &suffix, base.fg(p.blue));
            }
            x = start + name_w + 3;
            let start = x;
            for (t, c) in sync(b) {
                put(buf, &mut x, y, lim, &t, base.fg(c));
            }
            x = start + sync_w + 3;
            put(buf, &mut x, y, lim, &b.subject, base.fg(p.muted));
        }
        self.branch_off.set(off);
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
                    put(buf, &mut x, y, lim, if self.comparison.is_some() { " " } else { mark }, base.fg(mark_fg).add_modifier(Modifier::BOLD));
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
        if self.areas.title.height == 0 { return; }
        let color = if self.dragging { p.blue } else { p.rule };
        if self.areas.side {
            // A vertical line between the tree and the diff.
            let t = self.areas.tree;
            for y in t.y..t.y + t.height {
                buf.set_string(self.areas.splitter, y, "│", Style::new().fg(color));
            }
        } else if !self.areas.tree_frame.is_empty() {
            // The hr, inset one column on each side.
            hline(buf, area.x + 1, self.areas.splitter, area.width.saturating_sub(2), color);
        }

        // File title: path on the left, position in the diff on the right.
        let t = self.areas.title;
        let y = t.y;
        if self.focus == Focus::Reader {
            buf.set_style(t, Style::new().bg(p.sel_bg));
        }
        let right = t.x + t.width - 1;
        let mut lim = right;
        if let Some(d) = self.diff.as_ref().filter(|d| Some(&d.path) == self.active.as_ref()) {
            let layout = if self.reader.split() { "split" } else { "stacked" };
            let mut info = format!("{layout} · {} rows", d.rows.len());
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
            let count = self.notes.iter().filter(|n| &n.path == path && n.line.is_none()).count();
            if count > 0 { put(buf, &mut x, y, lim, &format!("●{count} "), Style::new().fg(p.blue)); }
            put(buf, &mut x, y, lim, &dir, Style::new().fg(p.muted));
            put(buf, &mut x, y, lim, name, Style::new().add_modifier(Modifier::BOLD));
        }
    }

    fn draw_diff(&self, buf: &mut Buffer) {
        match self.doc() {
            Some(d) => {
                let focused = self.focus == Focus::Reader && self.note_box.is_none() && self.picker.is_none() && self.help.is_none();
                self.reader.draw(self.areas.diff, d, &self.notes, &self.pal, focused, buf);
            }
            None if self.active.is_some() => center(buf, self.areas.diff, "Loading…", self.pal.faint),
            None => {}
        }
    }

    fn draw_note_list(&self, buf: &mut Buffer, area: Rect, selected: usize) {
        use ratatui::widgets::{Block, BorderType, Clear, Widget};
        let p = &self.pal;
        let dock = self.note_list_area(area);
        if dock.is_empty() { return; }
        Clear.render(dock,buf);
        let count = self.notes.len();
        let title = format!(" Annotations · {} / {count} ", if count == 0 {0} else {selected+1});
        let focused = self.focus == Focus::Notes;
        let block = Block::bordered().border_type(if focused { BorderType::Thick } else { BorderType::Rounded })
            .border_style(Style::new().fg(p.annotation)).title(title)
            .title_bottom(" ↑↓ preview · Enter edit · d delete · Esc close ");
        let inner = block.inner(dock);
        block.render(dock,buf);
        if inner.is_empty() { return; }
        if self.notes.is_empty() {
            center(buf,inner,"No pending annotations",p.muted);
            return;
        }
        let list_width = if inner.width >= 100 { inner.width * 45 / 100 } else { inner.width };
        let visible = (inner.height / 2).max(1) as usize;
        let first = selected.saturating_sub(visible / 2).min(count.saturating_sub(visible));
        for (i,note) in self.notes.iter().enumerate().skip(first).take(visible) {
            let y = inner.y + (i-first) as u16 * 2;
            let selected = i == selected;
            let base = if selected { Style::new().bg(p.blue).fg(p.selection_fg) } else { Style::new().fg(p.muted) };
            buf.set_style(Rect::new(inner.x,y,list_width,2.min(inner.bottom()-y)),base);
            let stale = !note.historical && self.revision(&note.path) != note.revision;
            let mut x = inner.x;
            put(buf,&mut x,y,inner.x+list_width,&format!("{} {} · {}{}",
                if selected {"▌"} else {" "},note.path,note.range_label(),if stale {" [stale]"} else {""}),
                if selected {base.add_modifier(Modifier::BOLD)} else {base});
            if y+1 < inner.bottom() {
                let comment = note.comment.lines().collect::<Vec<_>>().join(" ↵ ");
                let mut x = inner.x+2;
                put(buf,&mut x,y+1,inner.x+list_width,&comment,if selected {base} else {base.fg(p.fg)});
            }
        }
        if list_width < inner.width && let Some(note) = self.notes.get(selected) {
            let split = inner.x+list_width+1;
            for y in inner.y..inner.bottom() { buf.set_string(split,y,"│",Style::new().fg(p.rule)); }
            let left = split+2;
            let mut x = left;
            put(buf,&mut x,inner.y,inner.right(),"Captured selection",Style::new().fg(p.annotation).add_modifier(Modifier::BOLD));
            if note.locations.is_empty() {
                let mut x = left;
                if inner.height>1 { put(buf,&mut x,inner.y+1,inner.right(),"Whole-file annotation",Style::new().fg(p.muted)); }
            }
            for (i,row) in note.preview.iter().enumerate().take(inner.height.saturating_sub(1) as usize) {
                let y = inner.y+1+i as u16;
                let (sign,bg) = match row.kind { RowKind::Add=>("+",p.add_bg), RowKind::Del=>("−",p.del_bg), _=>(" ",Color::Reset) };
                let style = Style::new().fg(p.fg).bg(bg);
                let mut x = left;
                put(buf,&mut x,y,inner.right(),&format!("{sign} {} │ {}",row.new.or(row.old).unwrap_or(0),annotations::text(row)),style);
            }
        }
    }

    /// The bottom row: key hints, the commit box, progress, or a message.
    /// Returns where the text cursor goes when the commit box is open.
    fn draw_footer(&self, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
        let p = &self.pal;
        // With the notes dock open, the footer sits right above it.
        let dock = if self.view == View::Changes { self.areas.notes.height } else { 0 };
        let y = (area.y + area.height - 1).saturating_sub(dock);
        let mut x = area.x + 1;
        let right = area.x + area.width;
        if let Some(picker) = &self.picker {
            use ratatui::widgets::{Block, Clear, Widget, Paragraph};
            let popup = Rect::new(area.x, area.y + 2, area.width, area.height.saturating_sub(3));
            Clear.render(popup, buf);
            let items = picker.matches();
            let text: Vec<ratatui::text::Line> = items.iter().enumerate().skip(picker.cursor.saturating_sub(2)).take(popup.height.saturating_sub(2) as usize)
                .map(|(i,p)| { let mut line = picker.highlighted(p); line.spans.insert(0, ratatui::text::Span::raw(if i == picker.cursor { "▸ " } else { "  " })); line }).collect();
            Paragraph::new(text).block(Block::bordered().title(format!(" Open file · {}{} ", picker.query, if picker.scanning { " · scanning…" } else { "" }))).render(popup, buf);
            return None;
        }
        if let Some(command) = &self.command {
            put(buf, &mut x, y, right, command, Style::new().fg(p.fg));
            let cursor = (x.min(right.saturating_sub(1)), y);
            // An unknown command keeps the input and shows why beside it.
            if let Some(f) = self.flash.as_ref().filter(|f| !f.ok && Instant::now() < f.until) {
                let mut x = x + 3;
                put(buf, &mut x, y, right, &f.text, Style::new().fg(p.red));
            }
            return Some(cursor);
        }

        if let Some((_, msg)) = &self.note_box {
            use ratatui::widgets::{Block, BorderType, Clear, Padding, Widget};
            let height = area.height.min((msg.lines().count() as u16 + 5).clamp(6,10));
            let width = if area.width < 8 { area.width } else { area.width.saturating_sub(2).min(96) };
            let popup = Rect::new(area.x+(area.width-width)/2, area.bottom().saturating_sub(height), width, height);
            Clear.render(popup, buf);
            let block = Block::bordered().border_type(BorderType::Rounded)
                .border_style(Style::new().fg(p.annotation)).style(Style::new().bg(p.sel_bg))
                .padding(Padding::horizontal(if width < 8 {0} else {1}))
                .title(if self.editing_note.is_some() {" Edit annotation "} else {" New annotation "})
                .title_bottom(" Enter save · Shift+Enter newline · Esc cancel ");
            let mut inner = block.inner(popup);
            block.render(popup, buf);
            if inner.is_empty() { return None; }
            let note = self.editing_note.and_then(|i|self.notes.get(i)).or(self.draft_note.as_ref());
            if let Some(note) = note {
                let mut x = inner.x;
                put(buf,&mut x,inner.y,inner.right(),&format!("{} · {}",note.path,note.range_label()),Style::new().fg(p.muted));
            }
            let heading = if inner.height >= 4 {2} else {0};
            inner.y += heading;
            inner.height -= heading;
            if inner.is_empty() { return None; }
            if msg.is_empty() {
                let mut x = inner.x;
                put(buf,&mut x,inner.y,inner.right(),"Write a review note…",Style::new().fg(p.faint));
            }
            let row = msg[..self.note_cursor].bytes().filter(|b| *b == b'\n').count();
            let start = msg[..self.note_cursor].rfind('\n').map_or(0, |i| i + 1);
            let mut offset = start;
            while msg[offset..self.note_cursor].width() >= inner.width as usize {
                offset += msg[offset..].chars().next().unwrap().len_utf8();
            }
            let skip = msg[start..offset].chars().count();
            let first = row.saturating_sub(inner.height as usize - 1);
            for (i, line) in msg.split('\n').enumerate().skip(first).take(inner.height as usize) {
                let shown: String = line.chars().skip(skip).collect();
                let mut x = inner.x;
                put(buf, &mut x, inner.y + (i - first) as u16, inner.right(), &shown, Style::new().fg(p.fg));
            }
            return Some((inner.x + msg[offset..self.note_cursor].width() as u16, inner.y + (row - first) as u16));
        }
        if let Some((agents, cursor)) = &self.agent_picker {
            use ratatui::widgets::{Block, Clear, Widget};
            let height = (agents.len() as u16 + 2).min(area.height.saturating_sub(2)).max(3);
            let width = area.width.min(80);
            let popup = Rect::new(area.x + (area.width - width) / 2, y.saturating_sub(height), width, height);
            Clear.render(popup, buf);
            Block::bordered().title(" Send annotations to agent ").render(popup, buf);
            let visible = height.saturating_sub(2) as usize;
            let offset = cursor.saturating_sub(visible.saturating_sub(1));
            for (i, agent) in agents.iter().enumerate().skip(offset).take(visible) {
                let mut x = popup.x + 1;
                put(
                    buf,
                    &mut x,
                    popup.y + 1 + (i - offset) as u16,
                    popup.right() - 1,
                    &format!("{} {}", if i == *cursor { "▸" } else { " " }, agent.label),
                    if i == *cursor { Style::new().bg(p.sel_bg) } else { Style::new() },
                );
            }
            put(buf, &mut x, y, right, "↑↓ choose agent · Enter paste · Esc cancel", Style::new().fg(p.blue));
            return None;
        }
        if self.annotation_rx.is_some() {
            put(
                buf,
                &mut x,
                y,
                right,
                "Contacting Rex… annotations kept until delivery succeeds",
                Style::new().fg(p.blue),
            );
            return None;
        }
        if let Some(msg) = &self.commit_box {
            return Some(self.draw_input_box(buf, area, y, msg, "Commit", "commit"));
        }
        if let Some(c) = &self.confirm {
            let red = Style::new().fg(p.red);
            put(buf, &mut x, y, right, &format!("{} ", c.question), red.add_modifier(Modifier::BOLD));
            if let Some(w) = &c.warning {
                put(buf, &mut x, y, right, &format!("{w} "), red);
            }
            put(buf, &mut x, y, right, if c.enter_confirms { "y/Enter" } else { "y" }, Style::new().fg(p.muted));
            put(buf, &mut x, y, right, &format!(" {}   ", c.verb), Style::new().fg(p.faint));
            put(buf, &mut x, y, right, "n/Esc", Style::new().fg(p.muted));
            put(buf, &mut x, y, right, " cancel", Style::new().fg(p.faint));
            return None;
        }
        if let Some(busy) = &self.busy {
            put(buf, &mut x, y, right, "◌ ", Style::new().fg(p.blue));
            put(buf, &mut x, y, right, busy, Style::new().fg(p.muted));
            return None;
        }
        if let Some(f) = &self.flash
            && Instant::now() < f.until
        {
            put(buf, &mut x, y, right, &f.text, Style::new().fg(if f.ok { p.green } else { p.red }));
            return None;
        }
        if !self.notes.is_empty() {
            put(buf, &mut x, y, right, &format!("{} notes · ", self.notes.len()), Style::new().fg(p.blue));
        }
        let back = match &self.comparison {
            Some(c) if c.is_commit => [("Esc", "commits"), ("W", "working tree")].as_slice(),
            Some(_) => [("Esc", "working tree"), ("B", "base")].as_slice(),
            None => &[],
        };
        let hints: &[(&str, &str)] = match (self.view, self.focus) {
            (View::Changes, Focus::Reader) => &[
                ("v/V", "select"), ("a", "annotate"), ("[ ]", "hunks"), ("{ }", "files"), ("/", "search"),
                ("S", "split"), ("Space e", "sidebar"), ("Tab", "pane"), ("?", "help"),
            ],
            (View::Changes, Focus::Notes) => &[("j/k", "preview"), ("Enter", "edit"), ("d", "delete"), ("Esc", "close"), ("?", "help")],
            (View::Changes, Focus::Tree) => &[
                ("Enter", "open"), ("z", "stage/fold"), ("a", "annotate"), ("c", "commit"), ("d", "discard"),
                ("L", "notes"), ("{ }", "files"), ("2", "branches"), ("?", "help"), (":", "command"),
            ],
            (View::History, _) => &[("↑↓", "move"), ("Enter", "diff"), ("D", "branch diff"), ("B", "base"), ("W", "working tree"), ("Esc", "back"), ("?", "help")],
            (View::Branches, _) => &[
                ("↑↓", "move"),
                ("space", "checkout"),
                ("p", "pull"),
                ("P", "push"),
                ("m", "merge into current"),
                ("d", "delete"),
                ("f", "fetch"),
                ("⏎", "history"),
                ("1", "changes"),
                ("?", "help"),
            ],
        };
        let back = if self.view == View::Changes { back } else { &[] };
        for (key, what) in back.iter().chain(hints) {
            put(buf, &mut x, y, right, key, Style::new().fg(p.muted));
            put(buf, &mut x, y, right, &format!(" {what}   "), Style::new().fg(p.faint));
        }
        None
    }

    fn draw_input_box(&self, buf: &mut Buffer, area: Rect, y: u16, msg: &str, label: &str, action: &str) -> (u16, u16) {
        let p = &self.pal;
        let right = area.x + area.width - 1;

        // Hints on the right, shortened when the pane is narrow.
        let hints: &[(&str, &str)] = &[("⏎", action), ("esc", "cancel")];
        let hints_w: u16 =
            hints.iter().map(|(k, w)| (k.width() + if w.is_empty() { 0 } else { w.width() + 1 } + 2) as u16).sum();
        let mut x = right.saturating_sub(hints_w);
        let field_end = x.saturating_sub(1);
        for (key, what) in hints {
            put(buf, &mut x, y, right, key, Style::new().fg(p.muted));
            if !what.is_empty() {
                put(buf, &mut x, y, right, &format!(" {what}"), Style::new().fg(p.faint));
            }
            x += 2;
        }

        let mut x = area.x + 1;
        put(buf, &mut x, y, field_end, &format!("{label} "), Style::new().add_modifier(Modifier::BOLD));
        let field_start = x;
        let field_w = field_end.saturating_sub(field_start);
        buf.set_style(Rect::new(field_start, y, field_w, 1), Style::new().bg(p.pill_bg));

        let inner = field_w.saturating_sub(2) as usize;
        let mut tx = field_start + 1;
        if msg.is_empty() {
            put(buf, &mut tx, y, field_end, &format!("{label} message"), Style::new().fg(p.faint).bg(p.pill_bg));
            return (field_start + 1, y);
        }
        // Show the end of the message when it doesn't fit.
        let mut shown = msg;
        while shown.width() > inner.saturating_sub(1) && !shown.is_empty() {
            let mut chars = shown.chars();
            chars.next();
            shown = chars.as_str();
        }
        put(buf, &mut tx, y, field_end, shown, Style::new().bg(p.pill_bg));
        (tx, y)
    }
}

/// Something destructive waiting for `y` in the footer: discarding changes
/// (`d` on Changes) or deleting a branch (`d`/`D` on Branches).
struct Confirm {
    /// "Discard changes to app/?", "Delete branch old-idea?"
    question: String,
    /// What can't be undone, if anything.
    warning: Option<String>,
    /// The word next to `y`.
    verb: &'static str,
    /// The footer while it runs.
    busy: String,
    job: Job,
    /// Enter confirms too (not for discards, which can delete files for good).
    enter_confirms: bool,
}

/// A message shown in the footer for a few seconds.
struct Flash {
    text: String,
    ok: bool,
    until: Instant,
}

impl Flash {
    fn new(text: String, ok: bool) -> Self {
        let secs = if ok { 5 } else { 8 };
        Self { text, ok, until: Instant::now() + Duration::from_secs(secs) }
    }
}

/// The last folder of a path (`~/code/app-review` → `app-review`).
fn folder_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |f| f.to_string_lossy().into_owned())
}

/// A path with the home folder written as `~`.
fn tilde(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// "just now", "5 min ago", "3 hours ago", "2 days ago"…
fn relative_time(secs: i64) -> String {
    let (n, unit) = match secs.max(0) {
        s if s < 60 => return "just now".into(),
        s if s < 3600 => (s / 60, "min"),
        s if s < 86_400 => (s / 3600, "hour"),
        s if s < 7 * 86_400 => (s / 86_400, "day"),
        s if s < 30 * 86_400 => (s / (7 * 86_400), "week"),
        s if s < 365 * 86_400 => (s / (30 * 86_400), "month"),
        s => (s / (365 * 86_400), "year"),
    };
    let plural = if n == 1 || unit == "min" { "" } else { "s" };
    format!("{n} {unit}{plural} ago")
}

/// Ctrl+W / Option+Backspace: delete the last word.
fn delete_word(s: &mut String) {
    while s.ends_with(' ') {
        s.pop();
    }
    while s.chars().last().is_some_and(|c| c != ' ') {
        s.pop();
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


#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{DRow, Seg};

    fn key(code: KeyCode) -> KeyEvent { KeyEvent::new(code, KeyModifiers::NONE) }
    fn ch(c: char) -> KeyEvent { key(KeyCode::Char(c)) }
    fn ctrl(c: char) -> KeyEvent { KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL) }
    fn row(kind: RowKind, old: Option<u32>, new: Option<u32>, text: &str) -> DRow {
        DRow { kind, old, new, segs: vec![Seg { text: text.into(), fg: None, bold: false, emph: false }] }
    }
    /// Two hunks: a modification (first change at row 2) and an addition (row 6).
    fn hunks() -> Vec<DRow> {
        use RowKind::*;
        vec![row(Hunk,None,None,"@@ -1,2 +1,2 @@"), row(Ctx,Some(1),Some(1),"a"), row(Del,Some(2),None,"b"),
            row(Add,None,Some(2),"B"), row(Hunk,None,None,"@@ -20,1 +20,2 @@"), row(Ctx,Some(20),Some(20),"t"),
            row(Add,None,Some(21),"u")]
    }
    /// Delivers `rows` for `path` as the worker would.
    fn show(app: &mut App, path: &str, rows: Vec<DRow>) {
        if app.active.as_deref() != Some(path) { app.active = Some(path.into()); }
        let max_lineno = rows.iter().filter_map(|r| r.old.max(r.new)).max().unwrap_or(0);
        app.on_msg(Msg::Diff(FileDiff { revision: app.revision(path), path: path.into(), context: app.context(), max_lineno, rows }));
    }

    #[test]
    fn annotation_list_highlights_selection_and_reserves_reader_space() {
        let (jobs,_) = mpsc::channel();
        let mut app=App::new(jobs,PathBuf::from("/tmp/rexdiff-test"),None);
        app.notes=(0..10).map(|i| annotations::Note {id:i,path:"x.rs".into(),line:Some(44),old_side:true,
            locations:vec![(Some(44),None),(Some(45),None),(Some(46),None),(Some(47),None)],
            revision:0,target:String::new(),preview:Vec::new(),historical:true,comment:format!("comment {i}")}).collect();
        app.notes_open=true;
        app.note_sel=9;
        for (w,h) in [(20,12),(80,24),(180,60)] {
            let area=Rect::new(0,0,w,h);
            app.layout(area);
            let dock=app.note_list_area(area);
            assert!(app.areas.diff.bottom()<=dock.y);
            assert!(app.areas.diff.height>=2);
            let mut buf=Buffer::empty(area);
            app.draw_note_list(&mut buf,area,9);
            let text:String=buf.content.iter().map(|c|c.symbol()).collect();
            assert!(text.contains("comment 9"));
            if w>=80 { assert!(text.contains("old 44–47")); }
            let selected=buf.content.iter().find(|c|c.symbol()=="▌").unwrap();
            assert_eq!(selected.bg,app.pal.blue);
            assert_eq!(selected.fg,app.pal.selection_fg);
        }
        app.notes_open=false;
        app.layout(Rect::new(0,0,80,24));
        assert!(!app.areas.tree_frame.is_empty());
    }

    #[test]
    fn annotation_preview_waits_for_its_file_and_matches_the_exact_diff_side() {
        let (jobs,rx)=mpsc::channel();
        let mut app=App::new(jobs,PathBuf::from("/tmp/rexdiff-test"),None);
        let revision=app.revision("x.rs");
        app.notes=vec![annotations::Note {id:1,path:"x.rs".into(),line:Some(45),old_side:false,
            locations:vec![(None,Some(45))],revision,target:String::new(),preview:Vec::new(),historical:false,comment:"eh".into()}];
        show(&mut app,"other.rs",vec![row(RowKind::Del,Some(45),None,"example")]);
        app.on_key(ch('L'));
        assert_eq!(app.focus,Focus::Notes);
        assert_eq!(app.active.as_deref(),Some("x.rs"));
        assert!(matches!(rx.try_recv(),Ok(Job::Diff {..})));
        assert!(app.doc().is_none(),"the reader waits for the note's file");
        show(&mut app,"x.rs",vec![row(RowKind::Del,Some(45),None,"example"),row(RowKind::Add,None,Some(45),"example")]);
        assert_eq!(app.reader.cursor().row,1,"the note's anchor, not the first hunk");
        assert!(app.pending.is_none());
        app.notes[0].locations=vec![(Some(45),None)];
        app.on_key(ch('j'));
        assert_eq!(app.reader.cursor().row,0);
        assert_eq!(app.focus,Focus::Notes);
        // A newer selection cancels the pending seek, and the late result is ignored.
        app.notes[0].path="late.rs".into();
        app.notes[0].revision=app.revision("late.rs");
        app.preview_note();
        assert!(matches!(rx.try_recv(),Ok(Job::Diff { path, .. }) if path=="late.rs"));
        app.set_active("other.rs".into());
        assert!(rx.try_recv().is_err(),"cached");
        assert!(app.pending.is_none());
        app.on_msg(Msg::Diff(FileDiff {revision:app.revision("late.rs"),path:"late.rs".into(),context:CONTEXT,max_lineno:1,rows:hunks()}));
        assert_eq!(app.doc().unwrap().path,"other.rs");
        // Stale notes don't navigate.
        app.notes[0].path="x.rs".into();
        app.notes[0].revision=revision.wrapping_add(1);
        app.preview_note();
        assert!(app.pending.is_none());
        assert_eq!(app.active.as_deref(),Some("other.rs"));
    }

    #[test]
    fn empty_reader_navigation_does_not_require_a_snapshot() {
        let (jobs,_)=mpsc::channel();
        let dir=tempfile::tempdir().unwrap();
        let mut app=App::new(jobs,dir.path().into(),None);
        app.layout(Rect::new(0,0,80,24));
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus,Focus::Reader);
        app.on_key(ch('a'));
        assert!(app.note_box.is_none());
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus,Focus::Tree);
        app.on_key(ctrl('p'));
        assert!(app.picker.is_some());
        app.on_key(key(KeyCode::Esc));
        assert!(app.picker.is_none());
        for c in ":q".chars() { app.on_key(ch(c)); }
        app.on_key(key(KeyCode::Enter));
        assert!(app.quit);
    }

    #[test]
    fn source_jump_preserves_old_side_when_expanding_context() {
        let (jobs,rx)=mpsc::channel();
        let mut app=App::new(jobs,PathBuf::from("/tmp/rexdiff-test"),None);
        show(&mut app,"x.rs",Vec::new());
        app.goto_line(21,true);
        assert!(app.full_context);
        assert!(matches!(app.pending,Some(Pending { seek: PendingSeek::Source { line: 21, old_side: true }, .. })));
        assert!(matches!(rx.try_recv(),Ok(Job::Diff {context:FULL_CONTEXT,..})));
        show(&mut app,"x.rs",vec![row(RowKind::Ctx,Some(18),Some(21),"context"),row(RowKind::Ctx,Some(21),Some(24),"context")]);
        assert_eq!(app.reader.cursor().row,1);
        app.goto_line(21,false);
        assert_eq!(app.reader.cursor().row,0);
    }

    #[test]
    fn native_selection_keeps_deleted_unicode_and_snapshot_coordinates() {
        let (jobs, _) = mpsc::channel();
        let mut app = App::new(jobs, PathBuf::from("/tmp/rexdiff-test"), None);
        app.layout(Rect::new(0, 0, 160, 30));
        show(&mut app, "x.rs", vec![row(RowKind::Del, Some(9), None, "aé中z")]);
        app.set_focus(Focus::Reader);
        for k in [ch('l'), ch('l'), ch('v'), ch('h'), ch('a')] { app.on_key(k); }
        let note = app.draft_note.as_ref().unwrap();
        assert!(note.target.ends_with("\n  old:9 | é中"));
        assert_eq!(note.locations, vec![(Some(9), None)]);
        assert_eq!(note.line, Some(9));
        assert!(note.old_side);
        assert_eq!(note.revision, app.revision("x.rs"));
        assert_eq!(annotations::text(&note.preview[0]), "é中");
        app.on_key(key(KeyCode::Esc));
        // The old side of a split diff keeps a context row's old number.
        show(&mut app, "x.rs", vec![row(RowKind::Ctx, Some(9), Some(12), "aé中z")]);
        app.on_key(ch('S'));
        assert!(app.reader.split());
        for k in [ch(' '), ch('h'), ch('0'), ch('l'), ch('v'), ch('l'), ch('a')] { app.on_key(k); }
        let note = app.draft_note.as_ref().unwrap();
        assert!(note.target.ends_with("\n  old:9 | é中"));
        assert_eq!(note.range_label(), "old 9");
        assert_eq!(note.locations, vec![(Some(9), Some(12))]);
        app.on_key(key(KeyCode::Esc));
        show(&mut app, "x.rs", vec![row(RowKind::Ctx, None, Some(12), "aé中z")]);
        for k in [ch(' '), ch('h'), ch('0'), ch('l'), ch('v'), ch('l'), ch('a')] { app.on_key(k); }
        let note = app.draft_note.as_ref().unwrap();
        assert!(note.target.ends_with("\n  new:12 | é中"));
        assert_eq!(note.line, Some(12));
        assert!(!note.old_side);
        assert_eq!(note.range_label(), "new 12");
        assert_eq!(note.locations, vec![(None, Some(12))]);
        app.note_box = None;
        app.on_key(ch('A'));
        assert!(app.draft_note.as_ref().unwrap().target.ends_with("(whole file)"));
    }

    #[test]
    fn focus_select_annotate_and_cancel_picker() {
        let (jobs, _) = mpsc::channel();
        let mut app = App::new(jobs, PathBuf::from("/tmp/rexdiff-test"), None);
        app.layout(Rect::new(0, 0, 80, 24));
        show(&mut app, "test.rs", (1..=3).map(|n| row(RowKind::Add, None, Some(n), &format!("line {n}"))).collect());
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Reader);
        for k in [ch('V'), ch('j'), ch('a')] { app.on_key(k); }
        assert_eq!(app.reader.cursor().row, 1);
        assert_eq!(app.cursor, 0);
        app.on_key(ch('q'));
        assert!(!app.quit);
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.notes.len(), 1);
        assert!(app.notes[0].format().contains("new:1 | line 1\n  new:2 | line 2"));
        assert!(app.notes[0].format().ends_with("Comment: q"));
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Tree);
        app.open_note();
        assert!(app.note_box.as_ref().unwrap().0.ends_with("(whole file)"));
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.notes.len(), 1);
        app.agent_picker = Some((
            vec![Agent { id: "one".into(), label: "one".into() }, Agent { id: "two".into(), label: "two".into() }],
            0,
        ));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.agent_picker.as_ref().unwrap().1, 1);
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 24));
        app.draw_footer(&mut buf, Rect::new(0, 0, 80, 24));
        app.on_key(key(KeyCode::Esc));
        assert!(app.agent_picker.is_none());
        assert_eq!(app.notes.len(), 1);
        app.on_key(ch('L'));
        assert_eq!(app.focus, Focus::Notes);
        app.on_key(key(KeyCode::Enter));
        app.on_key(ctrl('u'));
        app.on_key(ch('z'));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.notes[0].comment, "z");
        app.on_key(ch('d'));
        assert!(app.notes.is_empty());
        assert!(!app.notes_open);
        assert_eq!(app.focus, Focus::Tree);
    }

    #[test]
    fn annotation_cursor_and_multiline_editing() {
        let (jobs, _) = mpsc::channel();
        let mut app = App::new(jobs, PathBuf::from("/tmp/rexdiff-test"), None);
        app.note_box = Some((String::new(), String::new()));
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        for c in "aé中z".chars() { app.on_note_key(key(KeyCode::Char(c))); }
        app.on_note_key(key(KeyCode::Left));
        app.on_note_key(key(KeyCode::Backspace));
        app.on_note_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(app.note_box.as_ref().unwrap().1, "aé\nz");
        assert!(app.notes.is_empty());
        app.on_note_key(key(KeyCode::Up));
        assert_eq!(app.note_cursor, 0);
        app.on_note_key(key(KeyCode::Right));
        app.on_note_key(key(KeyCode::Right));
        assert_eq!(app.note_cursor, "aé".len());
        app.on_note_key(key(KeyCode::Down));
        assert_eq!(app.note_cursor, "aé\nz".len());
        app.on_note_key(key(KeyCode::Home));
        app.on_note_key(key(KeyCode::Delete));
        app.on_note_key(key(KeyCode::Char('中')));
        assert_eq!(app.note_box.as_ref().unwrap().1, "aé\n中");
        app.on_note_key(key(KeyCode::Up));
        app.on_note_key(key(KeyCode::End));
        assert_eq!(app.note_cursor, "aé".len());
        for width in [3, 10, 80] {
            let area = Rect::new(0, 0, width, 10);
            let mut buf = Buffer::empty(area);
            let (x, y) = app.draw_footer(&mut buf, area).unwrap();
            assert!(x < width && y < area.height);
        }
        app.on_note_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.note_box.as_ref().unwrap().1, "\n中");
        assert_eq!(app.note_cursor, 0);
    }

    #[test]
    fn review_footer_and_escape_match_the_comparison_kind() {
        for is_commit in [false, true] {
            let (jobs, rx) = mpsc::channel();
            let mut app = App::new(jobs, std::env::current_dir().unwrap(), None);
            app.comparison = Some(git::Comparison {
                is_commit, base: None, tip: git2::Oid::from_bytes(&[1;20]).unwrap(), label: "fixture".into(),
            });
            app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            if is_commit {
                assert!(app.view == View::History && app.comparison.is_some());
                assert!(matches!(rx.recv().unwrap(), Job::History { .. }));
            } else {
                assert!(app.view == View::Changes && app.comparison.is_none());
                assert!(matches!(rx.recv().unwrap(), Job::Refresh));
            }
        }
    }

    #[test]
    fn review_navigation_rejects_stale_results_and_worktree_writes() {
        let (jobs, rx) = mpsc::channel();
        let mut app = App::new(jobs, PathBuf::from("/tmp/rexdiff-test"), None);
        let comparison = git::Comparison { is_commit: true, base: None, tip: git2::Oid::from_bytes(&[1;20]).unwrap(), label: "commit fixture".into() };
        app.on_key(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::NONE));
        assert!(matches!(rx.recv().unwrap(), Job::History { branch } if branch == "HEAD"));
        app.history = Some(Ok(vec![CommitInfo { oid: comparison.tip.to_string(), short: "0101010".into(), subject: "fixture".into(), author: "test".into(), time: 0, unpushed: false, merge: false, refs: vec![] }]));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let Job::Review { request, commit, .. } = rx.recv().unwrap() else { panic!() };
        assert_eq!(commit, Some(comparison.tip.to_string()));
        let snap = || Snapshot { branch: "commit fixture".into(), files: vec![git::FileEntry { path: "deleted.rs".into(), status: 'D', adds: 0, dels: 1, binary: false, staged: Staged::No }] };
        app.on_msg(Msg::Review { request, result: Ok((comparison.clone(), snap())) });
        assert!(matches!(rx.recv().unwrap(), Job::ReviewDiff { path, .. } if path == "deleted.rs"));
        app.on_msg(Msg::Snapshot(Ok(Snapshot { branch: "wrong".into(), files: vec![] })));
        assert_eq!(app.files().len(), 1);
        app.toggle_stage(0);
        app.ask_discard();
        app.open_commit_box();
        assert!(rx.try_recv().is_err());
        assert!(app.confirm.is_none() && app.commit_box.is_none());
        let rows = vec![DRow { kind: RowKind::Del, old: Some(1), new: None, segs: vec![Seg { text: "original".into(), fg: None, bold: false, emph: false }] }];
        app.on_msg(Msg::Diff(FileDiff { revision: comparison.revision(), path: "deleted.rs".into(), context: 3, max_lineno: 1, rows }));
        app.open_note();
        assert!(app.note_box.as_ref().unwrap().0.contains(&comparison.tip.to_string()));
        app.on_note_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('W'), KeyModifiers::NONE));
        assert!(matches!(rx.recv().unwrap(), Job::Refresh));
        app.on_msg(Msg::Review { request, result: Ok((comparison, snap())) });
        assert!(app.comparison.is_none() && app.snapshot.is_none());
    }

    #[test]
    fn tree_wheel_scroll_survives_redraw_and_resize_uses_outer_bounds() {
        let (jobs, _) = mpsc::channel();
        let mut app = App::new(jobs, std::env::current_dir().unwrap(), None);
        app.all_files = true;
        app.catalog = (0..100).map(|i| git::FileEntry {
            path: format!("file_{i:03}.rs"), status: ' ', adds: 0, dels: 0,
            binary: false, staged: Staged::No,
        }).collect();
        app.rebuild(None);
        for width in [80, 180] {
            let area = Rect::new(0, 0, width, 30);
            app.cursor = 0;
            app.tree_off = 0;
            app.tree_scrolling = false;
            app.layout(area);
            let wheel = |kind, frame: Rect| MouseEvent { kind, column: frame.x, row: frame.y + 1, modifiers: KeyModifiers::NONE };
            for _ in 0..20 {
                app.on_mouse(wheel(MouseEventKind::ScrollDown, app.areas.tree_frame));
                app.layout(area);
            }
            assert_eq!(app.tree_off, 20);
            assert_eq!(app.cursor, 0);
            app.rebuild(None);
            app.layout(area);
            assert_eq!(app.tree_off, 20);
            app.on_mouse(wheel(MouseEventKind::ScrollUp, app.areas.tree_frame));
            app.layout(area);
            assert_eq!(app.tree_off, 19);
            for _ in 0..150 { app.on_mouse(wheel(MouseEventKind::ScrollDown, app.areas.tree_frame)); }
            app.layout(area);
            assert_eq!(app.tree_off, app.rows.len() - app.areas.tree.height as usize);
            app.move_row(1);
            app.layout(area);
            assert_eq!(app.tree_off, 1);
            let before = app.areas.tree_frame;
            app.on_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
            app.layout(area);
            if width >= SIDE_MIN_WIDTH {
                assert!(app.areas.tree.width >= 28);
                assert_eq!(app.areas.tree_frame.width, before.width + 2);
            } else {
                assert_eq!(app.areas.tree_frame.height, before.height + 1);
            }
            app.on_key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE));
            app.layout(area);
            assert_eq!(app.areas.tree_frame, before);
        }
    }

    #[test]
    fn layouts_and_read_only_catalog() {
        let (jobs, rx) = mpsc::channel();
        let mut app = App::new(jobs, std::env::current_dir().unwrap(), None);
        app.catalog.push(git::FileEntry {path:"Cargo.toml".into(),status:' ',adds:0,dels:0,binary:false,staged:Staged::No});
        app.all_files = true;
        app.rebuild(None);
        app.toggle_stage(0);
        app.ask_discard();
        assert!(app.confirm.is_none());
        assert!(rx.try_recv().is_err());
        app.layout(Rect::new(0,0,160,24));
        assert!(app.areas.side);
        assert!(app.areas.tree.x < app.areas.diff.x);
        app.focus = Focus::Tree;
        app.layout(Rect::new(0,0,40,20));
        assert_eq!(app.areas.tree_frame.width,40);
        assert_eq!(app.areas.tree.width,38);
        assert!(app.areas.diff.height > 0);
        assert!(app.areas.tree.y < app.areas.diff.y);
        assert!(!app.areas.side);
        app.focus = Focus::Reader;
        app.layout(Rect::new(0,0,40,20));
        assert!(app.areas.tree.height > 0);
        assert!(app.areas.diff.height > 0);
        app.layout(Rect::new(0,0,80,24));
        let tree = app.areas.tree_frame;
        let file = app.areas.file_frame;
        for file_focus in [false, true] {
            app.focus = if file_focus { Focus::Reader } else { Focus::Tree };
            let mut buffer = Buffer::empty(Rect::new(0,0,80,24));
            app.draw_pane_borders(&mut buffer);
            assert_eq!(buffer[(tree.x, tree.y)].fg, if file_focus { app.pal.rule } else { app.pal.blue });
            assert_eq!(buffer[(file.x, file.y)].fg, if file_focus { app.pal.blue } else { app.pal.rule });
            assert_eq!(buffer[(file.x, file.y)].symbol(), if file_focus { "┏" } else { "┌" });
        }
        for width in [10,20,40,48,80,160] {
            app.layout(Rect::new(0,0,width,6));
            let mut buffer = Buffer::empty(Rect::new(0,0,width,6));
            app.draw_tree(&mut buffer);
            app.draw_splitter(&mut buffer,Rect::new(0,0,width,6));
            app.draw_diff(&mut buffer);
            app.draw_pane_borders(&mut buffer);
        }
    }

    #[test]
    fn review_keys_and_activation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        for path in ["a.rs", "src/b.rs", "src/c.rs"] { std::fs::write(dir.path().join(path), "x\n").unwrap(); }
        let (jobs, rx) = mpsc::channel();
        let mut app = App::new(jobs, dir.path().canonicalize().unwrap(), None);
        let entry = |path: &str, staged| git::FileEntry { path: path.into(), status: 'M', adds: 2, dels: 1, binary: false, staged };
        app.on_msg(Msg::Snapshot(Ok(Snapshot { branch: "main".into(),
            files: vec![entry("a.rs", Staged::Fully), entry("src/b.rs", Staged::No), entry("src/c.rs", Staged::No)] })));
        app.layout(Rect::new(0, 0, 160, 40));
        let requested = |rx: &Receiver<Job>| rx.try_iter().filter_map(|j| match j { Job::Diff { path, .. } => Some(path), _ => None }).collect::<Vec<_>>();
        // Rows: src/ (b.rs, c.rs), then a.rs. The first file previews without focus.
        assert_eq!(requested(&rx), ["src/b.rs"]);
        show(&mut app, "src/b.rs", hunks());
        assert_eq!((app.focus, app.reader.cursor().row), (Focus::Tree, 2));
        app.on_key(ch('j'));
        assert_eq!((app.active.as_deref(), app.focus), (Some("src/c.rs"), Focus::Tree));
        assert_eq!(requested(&rx), ["src/c.rs"]);
        show(&mut app, "src/c.rs", hunks());
        // Enter opens at the first change; the tree keeps the reader's place.
        app.on_key(key(KeyCode::Enter));
        assert_eq!((app.focus, app.reader.cursor().row), (Focus::Reader, 2));
        app.on_key(ch(']'));
        assert_eq!(app.reader.cursor().row, 6);
        app.on_key(key(KeyCode::BackTab));
        assert_eq!((app.focus, app.reader.cursor().row), (Focus::Tree, 6));
        // Tab on the same, cached file opens it at its first hunk again.
        app.on_key(key(KeyCode::Tab));
        assert_eq!((app.focus, app.reader.cursor().row), (Focus::Reader, 2));
        assert!(requested(&rx).is_empty());
        // `}` follows tree order; stale and late results never take focus or the file.
        app.on_key(ch('}'));
        assert_eq!((app.active.as_deref(), app.focus), (Some("a.rs"), Focus::Reader));
        assert_eq!(app.rows[app.cursor].key(app.files()), "a.rs");
        assert_eq!(requested(&rx), ["a.rs"]);
        app.on_msg(Msg::Diff(FileDiff { revision: app.revision("src/c.rs"), path: "src/c.rs".into(), context: CONTEXT, max_lineno: 21, rows: hunks() }));
        assert!(app.doc().is_none());
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Tree);
        show(&mut app, "a.rs", hunks());
        assert_eq!((app.focus, app.reader.cursor().row), (Focus::Tree, 2));
        // Enter on a folder folds it; `}` expands it again and selects the file.
        app.on_key(ch('g'));
        assert_eq!(app.cursor, 0);
        app.on_key(key(KeyCode::Enter));
        assert_eq!((app.rows.len(), app.focus, app.active.as_deref()), (2, Focus::Tree, Some("src/b.rs")));
        app.on_key(ch('}'));
        assert_eq!((app.active.as_deref(), app.focus), (Some("src/c.rs"), Focus::Reader));
        assert_eq!((app.rows.len(), app.rows[app.cursor].key(app.files())), (4, "src/c.rs"));
        for _ in 0..2 { app.on_key(ch('{')); }
        assert_eq!(app.active.as_deref(), Some("src/b.rs"));
        assert_eq!(app.flash.as_ref().unwrap().text, "First file");
        // Clicking a name opens the file; clicking a checkbox only stages.
        let tree = app.areas.tree;
        let click = |column, row| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column, row, modifiers: KeyModifiers::NONE };
        app.set_focus(Focus::Tree);
        app.on_mouse(click(tree.x + 6, tree.y + 3));
        assert_eq!((app.active.as_deref(), app.focus), (Some("a.rs"), Focus::Reader));
        app.on_mouse(click(tree.x + 4, tree.y + 1));
        assert_eq!(app.focus, Focus::Reader);
        assert!(rx.try_iter().any(|j| matches!(j, Job::Stage { path, add: true } if path == "src/b.rs")));
        // `:e` opens at the first hunk, even from a later one.
        app.on_key(ch(']'));
        assert_eq!(app.reader.cursor().row, 6);
        for c in ":e src/b.rs".chars() { app.on_key(ch(c)); }
        app.on_key(key(KeyCode::Enter));
        assert_eq!((app.active.as_deref(), app.focus, app.reader.cursor().row), (Some("src/b.rs"), Focus::Reader, 2));
        // z stages in the tree; Space alone does nothing anywhere.
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Tree);
        app.on_key(ch('z'));
        assert!(rx.try_iter().any(|j| matches!(j, Job::Stage { .. })));
        for pane in [Focus::Tree, Focus::Reader] {
            app.set_focus(pane);
            let cursor = (app.cursor, app.reader.cursor());
            app.on_key(ch(' '));
            app.on_key(key(KeyCode::Esc));
            assert_eq!((app.focus, app.cursor, app.reader.cursor(), app.view), (pane, cursor.0, cursor.1, View::Changes));
            assert!(rx.try_recv().is_err());
        }
        // <leader>e hides the tree, moving focus off it; <leader>h/l pick split sides.
        app.set_focus(Focus::Tree);
        for k in [ch(' '), ch('e')] { app.on_key(k); }
        assert_eq!((app.sidebar, app.focus), (false, Focus::Reader));
        for k in [ch(' '), ch('e')] { app.on_key(k); }
        assert_eq!((app.sidebar, app.focus), (true, Focus::Reader));
        app.on_key(ch('S'));
        for k in [ch(' '), ch('h')] { app.on_key(k); }
        assert!(app.reader.old_side());
        for k in [ch(' '), ch('l')] { app.on_key(k); }
        assert!(!app.reader.old_side());
        app.on_key(ch('S'));
        // S, x, Ctrl+p, s and L act directly from every review pane.
        for pane in [Focus::Tree, Focus::Reader, Focus::Notes] {
            if pane == Focus::Notes { app.on_key(ch('L')); } else { app.set_focus(pane); }
            assert_eq!(app.focus, pane);
            let split = app.reader.split();
            app.on_key(ch('S'));
            assert_ne!(app.reader.split(), split);
            app.on_key(ch('S'));
            let full = app.full_context;
            app.on_key(ch('x'));
            assert_ne!(app.full_context, full);
            app.on_key(ch('x'));
            app.on_key(ctrl('p'));
            assert!(app.picker.is_some());
            app.on_key(key(KeyCode::Esc));
            assert_eq!(keymap::lookup(app.scope(), Key::from_event(&ch('s'))).unwrap().action, Action::Send);
            assert_eq!(app.focus, pane);
        }
        app.on_key(ch('L'));
        assert_eq!((app.notes_open, app.focus), (false, Focus::Reader));
        // 1/2 switch screens outside the reader and count inside it.
        app.set_focus(Focus::Tree);
        app.on_key(ch('2'));
        assert!(app.view == View::Branches);
        app.on_key(ch('1'));
        assert!(app.view == View::Changes);
        app.set_focus(Focus::Reader);
        for k in [ch('g'), ch('2'), ch('j')] { app.on_key(k); }
        assert!(app.view == View::Changes);
        assert_eq!(app.reader.cursor().row, 3);
        // a line, A file and c commit are distinct.
        app.on_key(ch('a'));
        assert!(app.note_box.as_ref().unwrap().0.ends_with("\n  new:2 | B"));
        app.on_key(key(KeyCode::Esc));
        app.on_key(ch('A'));
        assert!(app.note_box.as_ref().unwrap().0.ends_with("(whole file)"));
        // Text entry stays text.
        for c in "q j:".chars() { app.on_key(ch(c)); }
        assert_eq!(app.note_box.as_ref().unwrap().1, "q j:");
        assert!(!app.quit);
        app.on_key(key(KeyCode::Enter));
        app.on_key(ch('c'));
        assert!(app.commit_box.is_some() && app.note_box.is_none());
        app.on_key(key(KeyCode::Esc));
        // Ctrl+d in the notes and Ctrl+p on Branches are not destructive.
        let _ = rx.try_iter().count();
        app.on_key(ch('L'));
        app.on_key(ctrl('d'));
        assert_eq!(app.notes.len(), 1);
        app.on_key(key(KeyCode::Esc));
        let branch = |name: &str, current| BranchInfo { name: name.into(), current, remote: false, worktree: None, upstream: None, subject: String::new(), time: 0 };
        app.branches = Some(Ok(vec![branch("main", true), branch("feature", false)]));
        app.set_focus(Focus::Tree);
        app.on_key(ch('2'));
        assert!(app.view == View::Branches);
        app.branch_cursor = 1;
        let _ = rx.try_iter().count();
        app.on_key(ctrl('p'));
        assert!(rx.try_recv().is_err() && app.picker.is_none());
        // Space on a branch row still checks it out.
        app.on_key(ch(' '));
        assert!(matches!(rx.try_recv(), Ok(Job::Checkout { name, remote: false }) if name == "feature"));
    }

    #[test]
    fn help_commands_and_confirm() {
        let (jobs, rx) = mpsc::channel();
        let mut app = App::new(jobs, PathBuf::from("/tmp/rexdiff-test"), None);
        let run = |app: &mut App, command: &str| {
            app.on_key(ch(':'));
            for c in command.chars() { app.on_key(ch(c)); }
            app.on_key(key(KeyCode::Enter));
        };
        let contexts = [(View::Changes, Focus::Tree), (View::Changes, Focus::Reader), (View::Changes, Focus::Notes), (View::Branches, Focus::Tree), (View::History, Focus::Tree)];
        for (view, focus) in contexts {
            app.view = view;
            app.focus = focus;
            let scope = app.scope();
            app.on_key(ch('?'));
            assert_eq!(app.help, Some(0));
            let area = Rect::new(0, 0, 100, 60);
            let mut buf = Buffer::empty(area);
            app.draw_help(&mut buf, area);
            let screen: String = buf.content.iter().map(|c| c.symbol()).collect();
            assert!(screen.contains(scope.title()));
            assert!(screen.contains(keymap::help(scope)[0].1), "{scope:?} help renders its keys");
            app.on_key(key(KeyCode::Esc));
            assert!(app.help.is_none());
            for name in ["help", "h"] {
                run(&mut app, name);
                assert_eq!(app.help, Some(0));
                app.on_key(ch('q'));
            }
            // Every key the help lists dispatches the listed action.
            let entries = keymap::help(scope);
            assert!(entries.len() > 5, "{scope:?} help lists its keys");
            for (keys, _, action) in entries {
                for k in keys {
                    app.prefix = None;
                    app.count = None;
                    let events = match k {
                        Key::Press(code, modifiers) => vec![KeyEvent::new(code, modifiers)],
                        Key::Leader(c) => vec![ch(' '), ch(c)],
                        Key::Z(c) => vec![ch('z'), ch(c)],
                    };
                    let mut resolved = None;
                    for e in events { resolved = app.resolve(e); }
                    match action {
                        Action::Leader | Action::MotionPrefix => assert_eq!(app.prefix, Some(action)),
                        Action::Count(digit) => assert_eq!(app.count, Some(usize::from(digit))),
                        _ => assert_eq!(resolved.map(|(a, _)| a), Some(action), "{} in {scope:?}", k.label()),
                    }
                }
            }
        }
        app.prefix = None;
        app.view = View::Changes;
        app.focus = Focus::Tree;
        // Tab completes and cycles command names; Shift-Tab goes back.
        app.on_key(ch(':'));
        app.on_key(ch('c'));
        let mut seen = Vec::new();
        for _ in 0..4 {
            app.on_key(key(KeyCode::Tab));
            seen.push(app.command.clone().unwrap());
        }
        assert_eq!(seen, [":changes", ":commit", ":context", ":changes"]);
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.command.as_deref(), Some(":context"));
        app.on_key(key(KeyCode::Esc));
        // Unknown commands report and keep the input.
        run(&mut app, "nope");
        assert_eq!(app.command.as_deref(), Some(":nope"));
        assert!(app.flash.as_ref().unwrap().text.starts_with("Unknown command"));
        app.on_key(key(KeyCode::Esc));
        // Each command runs the action of a bound key.
        for (name, action) in keymap::COMMANDS.iter().filter_map(|(n, a)| a.map(|a| (*n, a))) {
            assert_eq!(keymap::parse(name), Ok(keymap::Command::Run(action)));
            assert!(keymap::BINDINGS.iter().any(|b| b.action == action), ":{name}");
        }
        app.layout(Rect::new(0, 0, 160, 40));
        run(&mut app, "diff");
        assert!(app.reader.split());
        app.on_key(ch('S'));
        assert!(!app.reader.split());
        run(&mut app, "context");
        assert!(app.full_context);
        app.on_key(ch('x'));
        assert!(!app.full_context);
        run(&mut app, "notes");
        assert!(app.notes_open && app.focus == Focus::Notes);
        app.on_key(ch('L'));
        assert!(!app.notes_open);
        run(&mut app, "branches");
        assert!(app.view == View::Branches);
        run(&mut app, "changes");
        assert!(app.view == View::Changes);
        let _ = rx.try_iter().count();
        run(&mut app, "reload");
        assert!(matches!(rx.try_recv(), Ok(Job::Reload)));
        // Confirm: y or Enter deletes a branch; only y discards.
        let branch = |name: &str, current| BranchInfo { name: name.into(), current, remote: false, worktree: None, upstream: None, subject: String::new(), time: 0 };
        app.branches = Some(Ok(vec![branch("main", true), branch("old", false)]));
        app.view = View::Branches;
        app.branch_cursor = 1;
        let _ = rx.try_iter().count();
        for confirm in [ch('y'), key(KeyCode::Enter)] {
            app.busy = None;
            app.on_key(ch('d'));
            app.on_key(confirm);
            assert!(matches!(rx.try_recv(), Ok(Job::DeleteBranch { force: false, .. })));
        }
        for cancel in [ch('n'), key(KeyCode::Esc)] {
            app.busy = None;
            app.on_key(ch('d'));
            app.on_key(cancel);
            assert!(app.confirm.is_none() && rx.try_recv().is_err());
        }
        app.view = View::Changes;
        app.on_msg(Msg::Snapshot(Ok(Snapshot { branch: "main".into(),
            files: vec![git::FileEntry { path: "new.rs".into(), status: 'A', adds: 1, dels: 0, binary: false, staged: Staged::No }] })));
        let _ = rx.try_iter().count();
        for cancel in [key(KeyCode::Enter), ch('n'), key(KeyCode::Esc)] {
            app.busy = None;
            app.on_key(ch('d'));
            assert!(app.confirm.is_some());
            app.on_key(cancel);
            assert!(app.confirm.is_none() && rx.try_recv().is_err());
        }
        app.on_key(ch('d'));
        app.on_key(ch('y'));
        assert!(matches!(rx.try_recv(), Ok(Job::Discard { .. })));
    }
}
