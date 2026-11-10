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
use crate::theme::{self, Palette};
use crate::tree::{self, Item, TreeRow};

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
    tree_frame: Rect,
    file_frame: Rect,
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

/// The two screens: changed files with their diff, and the local branches.
#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Changes,
    Branches,
    /// The commit history of one branch, opened from the Branches screen.
    History,
}

struct App {
    reader: Option<crate::reader::Reader>,
    reader_error: Option<String>,
    note_preview_pending: bool,
    root: PathBuf,
    picker: Option<crate::project::Picker>,
    inventory: Option<crate::project::Browser>,
    other_collapsed: HashSet<String>,
    catalog: Vec<git::FileEntry>,
    all_files: bool,
    sidebar: bool,
    note_list: Option<usize>,
    editing_note: Option<usize>,
    command: Option<String>,
    file_focus: bool,
    file_cursor: Position,
    notes: Vec<annotations::Note>,
    next_note: u64,
    pending_line: Option<u32>,
    pending_exact: bool,
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
    let reader = crate::reader::Reader::start(&cwd)?;
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
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES))?;

    let mut app = App::new(job_tx, cwd, detected);
    app.reader = Some(reader);

    let res = app.event_loop(&mut terminal, &msg_rx, &fs_rx);
    let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags, DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    res
}

impl App {
    fn reader_failed(&mut self, error: std::io::Error) {
        self.reader_error = Some(error.to_string());
        self.reader = None;
    }

    fn sync_reader(&mut self) {
        let Some(reader) = self.reader.as_mut() else { return };
        let result = (|| {
            reader.resize(self.areas.diff)?;
            if let Some(d) = self.diff.as_ref().filter(|d| Some(&d.path) == self.active.as_ref()) {
                if reader.generation != Some(self.diff_gen) {
                    reader.load(d, self.diff_gen, self.file_cursor, &self.pal)?;
                }
                let marked = self.notes.iter().filter(|note| note.path == d.path && note.revision == self.document_revision)
                    .filter_map(|note| {
                        let rows: Vec<_> = d.rows.iter().enumerate().filter(|(_, row)|
                            note.locations.contains(&(row.old,row.new)) && (row.old.is_some() || row.new.is_some()))
                            .map(|(i,_)| i).collect();
                        (!rows.is_empty()).then(|| serde_json::json!({"rows":rows,"comment":note.comment}))
                    }).collect();
                reader.notes(marked)?;
                if self.note_preview_pending {
                    reader.reveal(self.file_cursor,d)?;
                    self.note_preview_pending = false;
                }
            }
            Ok(())
        })();
        if let Err(e) = result { self.reader_failed(e); }
    }

    fn reader_action(&mut self, action: serde_json::Value) {
        // A queued selection must never attach to a newly refreshed snapshot.
        if action["generation"].as_u64() != Some(self.diff_gen)
            || !self.diff.as_ref().is_some_and(|d| Some(&d.path) == self.active.as_ref()) { return; }
        self.file_cursor.row = action["row"].as_u64().unwrap_or(0) as usize;
        self.file_cursor.col = self.diff.as_ref().and_then(|d| d.rows.get(self.file_cursor.row)).map(|r| {
            let text = annotations::text(r);
            text.char_indices().take_while(|(byte,_)| *byte < action["byte"].as_u64().unwrap_or(0) as usize).count()
        }).unwrap_or(0);
        match action["action"].as_str().unwrap_or("") {
            "annotate" => {
                if self.annotation_rx.is_some() { return; }
                if action["selection"].as_array().is_some_and(|selected| !selected.is_empty()
                    && !selected.iter().any(|s| s["row"].as_u64().and_then(|i| self.diff.as_ref()?.rows.get(i as usize))
                        .is_some_and(|row| !matches!(row.kind,RowKind::Hunk|RowKind::Meta)))) {
                    self.flash = Some(Flash::new("Select source text to annotate".into(),false));
                    return;
                }
                self.open_note();
                if let (Some(draft), Some(diff), Some(selection)) = (self.draft_note.as_mut(), self.diff.as_ref(), action["selection"].as_array()) {
                    if !selection.is_empty() {
                        draft.target = draft.target.trim_end_matches(" (whole file)").to_string();
                        for selected in selection {
                            let Some(row) = selected["row"].as_u64().and_then(|i| diff.rows.get(i as usize)) else { continue };
                            if matches!(row.kind, RowKind::Hunk | RowKind::Meta) { continue; }
                            let text = selected["text"].as_str().unwrap_or("");
                            let (side, number) = if row.kind == RowKind::Del { ("old",row.old) } else { ("new",row.new) };
                            draft.target.push_str(&format!("\n  {side}:{} | {text}",number.unwrap_or(0)));
                            if draft.locations.is_empty() { draft.line = number; draft.old_side = row.kind == RowKind::Del; }
                            draft.locations.push((row.old,row.new));
                            if draft.preview.len() < 3 {
                                let mut preview = row.clone();
                                preview.segs = vec![git::Seg { text: text.into(), fg: None, bold: false, emph: false }];
                                draft.preview.push(preview);
                            }
                        }
                        if let Some((target,_)) = self.note_box.as_mut() { *target = draft.target.clone(); }
                    }
                }
            }
            "focus" => { self.file_focus = false; self.sidebar = true; }
            "sidebar" => { self.sidebar = !self.sidebar; }
            "send" => self.find_agents(),
            "notes" => self.open_note_list(),
            "picker" => {
                if self.comparison.is_none() { self.picker = Some(crate::project::Picker::new(self.root.clone())); }
                else { self.flash = Some(Flash::new("Press W in the tree for working-tree files".into(),false)); }
            }
            "open" => self.open_path(action["argument"].as_str().unwrap_or("")),
            "source" => {
                if let Ok(line) = action["argument"].as_str().unwrap_or("").parse() { self.goto_line(line); }
            }
            "context" => self.toggle_context(),
            "quit" => self.quit = true,
            _ => {}
        }
    }

