//! Reads the repository on its own thread: the list of changed files and the
//! diff (with syntax colors and word-level highlights) of the selected file.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use git2::{Delta, DiffOptions, Patch, Repository, Tree};

use crate::highlight::{Highlighter, Span};

/// Larger diffs are truncated (nobody reads 50k lines in a pane).
const MAX_ROWS: usize = 50_000;

pub struct FileEntry {
    pub path: String,
    pub status: char,
    pub adds: usize,
    pub dels: usize,
    pub binary: bool,
}

pub struct Snapshot {
    pub branch: String,
    pub files: Vec<FileEntry>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Hunk,
    Ctx,
    Add,
    Del,
    Meta,
}

/// A run of text with one color. `fg` is an index into the terminal's ANSI
/// palette (so syntax colors follow the Rex theme); `None` is the default color.
pub struct Seg {
    pub text: String,
    pub fg: Option<u8>,
    pub bold: bool,
    pub emph: bool,
}

pub struct DRow {
    pub kind: RowKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub segs: Vec<Seg>,
}

pub struct FileDiff {
    pub path: String,
    pub context: u32,
    pub rows: Vec<DRow>,
    pub max_lineno: u32,
}

pub enum Job {
    Refresh,
    Diff { path: String, context: u32 },
}

pub enum Msg {
    Snapshot(Result<Snapshot, String>),
    Diff(FileDiff),
}

pub fn run_worker(cwd: PathBuf, rx: Receiver<Job>, tx: Sender<Msg>) {
    let hl = Highlighter::new();
    let mut repo: Option<Repository> = None;

    while let Ok(first) = rx.recv() {
        // Coalesce queued requests: only the latest of each kind matters.
        let mut refresh = false;
        let mut diff_job = None;
        for job in std::iter::once(first).chain(rx.try_iter()) {
            match job {
                Job::Refresh => refresh = true,
                Job::Diff { path, context } => diff_job = Some((path, context)),
            }
        }

        if repo.is_none() {
            repo = Repository::discover(&cwd).ok();
        }
        let Some(r) = repo.as_ref() else {
            let _ = tx.send(Msg::Snapshot(Err("This folder is not a git repository".into())));
            continue;
        };

        if refresh {
            let snap = snapshot(r).map_err(|e| e.message().to_string());
            if tx.send(Msg::Snapshot(snap)).is_err() {
                return;
            }
        }
        if let Some((path, context)) = diff_job {
            let diff = file_diff(r, &hl, &path, context);
            if tx.send(Msg::Diff(diff)).is_err() {
                return;
            }
        }
    }
}

fn base_opts() -> DiffOptions {
    let mut o = DiffOptions::new();
    o.include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true)
        .include_typechange(true);
    o
}

fn head_tree(repo: &Repository) -> Option<Tree<'_>> {
    repo.head().ok()?.peel_to_tree().ok()
}

fn branch_name(repo: &Repository) -> String {
    match repo.head() {
        Ok(h) if h.is_branch() => h.shorthand().unwrap_or("?").to_string(),
        Ok(h) => h
            .target()
            .map(|o| o.to_string()[..7].to_string())
            .unwrap_or_else(|| "detached".into()),
        // Repository without commits: HEAD points to a branch that doesn't exist yet.
        Err(_) => repo
            .find_reference("HEAD")
            .ok()
            .and_then(|r| r.symbolic_target().ok().flatten().map(|t| t.trim_start_matches("refs/heads/").to_string()))
            .unwrap_or_else(|| "?".into()),
    }
}

fn status_char(d: Delta) -> char {
    match d {
        Delta::Added | Delta::Untracked => 'A',
        Delta::Deleted => 'D',
        Delta::Modified => 'M',
        Delta::Renamed => 'R',
        Delta::Copied => 'C',
        Delta::Typechange => 'T',
        Delta::Conflicted => 'U',
        _ => '?',
    }
}

fn snapshot(repo: &Repository) -> Result<Snapshot, git2::Error> {
    // Re-read the index if it changed on disk (after a `git add`, for example).
    if let Ok(mut idx) = repo.index() {
        let _ = idx.read(false);
    }
    let tree = head_tree(repo);
    let mut opts = base_opts();
    opts.context_lines(0);
    let diff = repo.diff_tree_to_workdir_with_index(tree.as_ref(), Some(&mut opts))?;

    let mut files = Vec::with_capacity(diff.deltas().len());
    for i in 0..diff.deltas().len() {
        let delta = diff.get_delta(i).expect("delta in range");
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (adds, dels, binary) = match Patch::from_diff(&diff, i)? {
            Some(p) => {
                let (_, a, d) = p.line_stats()?;
                (a, d, p.delta().flags().is_binary())
            }
            None => (0, 0, true),
        };
        files.push(FileEntry { path, status: status_char(delta.status()), adds, dels, binary });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Snapshot { branch: branch_name(repo), files })
}

fn meta(text: &str) -> DRow {
    DRow {
        kind: RowKind::Meta,
        old: None,
        new: None,
        segs: vec![Seg { text: text.into(), fg: None, bold: false, emph: false }],
    }
}

/// A raw line, before it gets colors.
struct Raw {
    kind: RowKind,
    old: Option<u32>,
    new: Option<u32>,
    text: String,
}

fn file_diff(repo: &Repository, hl: &Highlighter, path: &str, context: u32) -> FileDiff {
    let mut out = FileDiff { path: path.to_string(), context, rows: Vec::new(), max_lineno: 0 };
    match collect_raw(repo, path, context) {
        Ok(Some(raw)) => {
            out.max_lineno = raw.iter().filter_map(|r| r.new.max(r.old)).max().unwrap_or(0);
            out.rows = colorize(raw, hl, path);
        }
        Ok(None) => out.rows.push(meta("Binary file — no text diff")),
        Err(e) => out.rows.push(meta(&format!("Could not read the diff: {}", e.message()))),
    }
    if out.rows.is_empty() {
        out.rows.push(meta("No content changes (permissions only, or an empty file)"));
    }
    out
}

fn collect_raw(repo: &Repository, path: &str, context: u32) -> Result<Option<Vec<Raw>>, git2::Error> {
    let tree = head_tree(repo);
    let mut opts = base_opts();
    opts.context_lines(context).pathspec(path).disable_pathspec_match(true);
    let diff = repo.diff_tree_to_workdir_with_index(tree.as_ref(), Some(&mut opts))?;
    if diff.deltas().len() == 0 {
        return Ok(Some(Vec::new()));
    }
    let Some(patch) = Patch::from_diff(&diff, 0)? else { return Ok(None) };
    if patch.delta().flags().is_binary() {
        return Ok(None);
    }

    let mut raw = Vec::new();
    'hunks: for h in 0..patch.num_hunks() {
        let (hunk, n) = patch.hunk(h)?;
        let header = String::from_utf8_lossy(hunk.header()).trim_end().to_string();
        raw.push(Raw { kind: RowKind::Hunk, old: None, new: None, text: header });
        for l in 0..n {
            if raw.len() >= MAX_ROWS {
                raw.push(Raw {
                    kind: RowKind::Meta,
                    old: None,
                    new: None,
                    text: format!("… diff truncated to the first {MAX_ROWS} lines"),
                });
                break 'hunks;
            }
            let line = patch.line_in_hunk(h, l)?;
            let text = String::from_utf8_lossy(line.content())
                .trim_end_matches(['\n', '\r'])
                .to_string();
            let kind = match line.origin() {
                ' ' => RowKind::Ctx,
                '+' => RowKind::Add,
                '-' => RowKind::Del,
                '=' | '>' | '<' => {
                    raw.push(Raw {
                        kind: RowKind::Meta,
                        old: None,
                        new: None,
                        text: "⏎ no newline at end of file".into(),
                    });
                    continue;
                }
                _ => continue,
            };
            raw.push(Raw { kind, old: line.old_lineno(), new: line.new_lineno(), text });
        }
    }
    Ok(Some(raw))
}