    fn toggle_context(&mut self) {
        let location = self.diff.as_ref().and_then(|d| d.rows.get(self.file_cursor.row)).and_then(|r| r.new.or(r.old));
        self.full_context = !self.full_context;
        let revision = self.active.as_ref().map(|p| self.revision(p));
        if let Some((_, d)) = self.cached.iter().find(|(rev,d)| Some(*rev) == revision && Some(&d.path) == self.active.as_ref() && d.context == self.context()) {
            self.diff = Some(d.clone()); self.diff_gen += 1;
            if let Some(line) = location { self.nearest_line(line); }
        } else { self.pending_line = location; self.pending_exact = false; self.request_diff(); }
    }

    fn new(jobs: Sender<Job>, cwd: PathBuf, detected: Option<theme::Detected>) -> Self {
        Self {
            reader: None,
            reader_error: None,
            note_preview_pending: false,
            picker: None,
            inventory: None,
            other_collapsed: HashSet::new(),
            catalog: Vec::new(),
            all_files: false,
            sidebar: true,
            note_list: None,
            editing_note: None,
            command: None,
            root: git2::Repository::discover(&cwd).ok().and_then(|r| r.workdir().map(Path::to_path_buf)).unwrap_or(cwd),
            file_focus: false,
            file_cursor: Position::default(),
            notes: Vec::new(),
            next_note: 1,
            pending_line: None,
            pending_exact: true,
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
            if let Some(reader) = self.reader.as_mut() {
                match reader.poll() {
                    Ok((changed, actions)) => {
                        dirty |= changed || !actions.is_empty();
                        for action in actions { self.reader_action(action); }
                    }
                    Err(e) => { self.reader_failed(e); dirty = true; }
                }
            }
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
                            } else if self.file_focus && self.view == View::Changes {
                                if let Some(reader) = self.reader.as_mut() {
                                    let result = reader.notify("nvim_paste", serde_json::json!([text, true, -1]));
                                    if let Err(e) = result { self.reader_failed(e); }
                                }
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
                    if let Some(reader) = self.reader.as_mut() { reader.generation = None; }
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
                self.set_active(path);
                self.file_focus = true;
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
                    if self.diff.as_ref() == Some(&d) { return; }
                    let revision = self.revision(&d.path);
                    self.cached.retain(|(rev, old)| old.path != d.path || (*rev == revision && old.context != d.context));
                    self.cached.push((revision, d.clone()));
                    if self.cached.len() > 16 { self.cached.remove(0); }
                    let same_file = self.diff.as_ref().is_some_and(|old| old.path == d.path);
                    if !same_file {
                        self.file_cursor = Position::default();
                    }
                    self.file_cursor.row = self.file_cursor.row.min(d.rows.len().saturating_sub(1));
                    self.diff = Some(d);
                    self.diff_gen += 1;
                    if let Some(path) = &self.active { self.document_revision = self.revision(path); }
                    if let Some(line) = self.pending_line.take() {
                        if self.pending_exact { self.goto_line(line); } else { self.nearest_line(line); }
                    }
                    if self.note_list.is_some() { self.preview_note(); }
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
        self.pending_line = None;
        self.file_cursor = Position::default();
        self.cursor = 0;
        self.tree_off = 0;
        self.tree_scrolling = false;
        self.all_files = false;
        self.collapsed.clear();
        self.file_focus = false;
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
            self.file_cursor = Position::default();
            let revision = self.revision(&path);
            let cached = self.cached.iter().find(|(rev,d)| *rev == revision && d.path == path && d.context == self.context())
                .map(|(_,d)| d.clone());
            self.active = Some(path);
            if let Some(diff) = cached { self.on_msg(Msg::Diff(diff)); }
            else { self.request_diff(); }
        }
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
        if self.comparison.is_some() { return; }
        let Some(f) = self.files().get(idx) else { return };
        if f.status == ' ' { return; }
        let add = f.staged != Staged::Fully;
        let _ = self.jobs.send(Job::Stage { path: f.path.clone(), add });
    }