/// Applies syntax colors and highlights the words that changed.
fn colorize(raw: Vec<Raw>, hl: &Highlighter, path: &str) -> Vec<DRow> {
    let emph = word_emphasis(&raw);
    let mut file_hl = hl.for_path(path, raw.len());

    raw.into_iter()
        .zip(emph)
        .map(|(r, em)| {
            let spans = match r.kind {
                RowKind::Hunk => {
                    file_hl.reset();
                    Vec::new()
                }
                RowKind::Meta => Vec::new(),
                RowKind::Ctx => file_hl.both(&r.text),
                RowKind::Add => file_hl.new_side(&r.text),
                RowKind::Del => file_hl.old_side(&r.text),
            };
            let segs = match r.kind {
                RowKind::Hunk | RowKind::Meta => {
                    vec![Seg { text: sanitize(&r.text), fg: None, bold: false, emph: false }]
                }
                _ => build_segs(&r.text, &spans, &em),
            };
            DRow { kind: r.kind, old: r.old, new: r.new, segs }
        })
        .collect()
}

/// For each "deleted lines followed by added lines" block, pairs the lines up
/// and marks the parts that actually changed.
fn word_emphasis(raw: &[Raw]) -> Vec<Vec<Range<usize>>> {
    let mut out: Vec<Vec<Range<usize>>> = vec![Vec::new(); raw.len()];
    let mut i = 0;
    while i < raw.len() {
        if raw[i].kind != RowKind::Del {
            i += 1;
            continue;
        }
        let del_start = i;
        while i < raw.len() && raw[i].kind == RowKind::Del {
            i += 1;
        }
        let add_start = i;
        while i < raw.len() && raw[i].kind == RowKind::Add {
            i += 1;
        }
        let pairs = (add_start - del_start).min(i - add_start);
        for k in 0..pairs {
            let (o, n) = (del_start + k, add_start + k);
            let (old, new) = (&raw[o].text, &raw[n].text);
            if old.len() > 2000 || new.len() > 2000 {
                continue;
            }
            let d = similar::TextDiff::from_words(old.as_str(), new.as_str());
            // Very different lines: highlighting everything is just noise.
            if d.ratio() < 0.35 {
                continue;
            }
            let (mut oo, mut no) = (0usize, 0usize);
            for ch in d.iter_all_changes() {
                let len = ch.value().len();
                match ch.tag() {
                    similar::ChangeTag::Equal => {
                        oo += len;
                        no += len;
                    }
                    similar::ChangeTag::Delete => {
                        push_range(&mut out[o], oo..oo + len);
                        oo += len;
                    }
                    similar::ChangeTag::Insert => {
                        push_range(&mut out[n], no..no + len);
                        no += len;
                    }
                }
            }
        }
    }
    out
}

fn push_range(v: &mut Vec<Range<usize>>, r: Range<usize>) {
    if let Some(last) = v.last_mut()
        && last.end == r.start
    {
        last.end = r.end;
        return;
    }
    v.push(r);
}

fn build_segs(text: &str, spans: &[Span], emph: &[Range<usize>]) -> Vec<Seg> {
    let mut cuts: Vec<usize> = vec![0, text.len()];
    for s in spans {
        cuts.push(s.range.start);
        cuts.push(s.range.end);
    }
    for r in emph {
        cuts.push(r.start);
        cuts.push(r.end);
    }
    cuts.retain(|&c| c <= text.len() && text.is_char_boundary(c));
    cuts.sort_unstable();
    cuts.dedup();

    let mut segs: Vec<Seg> = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b {
            continue;
        }
        let span = spans.iter().find(|s| s.range.start <= a && a < s.range.end);
        let fg = span.and_then(|s| s.fg);
        let bold = span.is_some_and(|s| s.bold);
        let em = emph.iter().any(|r| r.start <= a && a < r.end);
        let piece = sanitize(&text[a..b]);
        match segs.last_mut() {
            Some(l) if l.fg == fg && l.bold == bold && l.emph == em => l.text.push_str(&piece),
            _ => segs.push(Seg { text: piece, fg, bold, emph: em }),
        }
    }
    segs
}

/// Tabs become spaces and control characters never reach the terminal.
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => out.push('·'),
            c => out.push(c),
        }
    }
    out
}