    fn scroll_diff(&mut self, delta: isize) {
        let key = if delta < 0 { "<C-y>" } else { "<C-e>" };
        let count = delta.unsigned_abs().min(50_000);
        if count == 0 { return; }
        if let Some(reader) = self.reader.as_mut() {
            if let Err(e) = reader.notify("nvim_input", serde_json::json!([format!("{count}{key}")])) { self.reader_failed(e); }
        }
    }

    fn keep_file_cursor_visible(&mut self) {
        if let (Some(reader), Some(diff)) = (self.reader.as_mut(), self.diff.as_ref()) {
            if reader.generation == Some(self.diff_gen) {
                if let Err(e) = reader.goto(self.file_cursor, diff) { self.reader_failed(e); }
            } else { reader.seek = true; }
        }
    }

    fn open_note(&mut self) {
        self.editing_note = None;
        if self.annotation_rx.is_some() {
            return;
        }
        let Some(d) = self.diff.as_ref().filter(|d| Some(&d.path) == self.active.as_ref()) else {
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

    fn open_note_list(&mut self) {
        self.note_list = Some(0);
        self.preview_note();
    }

    fn preview_note(&mut self) {
        self.note_preview_pending = false;
        let Some(note) = self.note_list.and_then(|i| self.notes.get(i)).cloned() else { return };
        if self.revision(&note.path) != note.revision { return; }
        self.pending_line = None;
        self.set_active(note.path.clone());
        let Some(diff) = self.diff.as_ref().filter(|d| d.path == note.path) else { return };
        let row = if let Some(location) = note.locations.first() {
            diff.rows.iter().position(|row| (row.old,row.new) == *location)
        } else { Some(0) };
        if let Some(row) = row {
            self.file_cursor = Position {row,col:0};
            self.note_preview_pending = true;
        } else if !self.full_context {
            self.full_context = true;
            self.request_diff();
        }
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

    fn goto_line(&mut self, line: u32) {
        if let Some(d) = &self.diff {
            if let Some(row) = d.rows.iter().position(|r| r.new == Some(line)).or_else(|| d.rows.iter().position(|r| r.old == Some(line))) {
                self.file_cursor = Position { row, col: 0 };
                self.keep_file_cursor_visible();
            } else if !self.full_context {
                self.pending_line = Some(line);
                self.pending_exact = true;
                self.full_context = true;
                self.request_diff();
            } else { self.flash = Some(Flash::new(format!("Line {line} does not exist or exceeds the preview limit"), false)); }
        }
    }

    fn nearest_line(&mut self, line: u32) {
        if let Some(d) = &self.diff && let Some((row,_)) = d.rows.iter().enumerate().filter_map(|(i,r)| r.new.or(r.old).map(|n| (i,n.abs_diff(line)))).min_by_key(|(_,distance)| *distance) {
            self.file_cursor = Position {row,col:0}; self.keep_file_cursor_visible();
        }
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
        if let Some(mut command) = self.command.take() {
            match k.code {
                KeyCode::Esc => return,
                KeyCode::Enter => {
                    if let Some(base) = command.strip_prefix(":base ") {
                        self.review_base = base.trim().to_string();
                        self.open_review(None);
                    } else if let Some(path) = command.strip_prefix(":e ") {
                        self.open_path(path);
                    } else if let Ok(line) = command.trim_start_matches(':').parse::<u32>() {
                        self.goto_line(line);
                    }
                    return;
                }
                KeyCode::Backspace => { command.pop(); }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => command.push(c),
                _ => {}
            }
            self.command = Some(command);
            return;
        }
        if self.note_box.is_none() && let Some(i) = self.note_list {
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') => self.note_list = None,
                KeyCode::Down | KeyCode::Char('j') => self.note_list = Some((i + 1).min(self.notes.len().saturating_sub(1))),
                KeyCode::Up | KeyCode::Char('k') => self.note_list = Some(i.saturating_sub(1)),
                KeyCode::Char('d') if self.annotation_rx.is_none() && !self.notes.is_empty() => {
                    self.notes.remove(i);
                    self.note_list = if self.notes.is_empty() { None } else { Some(i.min(self.notes.len()-1)) };
                }
                KeyCode::Enter if self.annotation_rx.is_none() && !self.notes.is_empty() => {
                    self.note_box = Some((self.notes[i].target.clone(), self.notes[i].comment.clone()));
                    self.note_cursor = self.notes[i].comment.len();
                    self.editing_note = Some(i);
                }
                _ => {}
            }
            self.preview_note();
            return;
        }
        if self.note_box.is_some() {
            self.on_note_key(k);
            return;
        }
        if self.agent_picker.is_some() {
            self.on_agent_key(k);
            return;
        }
        if self.commit_box.is_some() {
            self.on_commit_key(k);
            return;
        }
        // A pending confirmation: `y` goes ahead, any other key cancels.
        if let Some(c) = self.confirm.take() {
            if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.busy = Some(c.busy);
                let _ = self.jobs.send(c.job);
            }
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if self.view == View::Changes && self.file_focus {
            if let Some(reader) = self.reader.as_mut() {
                if let Err(e) = reader.key(k) { self.reader_failed(e); }
            } else if k.code == KeyCode::Tab || k.code == KeyCode::BackTab {
                self.file_focus = false; self.sidebar = true;
            }
            return;
        }
        match k.code {
            KeyCode::Char('q') => return self.quit = true,
            KeyCode::Char('c') if ctrl => return self.quit = true,
            KeyCode::Char('1') => return self.show_view(View::Changes),
            KeyCode::Char('2') => return self.show_view(View::Branches),
            KeyCode::Char('W') => return self.working_tree(),
            _ => {}
        }
        match self.view {
            View::Branches => return self.on_branches_key(k),
            View::History => return self.on_history_key(k),
            View::Changes => {}
        }
        let page = self.areas.diff.height.max(2) as isize - 1;
        match k.code {
            KeyCode::Char('H') => {
                self.hist_branch = "HEAD".into();
                self.history = None;
                self.hist_cursor = 0;
                self.hist_off.set(0);
                self.view = View::History;
                let _ = self.jobs.send(Job::History { branch: self.hist_branch.clone() });
                return;
            }
            KeyCode::Char('D') => { self.open_review(None); return; }
            KeyCode::Char('B') => { self.command = Some(format!(":base {}", self.review_base)); return; }
            KeyCode::Esc if self.comparison.is_some() => {
                if self.comparison.as_ref().is_some_and(|c| c.is_commit) {
                    self.view = View::History;
                    if self.history.is_none() {
                        self.hist_branch = "HEAD".into();
                        let _ = self.jobs.send(Job::History { branch: self.hist_branch.clone() });
                    }
                } else { self.working_tree(); }
                return;
            }
            KeyCode::Char('p') if ctrl => { if self.comparison.is_none() { self.picker = Some(crate::project::Picker::new(self.root.clone())); } else { self.flash = Some(Flash::new("Press W for working-tree files".into(), false)); } return; }
            KeyCode::Char('t') => {
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
                self.update_catalog(); self.rebuild(None); return;
            }
            KeyCode::Char('L') => { self.open_note_list(); return; }
            KeyCode::Char('\\') => { self.sidebar = !self.sidebar; self.file_focus = !self.sidebar; return; }
            KeyCode::Tab | KeyCode::BackTab => {
                self.file_focus = !self.file_focus;
                if !self.file_focus { self.sidebar = true; }
                return;
            }
            KeyCode::Char('a') => {
                self.open_note();
                return;
            }
            KeyCode::Char('s') => {
                self.find_agents();
                return;
            }
            _ => {}
        }
        match k.code {
            KeyCode::Char('c') if !ctrl => self.open_commit_box(),
            KeyCode::Char('d') if !ctrl => self.ask_discard(),
            KeyCode::Char('d') if ctrl => self.scroll_diff(page / 2),
            KeyCode::Char('u') if ctrl => self.scroll_diff(-page / 2),
            KeyCode::Up | KeyCode::Char('k') => self.move_row(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_row(1),
            KeyCode::Left | KeyCode::Char('h') => self.tree_motion(false),
            KeyCode::Right | KeyCode::Char('l') => self.tree_motion(true),
            KeyCode::Char(' ') => self.toggle_here(),
            KeyCode::Enter => { self.sync_active_from_cursor(); self.file_focus = true; }
            KeyCode::Char('J') => self.scroll_diff(1),
            KeyCode::Char('K') => self.scroll_diff(-1),
            KeyCode::Char('f') | KeyCode::PageDown => self.scroll_diff(page),
            KeyCode::Char('b') | KeyCode::PageUp => self.scroll_diff(-page),
            KeyCode::Char('g') | KeyCode::Home => { self.file_cursor = Position::default(); self.keep_file_cursor_visible(); }
            KeyCode::Char('G') | KeyCode::End => self.scroll_diff(isize::MAX / 2),
            KeyCode::Char('x') => self.toggle_context(),
            KeyCode::Char('+') | KeyCode::Char('=') if self.areas.side => {
                self.tree_pref_w = Some(self.areas.tree_frame.width + 2);
            }
            KeyCode::Char('-') if self.areas.side => {
                self.tree_pref_w = Some(self.areas.tree_frame.width.saturating_sub(2));
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.tree_pref = Some(self.areas.tree_frame.height + 1);
            }
            KeyCode::Char('-') => {
                self.tree_pref = Some(self.areas.tree_frame.height.saturating_sub(1).max(1));
            }
            KeyCode::Char('r') => {
                self.cached.clear();
                if self.comparison.is_some() { self.request_diff(); } else { let _ = self.jobs.send(Job::Reload); }
            }
            _ => {}
        }
    }

    fn show_view(&mut self, view: View) {
        if view == View::Branches {
            let _ = self.jobs.send(Job::Branches);
        }
        self.view = view;
    }

    fn selected_branch(&self) -> Option<&BranchInfo> {
        match &self.branches {
            Some(Ok(l)) => l.get(self.branch_cursor),
            _ => None,
        }
    }

    /// Branches screen: move, `space`/`Enter` checks out, `p` pulls, `P` pushes,
    /// `m` merges the selected branch into the current one.
    fn on_branches_key(&mut self, k: KeyEvent) {
        let len = match &self.branches {
            Some(Ok(l)) => l.len(),
            _ => 0,
        };
        let move_by = |c: usize, d: isize| (c as isize + d).clamp(0, len.saturating_sub(1) as isize) as usize;
        match k.code {
            KeyCode::Esc => self.view = View::Changes,
            KeyCode::Up | KeyCode::Char('k') => self.branch_cursor = move_by(self.branch_cursor, -1),
            KeyCode::Down | KeyCode::Char('j') => self.branch_cursor = move_by(self.branch_cursor, 1),
            KeyCode::Home | KeyCode::Char('g') => self.branch_cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.branch_cursor = len.saturating_sub(1),
            KeyCode::Char(' ') => self.checkout_selected(),
            KeyCode::Enter => self.open_history(),
            KeyCode::Char('P') => self.push_selected(),
            KeyCode::Char('p') => self.pull_selected(),
            KeyCode::Char('m') => self.merge_selected(),
            KeyCode::Char('d') => self.ask_delete_branch(false),
            KeyCode::Char('f') => self.fetch(),
            KeyCode::Char('D') => self.ask_delete_branch(true),
            KeyCode::Char('r') => {
                let _ = self.jobs.send(Job::Branches);
            }
            _ => {}
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

    /// History screen: move through the commits, Esc goes back to Branches.
    fn on_history_key(&mut self, k: KeyEvent) {
        let len = self.history_len();
        let page = (self.areas.diff.height.max(2) as isize).max(10);
        let move_by = |c: usize, d: isize| (c as isize + d).clamp(0, len.saturating_sub(1) as isize) as usize;
        match k.code {
            KeyCode::Enter => {
                if let Some(Ok(commits)) = &self.history && let Some(commit) = commits.get(self.hist_cursor) {
                    self.open_review(Some(commit.oid.clone()));
                }
            }
            KeyCode::Char('D') => self.open_review(None),
            KeyCode::Char('B') => self.command = Some(format!(":base {}", self.review_base)),
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.view = View::Branches,
            KeyCode::Up | KeyCode::Char('k') => self.hist_cursor = move_by(self.hist_cursor, -1),
            KeyCode::Down | KeyCode::Char('j') => self.hist_cursor = move_by(self.hist_cursor, 1),
            KeyCode::PageDown | KeyCode::Char('f') => self.hist_cursor = move_by(self.hist_cursor, page),
            KeyCode::PageUp | KeyCode::Char('b') => self.hist_cursor = move_by(self.hist_cursor, -page),
            KeyCode::Home | KeyCode::Char('g') => self.hist_cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.hist_cursor = len.saturating_sub(1),
            _ => {}
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
        if self.picker.is_some() || self.command.is_some() || self.note_list.is_some() || self.note_box.is_some() || self.agent_picker.is_some() || self.commit_box.is_some() || self.confirm.is_some()
        {
            return;
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
        if (inside(a.diff) || (self.file_focus && matches!(m.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_)))) && !self.dragging {
            if matches!(m.kind, MouseEventKind::Down(_)) { self.file_focus = true; }
            if let Some(reader) = self.reader.as_mut() {
                if let Err(e) = reader.mouse(m, a.diff) { self.reader_failed(e); }
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
                self.file_focus = false;
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
        if self.note_list.is_none() { return Rect::default(); }
        let wanted = (self.notes.len().min(4) as u16 * 2 + 2).max(6);
        let height = wanted.min((area.height / 3).max(4)).min(area.height.saturating_sub(8));
        Rect::new(area.x, area.bottom().saturating_sub(height), area.width, height)
    }

    fn layout(&mut self, mut area: Rect) {
        let previous_diff = self.areas.diff;
        let dock = self.note_list_area(area);
        if !dock.is_empty() { area.height = area.height.saturating_sub(dock.height); }
        self.areas = if self.sidebar && !(self.note_list.is_some() && area.width < SIDE_MIN_WIDTH) {
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
        if self.note_list.is_some() && self.areas.diff != previous_diff { self.preview_note(); }
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
        Areas { side: false, tree, splitter, title, diff, tree_frame: Rect::default(), file_frame: Rect::default() }
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
        Areas { side: true, tree, splitter, title, diff, tree_frame: Rect::default(), file_frame: Rect::default() }
    }

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        if area.width < 10 || area.height < 6 {
            return;
        }
        self.layout(area);
        self.sync_reader();
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
                return;
            }
            View::History => {
                self.draw_history(buf, area);
                self.draw_footer(buf, area);
                return;
            }
            View::Changes => {}
        }

        if files.is_empty() && self.active.is_none() {
            let rest = Rect::new(area.x, area.y + 2, area.width, area.height - 2);
            center(buf, rest, if self.comparison.is_some() { "No changes in this comparison" } else { "No changes ✓  working tree clean" }, p.faint);
            // Still show the footer: the result of a commit that took everything lands here.
            self.draw_footer(buf, area);
            return;
        }

        self.draw_tree(buf);
        self.draw_splitter(buf, area);
        self.draw_diff(buf);
        self.draw_pane_borders(buf);
        let cursor = self.draw_footer(buf, area);
        if let Some(pos) = cursor {
            f.set_cursor_position(pos);
        }
    }

    fn draw_pane_borders(&self, buf: &mut Buffer) {
        use ratatui::widgets::{Block, BorderType, Widget};
        for (area, focused, title) in [
            (self.areas.tree_frame, !self.file_focus, " Files "),
            (self.areas.file_frame, self.file_focus, " File "),
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
        if self.file_focus {
            buf.set_style(t, Style::new().bg(p.sel_bg));
        }
        let right = t.x + t.width - 1;
        let mut lim = right;
        if let Some(d) = self.diff.as_ref().filter(|d| Some(&d.path) == self.active.as_ref()) {
            let mut info = format!("Neovim · {} rows", d.rows.len());
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
        if let Some(error) = &self.reader_error { center(buf, self.areas.diff, error, self.pal.red); return; }
        if let Some(reader) = &self.reader {
            if self.diff.as_ref().is_some_and(|d| Some(&d.path) == self.active.as_ref()) {
                reader.draw(self.areas.diff, buf, self.file_focus && self.note_box.is_none() && self.picker.is_none() && self.note_list.is_none());
            } else { center(buf, self.areas.diff, "Loading…", self.pal.faint); }
            return;
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
        let block = Block::bordered().border_type(BorderType::Rounded)
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
        let y = area.y + area.height - 1;
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
            return Some((x.min(right.saturating_sub(1)), y));
        }

        if let Some((_, msg)) = &self.note_box {
            use ratatui::widgets::{Block, Clear, Widget};
            let height = area.height.min(8);
            let popup = Rect::new(area.x, area.bottom().saturating_sub(height), area.width, height);
            Clear.render(popup, buf);
            let block = Block::bordered().title(" Note · Enter save · Shift+Enter newline · Esc cancel ");
            let inner = block.inner(popup);
            block.render(popup, buf);
            if inner.is_empty() { return None; }
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
        if let Some(i) = self.note_list {
            self.draw_note_list(buf,area,i);
            return None;
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
            put(buf, &mut x, y, right, "y", Style::new().fg(p.muted));
            put(buf, &mut x, y, right, &format!(" {}   ", c.verb), Style::new().fg(p.faint));
            put(buf, &mut x, y, right, "any key", Style::new().fg(p.muted));
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
        let hints: &[(&str, &str)] = match self.view {
            View::Changes if self.file_focus => &[
                ("Space+a", "annotate"), ("Space+s", "send"), ("Space+l", "notes"),
                ("Space+x", "whole file"), ("Space+p", "open"), ("/", "search"),
                ("Tab", "tree"), (":q", "quit"),
            ],
            View::Changes if self.comparison.as_ref().is_some_and(|c| !c.is_commit) => &[
                ("Esc", "working tree"), ("W", "working tree"), ("B", "base"),
                ("Tab", "focus"), ("x", "whole file"), ("a", "annotate"), ("L", "notes"), ("s", "send"),
            ],
            View::Changes if self.comparison.is_some() => &[
                ("W", "working tree"), ("Esc", "commits"), ("D", "branch diff"), ("B", "base"),
                ("Tab", "focus"), ("x", "whole file"), ("a", "annotate"), ("L", "notes"), ("s", "send"),
            ],
            View::Changes => &[
                ("H", "commits"), ("D", "branch diff"), ("t", "all/changed"), ("Ctrl+p", "open"), ("L", "notes"),
                ("Tab", "file"),
                ("a", "annotate file"),
                ("s", "send notes"),
                ("↑↓", "navigate"),
                ("←→", "folders"),
                ("space", "stage/fold"),
                ("c", "commit"),
                ("d", "discard"),
                ("x", "whole file"),
                ("2", "branches"),
                ("q", "quit"),
            ],
            View::History => &[("↑↓", "move"), ("Enter", "diff"), ("D", "branch diff"), ("B", "base"), ("W", "working tree"), ("esc", "back"), ("q", "quit")],
            View::Branches => &[
                ("↑↓", "move"),
                ("space", "checkout"),
                ("p", "pull"),
                ("P", "push"),
                ("m", "merge into current"),
                ("d", "delete"),
                ("f", "fetch"),
                ("⏎", "history"),
                ("1", "changes"),
                ("q", "quit"),
            ],
        };
        for (key, what) in hints {
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

    #[test]
    fn annotation_list_highlights_selection_and_reserves_reader_space() {
        let (jobs,_) = mpsc::channel();
        let mut app=App::new(jobs,PathBuf::from("/tmp/rexdiff-test"),None);
        app.notes=(0..10).map(|i| annotations::Note {id:i,path:"x.rs".into(),line:Some(44),old_side:true,
            locations:vec![(Some(44),None),(Some(45),None),(Some(46),None),(Some(47),None)],
            revision:0,target:String::new(),preview:Vec::new(),historical:true,comment:format!("comment {i}")}).collect();
        app.note_list=Some(9);
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
        app.note_list=None;
        app.layout(Rect::new(0,0,80,24));
        assert!(!app.areas.tree_frame.is_empty());
    }

    #[test]
    fn annotation_preview_waits_for_its_file_and_matches_the_exact_diff_side() {
        let (jobs,rx)=mpsc::channel();
        let mut app=App::new(jobs,PathBuf::from("/tmp/rexdiff-test"),None);
        let revision=app.revision("x.rs");
        let row=|old: Option<u32>,new: Option<u32>| DRow {kind:if new.is_none(){RowKind::Del}else{RowKind::Add},old,new,
            segs:vec![Seg{text:"example".into(),fg:None,bold:false,emph:false}]};
        app.notes=vec![annotations::Note {id:1,path:"x.rs".into(),line:Some(45),old_side:false,
            locations:vec![(None,Some(45))],revision,target:String::new(),preview:Vec::new(),historical:false,comment:"eh".into()}];
        app.active=Some("other.rs".into());
        app.diff=Some(FileDiff {revision,path:"other.rs".into(),context:CONTEXT,max_lineno:45,rows:vec![row(Some(45),None)]});
        app.open_note_list();
        assert_eq!(app.active.as_deref(),Some("x.rs"));
        assert!(!app.note_preview_pending);
        assert!(matches!(rx.try_recv(),Ok(Job::Diff {..})));
        app.on_msg(Msg::Diff(FileDiff {revision,path:"x.rs".into(),context:CONTEXT,max_lineno:45,
            rows:vec![row(Some(45),None),row(None,Some(45))]}));
        assert!(app.note_preview_pending);
        assert_eq!(app.file_cursor.row,1);
        app.notes[0].locations=vec![(Some(45),None)];
        app.preview_note();
        assert_eq!(app.file_cursor.row,0);
        app.set_active("other.rs".into());
        assert!(matches!(rx.try_recv(),Ok(Job::Diff {..})));
        app.on_msg(Msg::Diff(FileDiff {revision:app.revision("other.rs"),path:"other.rs".into(),context:CONTEXT,
            max_lineno:45,rows:vec![row(Some(45),None)]}));
        // The open list returns to its selected note without another diff job.
        assert_eq!(app.active.as_deref(),Some("x.rs"));
        assert_eq!(app.diff.as_ref().unwrap().path,"x.rs");
        assert!(app.note_preview_pending);
        assert!(rx.try_recv().is_err());
        assert!(app.cached.iter().any(|(_,d)|d.path=="other.rs"));
        app.note_list=None;
        app.set_active("other.rs".into());
        assert!(rx.try_recv().is_err());
        for (rev,d) in &mut app.cached { if d.path=="x.rs" { *rev=revision.wrapping_add(1); } }
        app.set_active("x.rs".into());
        assert!(matches!(rx.try_recv(),Ok(Job::Diff {..})));
        app.note_list=Some(0);
        app.notes[0].revision=revision.wrapping_add(1);
        app.preview_note();
        assert!(!app.note_preview_pending);
    }

    #[test]
    fn native_selection_keeps_deleted_unicode_and_snapshot_coordinates() {
        let (jobs, _) = mpsc::channel();
        let mut app = App::new(jobs, PathBuf::from("/tmp/rexdiff-test"), None);
        app.active = Some("x.rs".into());
        app.diff_gen = 7;
        app.document_revision = 42;
        app.diff = Some(FileDiff { revision:42, path:"x.rs".into(), context:3, max_lineno:9,
            rows:vec![DRow {kind:RowKind::Del,old:Some(9),new:None,
                segs:vec![Seg{text:"aé中z".into(),fg:None,bold:false,emph:false}]}] });
        app.reader_action(serde_json::json!({"action":"annotate","generation":7,"row":0,"byte":1,
            "selection":[{"row":0,"text":"é中"}]}));
        let note = app.draft_note.as_ref().unwrap();
        assert!(note.target.ends_with("\n  old:9 | é中"));
        assert_eq!(note.locations,vec![(Some(9),None)]);
        assert_eq!(note.line,Some(9));
        assert!(note.old_side);
        assert_eq!(note.revision,42);
        assert_eq!(annotations::text(&note.preview[0]),"é中");
        app.note_box = None;
        app.open_note();
        assert!(app.draft_note.as_ref().unwrap().target.ends_with("(whole file)"));
    }

    #[test]
    fn focus_select_annotate_and_cancel_picker() {
        let (jobs, _) = mpsc::channel();
        let mut app = App::new(jobs, PathBuf::from("/tmp/rexdiff-test"), None);
        app.active = Some("test.rs".into());
        app.diff = Some(FileDiff {
            revision: 0,
            path: "test.rs".into(),
            context: 3,
            max_lineno: 3,
            rows: (1..=3)
                .map(|n| DRow {
                    kind: RowKind::Add,
                    old: None,
                    new: Some(n),
                    segs: vec![Seg { text: format!("line {n}"), fg: None, bold: false, emph: false }],
                })
                .collect(),
        });
        app.diff_gen = 1;
        app.reader_action(serde_json::json!({"action":"annotate","generation":0,"row":1,"byte":0,
            "selection":[{"row":0,"text":"stale"}]}));
        assert!(app.note_box.is_none());
        app.layout(Rect::new(0, 0, 80, 24));
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        app.on_key(key(KeyCode::Tab));
        assert!(app.file_focus);
        app.reader_action(serde_json::json!({"action":"annotate","generation":1,"row":1,"byte":0,
            "selection":[{"row":0,"text":"line 1"},{"row":1,"text":"line 2"}]}));
        assert_eq!(app.file_cursor.row, 1);
        assert_eq!(app.cursor, 0);
        app.on_key(key(KeyCode::Char('q')));
        assert!(!app.quit);
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.notes.len(), 1);
        assert!(app.notes[0].format().contains("new:1 | line 1\n  new:2 | line 2"));
        assert!(app.notes[0].format().ends_with("Comment: q"));
        app.on_key(key(KeyCode::Tab));
        assert!(!app.file_focus);
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
        app.on_key(key(KeyCode::Char('L')));
        app.on_key(key(KeyCode::Enter));
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        app.on_key(key(KeyCode::Char('z')));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.notes[0].comment, "z");
        app.on_key(key(KeyCode::Char('d')));
        assert!(app.notes.is_empty());
        assert!(app.note_list.is_none());
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
            let area = Rect::new(0, 0, 180, 24);
            let mut buffer = Buffer::empty(area);
            app.draw_footer(&mut buffer, area);
            let footer: String = (0..area.width).map(|x| buffer[(x, area.bottom()-1)].symbol()).collect();
            assert_eq!(footer.contains("D branch diff"), is_commit);
            assert!(footer.contains(if is_commit { "Esc commits" } else { "Esc working tree" }));
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
        app.file_focus = false;
        app.layout(Rect::new(0,0,40,20));
        assert_eq!(app.areas.tree_frame.width,40);
        assert_eq!(app.areas.tree.width,38);
        assert!(app.areas.diff.height > 0);
        assert!(app.areas.tree.y < app.areas.diff.y);
        assert!(!app.areas.side);
        app.file_focus = true;
        app.layout(Rect::new(0,0,40,20));
        assert!(app.areas.tree.height > 0);
        assert!(app.areas.diff.height > 0);
        app.layout(Rect::new(0,0,80,24));
        let tree = app.areas.tree_frame;
        let file = app.areas.file_frame;
        for file_focus in [false, true] {
            app.file_focus = file_focus;
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
}
