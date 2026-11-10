//! The native file reader: cursor, selection, search, hunk targets, and the
//! stacked/split rendering of one diff document. The app owns the document;
//! the reader keeps everything derived from it, cached per generation.

use std::cell::{RefCell, RefMut};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::annotations::{self, Note, Position, Selection};
use crate::git::{FileDiff, RowKind};
use crate::navigation::{self, Hit, Search};
use crate::theme::Palette;
use crate::wrap::{WrapCache, Wrapped, wrap_row};

/// Narrowest text column of each split pane; below it the reader stacks.
const MIN_SPLIT_TEXT: u16 = 16;
/// Columns `zh` / `zl` scroll per count.
const HSCROLL_STEP: usize = 4;

/// Cursor and viewport movements inside the reader.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordNext,
    WordBack,
    WordEnd,
    LineStart,
    FirstNonBlank,
    LineEnd,
    Top,
    Bottom,
    HalfDown,
    HalfUp,
    PageDown,
    PageUp,
    ScrollDown,
    ScrollUp,
    PlaceTop,
    PlaceCenter,
    PlaceBottom,
    ScrollLeft,
    ScrollRight,
}

/// One display row of the split layout, as indices into the diff rows;
/// `None` is filler on the shorter side of a change.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SplitRow {
    pub old: Option<usize>,
    pub new: Option<usize>,
}

/// Where `[` / `]` land: the first changed row of a hunk, the row to show
/// above it, and whether the hunk only deletes lines.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Target {
    row: usize,
    anchor: usize,
    old_side: bool,
}

type RowKey = (RowKind, Option<u32>, Option<u32>);
/// A row's old and new line numbers, as notes store them.
type RowLines = (Option<u32>, Option<u32>);

fn is_source(kind: RowKind) -> bool {
    !matches!(kind, RowKind::Hunk | RowKind::Meta)
}

fn changed(kind: RowKind) -> bool {
    matches!(kind, RowKind::Add | RowKind::Del)
}

/// Everything derived from the document's text and line numbers.
#[derive(Default)]
struct Doc {
    /// Bumps whenever text or line numbers change (not for highlighting).
    revision: u64,
    texts: Vec<String>,
    keys: Vec<RowKey>,
    split: Vec<SplitRow>,
    split_of: Vec<usize>,
    stacked_rows: Vec<usize>,
    old_rows: Vec<usize>,
    new_rows: Vec<usize>,
    targets: Vec<Target>,
}

impl Doc {
    fn new(texts: Vec<String>, keys: Vec<RowKey>, whole: bool, revision: u64) -> Self {
        let n = keys.len();
        let mut split = Vec::new();
        let mut split_of = vec![0; n];
        let mut i = 0;
        while i < n {
            if changed(keys[i].0) {
                let start = i;
                while i < n && changed(keys[i].0) {
                    i += 1;
                }
                let dels: Vec<usize> = (start..i).filter(|&r| keys[r].0 == RowKind::Del).collect();
                let adds: Vec<usize> = (start..i).filter(|&r| keys[r].0 == RowKind::Add).collect();
                for k in 0..dels.len().max(adds.len()) {
                    let (old, new) = (dels.get(k).copied(), adds.get(k).copied());
                    for r in old.into_iter().chain(new) {
                        split_of[r] = split.len();
                    }
                    split.push(SplitRow { old, new });
                }
            } else {
                split_of[i] = split.len();
                split.push(SplitRow { old: Some(i), new: Some(i) });
                i += 1;
            }
        }
        let rows = |keep: fn(RowKind) -> bool| (0..n).filter(|&r| keep(keys[r].0)).collect::<Vec<_>>();
        Self {
            revision,
            stacked_rows: rows(is_source),
            old_rows: rows(|k| matches!(k, RowKind::Del | RowKind::Ctx)),
            new_rows: rows(|k| matches!(k, RowKind::Add | RowKind::Ctx)),
            targets: targets(&keys, whole),
            texts,
            keys,
            split,
            split_of,
        }
    }
}

/// Hunk targets: in hunk mode, the first change after each header; in
/// whole-file mode, the start of each run of changes.
fn targets(keys: &[RowKey], whole: bool) -> Vec<Target> {
    let mut out = Vec::new();
    let deleted_only = |range: std::ops::Range<usize>| !range.into_iter().any(|r| keys[r].0 == RowKind::Add);
    if !whole && keys.iter().any(|k| k.0 == RowKind::Hunk) {
        let mut start = 0;
        while start < keys.len() {
            let end = (start + 1..keys.len()).find(|&r| keys[r].0 == RowKind::Hunk).unwrap_or(keys.len());
            if let Some(row) = (start..end).find(|&r| changed(keys[r].0)) {
                let anchor = if keys[start].0 == RowKind::Hunk { start } else { row };
                out.push(Target { row, anchor, old_side: deleted_only(start..end) });
            }
            start = end;
        }
    } else {
        let (mut r, mut previous_end) = (0, 0);
        while r < keys.len() {
            if !changed(keys[r].0) {
                r += 1;
                continue;
            }
            let start = r;
            while r < keys.len() && changed(keys[r].0) {
                r += 1;
            }
            let anchor = start.saturating_sub(3).max(previous_end);
            out.push(Target { row: start, anchor, old_side: deleted_only(start..r) });
            previous_end = r;
        }
    }
    out
}

/// A note card drawn above the first annotated row.
struct Card {
    lines: Vec<String>,
    width: usize,
    old_side: bool,
}

impl Card {
    fn height(&self) -> usize {
        self.lines.len() + 4
    }
}

/// Wraps a comment to at most 64 columns and three quarters of the text area.
fn card(comment: &str, text_width: usize, old_side: bool) -> Card {
    let max = (text_width * 3 / 4).clamp(10, 64);
    let body = max - 6;
    let mut lines = Vec::new();
    for line in comment.replace('\t', "    ").split('\n') {
        let mut part = String::new();
        for c in line.chars() {
            if part.width() + c.width().unwrap_or(0) > body && !part.is_empty() {
                match part.rfind(' ') {
                    Some(i) if i > 0 => {
                        let rest = part[i + 1..].to_string();
                        part.truncate(i);
                        lines.push(std::mem::replace(&mut part, rest));
                    }
                    _ => lines.push(std::mem::take(&mut part)),
                }
            }
            part.push(c);
        }
        lines.push(part);
    }
    let widest = lines.iter().map(|l| l.width()).max().unwrap_or(0);
    Card { width: max.min((widest + 6).max(28)), lines, old_side }
}

/// Visual rows for one width: wraps, note cards, and the scroll position.
#[derive(Default)]
struct View {
    key: Option<(u64, u16, u16, bool, u64)>,
    notes_key: u64,
    /// The file's notes from the last draw: source locations and comment.
    notes: Vec<(Vec<RowLines>, String)>,
    annotated: Vec<bool>,
    cards: HashMap<usize, Vec<Card>>,
    wraps: Vec<Wrapped>,
    lines: WrapCache,
    scroll: usize,
    /// The cursor's row inside the viewport, kept across rebuilds.
    offset: usize,
}

impl View {
    fn cards_height(&self, unit: usize) -> usize {
        self.cards.get(&unit).map_or(0, |cards| cards.iter().map(Card::height).sum())
    }
}

/// Diff columns: line number, sign, and text.
#[derive(Clone, Copy)]
struct Geom {
    width: u16,
    num_w: u16,
    gutter_w: u16,
    text_off: u16,
    text_w: u16,
}

impl Geom {
    fn new(max_lineno: u32, width: u16) -> Self {
        let num_w = max_lineno.max(1).to_string().len().max(2) as u16;
        let gutter_w = num_w + 2;
        let text_off = gutter_w + 2;
        // One free column on the right, so text doesn't touch the edge.
        let text_w = width.saturating_sub(text_off + 1).max(1);
        Self { width, num_w, gutter_w, text_off, text_w }
    }
}

/// `@@ -1,3 +1,4 @@ fn foo` → (`@@ -1,3 +1,4 @@`, ` fn foo`).
fn split_hunk(h: &str) -> (&str, &str) {
    match h.get(2..).and_then(|r| r.find("@@")) {
        Some(i) => h.split_at(i + 4),
        None => (h, ""),
    }
}

struct Paint<'a> {
    diff: &'a FileDiff,
    view: &'a View,
    p: &'a Palette,
    g: Geom,
    focused: bool,
    cursor: Position,
}

#[derive(Default)]
pub struct Reader {
    generation: Option<u64>,
    path: String,
    max_lineno: u32,
    doc: Doc,
    cursor: Position,
    /// The focused side in split layout.
    old_side: bool,
    selection: Option<Selection>,
    press: Option<(Position, bool)>,
    split: bool,
    area: Rect,
    hscroll: usize,
    search: Option<Search>,
    hits: Vec<Hit>,
    highlight: bool,
    view: RefCell<View>,
}

impl Reader {
    pub fn loaded(&self, generation: u64) -> bool {
        self.generation == Some(generation)
    }

    /// Whether the split layout is showing (it stacks in tiny panes).
    pub fn split(&self) -> bool {
        self.split_active()
    }

    #[cfg(test)]
    pub fn cursor(&self) -> Position {
        self.clamped()
    }

    /// The side whose line numbers source jumps use.
    pub fn old_side(&self) -> bool {
        if self.split_active() { self.old_side } else { self.kind(self.cursor.row) == Some(RowKind::Del) }
    }

    pub fn resize(&mut self, area: Rect) {
        if area != self.area {
            self.area = area;
            self.normalize_side();
        }
    }

    /// Shows `diff`. A new file starts at its first hunk; a reload of the same
    /// file keeps the cursor on its source row. `seek` moves the cursor.
    pub fn load(&mut self, diff: &FileDiff, generation: u64, seek: Option<Position>) {
        if self.generation != Some(generation) {
            let same_file = self.generation.is_some() && self.path == diff.path;
            self.generation = Some(generation);
            let texts: Vec<String> = diff.rows.iter().map(annotations::text).collect();
            let keys: Vec<RowKey> = diff.rows.iter().map(|r| (r.kind, r.old, r.new)).collect();
            if !same_file || texts != self.doc.texts || keys != self.doc.keys {
                let previous = if same_file { self.doc.keys.get(self.cursor.row).copied() } else { None };
                let whole = diff.context >= crate::app::FULL_CONTEXT;
                self.doc = Doc::new(texts, keys, whole, self.doc.revision + 1);
                self.path = diff.path.clone();
                self.max_lineno = diff.max_lineno;
                self.selection = None;
                self.press = None;
                self.hits = self.search.as_ref().map_or_else(Vec::new, |s| {
                    navigation::matches(&self.doc.texts, self.doc.stacked_rows.iter().copied(), &s.regex)
                });
                match previous {
                    Some(key) => {
                        self.cursor.row = self.relocate(key);
                        self.normalize_side();
                    }
                    None => {
                        self.hscroll = 0;
                        if seek.is_none() {
                            self.first_hunk(diff);
                        }
                    }
                }
            }
        }
        if let Some(pos) = seek {
            self.seek(diff, pos);
        }
    }

    /// The row with the same line numbers, or the nearest source line.
    fn relocate(&self, (_, old, new): RowKey) -> usize {
        let keys = &self.doc.keys;
        if let Some(r) = keys.iter().position(|k| is_source(k.0) && k.1 == old && k.2 == new) {
            return r;
        }
        let Some(line) = new.or(old) else { return self.doc.stacked_rows.first().copied().unwrap_or(0) };
        keys.iter()
            .enumerate()
            .filter(|(_, k)| is_source(k.0))
            .filter_map(|(r, k)| k.2.or(k.1).map(|n| (r, n.abs_diff(line))))
            .min_by_key(|(_, distance)| *distance)
            .map_or(0, |(r, _)| r)
    }

    fn kind(&self, row: usize) -> Option<RowKind> {
        self.doc.keys.get(row).map(|k| k.0)
    }

    fn split_active(&self) -> bool {
        self.split && self.area.width > 2 * (Geom::new(self.max_lineno, 0).text_off + MIN_SPLIT_TEXT + 1)
    }

    fn on_side(&self, row: usize, old: bool) -> bool {
        match self.kind(row) {
            Some(RowKind::Ctx) => true,
            Some(RowKind::Del) => old,
            Some(RowKind::Add) => !old,
            _ => false,
        }
    }

    fn side_rows(&self) -> &[usize] {
        match (self.split_active(), self.old_side) {
            (false, _) => &self.doc.stacked_rows,
            (true, true) => &self.doc.old_rows,
            (true, false) => &self.doc.new_rows,
        }
    }

    fn unit_of(&self, row: usize) -> usize {
        if self.split_active() { self.doc.split_of.get(row).copied().unwrap_or(0) } else { row }
    }

    fn len(&self, row: usize) -> usize {
        self.doc.texts.get(row).map_or(0, |t| t.chars().count())
    }

    fn clamped(&self) -> Position {
        Position { row: self.cursor.row, col: self.cursor.col.min(self.len(self.cursor.row).saturating_sub(1)) }
    }

    /// Split layout follows the cursor's row: deleted lines are old, added new.
    fn normalize_side(&mut self) {
        if self.split_active() {
            match self.kind(self.cursor.row) {
                Some(RowKind::Del) => self.old_side = true,
                Some(RowKind::Add) => self.old_side = false,
                _ => {}
            }
        }
    }

    /// In split layout, moves the cursor to the nearest row on the focused side.
    fn snap_to_side(&mut self) {
        if !self.split_active() || self.on_side(self.cursor.row, self.old_side) || self.doc.split.is_empty() {
            return;
        }
        let old = self.old_side;
        let unit = self.unit_of(self.cursor.row);
        let side_row = |u: usize| {
            let s = self.doc.split.get(u)?;
            let r = if old { s.old } else { s.new }?;
            self.on_side(r, old).then_some(r)
        };
        let found = (0..self.doc.split.len()).find_map(|d| side_row(unit + d).or_else(|| side_row(unit.checked_sub(d)?)));
        if let Some(r) = found {
            self.cursor.row = r;
        }
    }

    fn body(&self) -> Rect {
        let a = self.area;
        if self.split_active() { Rect::new(a.x, a.y + 1, a.width, a.height.saturating_sub(1)) } else { a }
    }

    fn geom(&self) -> Geom {
        let width = if self.split_active() { self.area.width.saturating_sub(1) / 2 } else { self.area.width };
        Geom::new(self.max_lineno, width)
    }

    fn height(&self) -> usize {
        self.body().height.max(1) as usize
    }

    /// The visual rows for the current width, rebuilt when the document,
    /// size, layout, or (when given) the file's notes change.
    fn view(&self, diff: &FileDiff, notes: Option<&[Note]>) -> RefMut<'_, View> {
        let mut v = self.view.borrow_mut();
        if let Some(notes) = notes {
            let mine = || notes.iter().filter(|n| n.path == diff.path && n.revision == diff.revision && !n.locations.is_empty());
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            for note in mine() {
                (&note.locations, &note.comment).hash(&mut hash);
            }
            let key = hash.finish();
            if key != v.notes_key {
                v.notes_key = key;
                v.notes = mine().map(|n| (n.locations.clone(), n.comment.clone())).collect();
            }
        }
        let key = (self.doc.revision, self.area.width, self.area.height, self.split_active(), v.notes_key);
        let previous = v.key;
        if previous != Some(key) {
            v.key = Some(key);
            // Resizes and note changes keep the top row; new content keeps the cursor's place.
            let top = previous.filter(|k| k.0 == key.0 && k.3 == key.3).map(|_| v.lines.locate(v.scroll).0);
            self.rebuild(&mut v, diff, top);
        }
        v
    }

    fn rebuild(&self, v: &mut View, diff: &FileDiff, top: Option<usize>) {
        let split = self.split_active();
        let g = self.geom();
        let View { notes, annotated, cards, .. } = v;
        *annotated = vec![false; self.doc.keys.len()];
        cards.clear();
        for (locations, comment) in notes.iter() {
            let rows: Vec<usize> = (0..self.doc.keys.len())
                .filter(|&r| {
                    let (_, old, new) = self.doc.keys[r];
                    (old.is_some() || new.is_some()) && locations.contains(&(old, new))
                })
                .collect();
            let Some(&first) = rows.first() else { continue };
            for r in rows {
                annotated[r] = true;
            }
            let old_side = split && !self.on_side(first, false);
            cards.entry(self.unit_of(first)).or_default().push(card(comment, g.text_w as usize, old_side));
        }
        v.wraps = if split { Vec::new() } else { diff.rows.iter().map(|r| wrap_row(r, g.text_w as usize)).collect() };
        let units = if split { self.doc.split.len() } else { self.doc.keys.len() };
        let lines: Vec<usize> =
            (0..units).map(|u| v.cards_height(u) + if split { 1 } else { v.wraps[u].starts.len() }).collect();
        v.lines.rebuild(lines.into_iter());
        let h = self.height();
        let cursor = self.cursor_line(v);
        v.scroll = match top {
            Some(unit) => v.lines.first.get(unit).copied().unwrap_or(0),
            None => cursor.saturating_sub(v.offset.min(h - 1)),
        };
        self.settle(v, cursor);
    }

    /// Clamps the scroll position and records the cursor's place in the viewport.
    fn settle(&self, v: &mut View, cursor: usize) {
        let h = self.height();
        v.scroll = v.scroll.min(v.lines.total().saturating_sub(h));
        if cursor < v.scroll {
            v.scroll = cursor;
        } else if cursor >= v.scroll + h {
            v.scroll = cursor + 1 - h;
        }
        v.offset = cursor - v.scroll;
    }

    fn cursor_line(&self, v: &View) -> usize {
        if self.doc.keys.is_empty() {
            return 0;
        }
        let unit = self.unit_of(self.cursor.row);
        let sub = match v.wraps.get(unit) {
            Some(w) if !self.split_active() => w.starts.partition_point(|&s| s <= self.clamped().col).saturating_sub(1),
            _ => 0,
        };
        v.lines.first.get(unit).copied().unwrap_or(0) + v.cards_height(unit) + sub
    }

    fn ensure_visible(&mut self, diff: &FileDiff) {
        if self.doc.keys.is_empty() {
            return;
        }
        {
            let mut v = self.view(diff, None);
            let cursor = self.cursor_line(&v);
            let top = v.lines.first.get(self.unit_of(self.cursor.row)).copied().unwrap_or(cursor);
            // Moving up onto an annotated row shows its card too.
            if cursor < v.scroll && cursor - top < self.height() {
                v.scroll = top;
            }
            self.settle(&mut v, cursor);
        }
        if self.split_active() {
            let text = &self.doc.texts[self.cursor.row];
            let x: usize = text.chars().take(self.clamped().col).map(|c| c.width().unwrap_or(0)).sum();
            let w = self.geom().text_w as usize;
            if x < self.hscroll {
                self.hscroll = x;
            } else if x >= self.hscroll + w {
                self.hscroll = x + 1 - w;
            }
        }
    }

    fn scroll_lines(&mut self, diff: &FileDiff, delta: isize) {
        let (top, bottom) = {
            let mut v = self.view(diff, None);
            let h = self.height();
            let max = v.lines.total().saturating_sub(h);
            v.scroll = v.scroll.saturating_add_signed(delta).min(max);
            (v.scroll, v.scroll + h - 1)
        };
        // Keep the cursor on screen, like Vim's Ctrl+e / Ctrl+y.
        let line_of = |r: &Reader, row: usize| {
            let v = r.view.borrow();
            v.lines.first.get(r.unit_of(row)).copied().unwrap_or(0) + v.cards_height(r.unit_of(row))
        };
        let current = line_of(self, self.cursor.row);
        let rows = self.side_rows().to_vec();
        if current < top {
            if let Some(&r) = rows.iter().find(|&&r| line_of(self, r) >= top) {
                self.cursor.row = r;
            }
        } else if current > bottom
            && let Some(&r) = rows.iter().rev().find(|&&r| line_of(self, r) <= bottom)
        {
            self.cursor.row = r;
        }
        let mut v = self.view(diff, None);
        let cursor = self.cursor_line(&v);
        v.offset = cursor.saturating_sub(v.scroll).min(self.height() - 1);
    }

    fn step(&mut self, delta: isize) {
        let rows = self.side_rows();
        if rows.is_empty() {
            return;
        }
        let k = rows.binary_search(&self.cursor.row).unwrap_or_else(|k| k.min(rows.len() - 1));
        self.cursor.row = rows[k.saturating_add_signed(delta).min(rows.len() - 1)];
    }

    pub fn motion(&mut self, diff: &FileDiff, motion: Motion, count: usize) {
        if self.doc.keys.is_empty() {
            return;
        }
        let n = count.max(1);
        let h = self.height();
        let at = self.clamped();
        match motion {
            Motion::Left => self.cursor.col = at.col.saturating_sub(n),
            Motion::Right => self.cursor.col = (at.col + n).min(self.len(at.row).saturating_sub(1)),
            Motion::Up => self.step(-(n as isize)),
            Motion::Down => self.step(n as isize),
            Motion::WordNext | Motion::WordBack | Motion::WordEnd => {
                let c = match motion {
                    Motion::WordBack => 'b',
                    Motion::WordEnd => 'e',
                    _ => 'w',
                };
                self.cursor = navigation::word(&self.doc.texts, self.side_rows(), at, c, n);
            }
            Motion::LineStart => self.cursor.col = 0,
            Motion::FirstNonBlank => {
                self.cursor.col = self.doc.texts[at.row].chars().take_while(|c| c.is_whitespace()).count();
            }
            Motion::LineEnd => self.cursor.col = self.len(at.row).saturating_sub(1),
            Motion::Top | Motion::Bottom => {
                let rows = self.side_rows();
                let row = if motion == Motion::Top { rows.first() } else { rows.last() };
                if let Some(&row) = row {
                    self.cursor = Position { row, col: 0 };
                }
            }
            Motion::HalfDown | Motion::HalfUp | Motion::PageDown | Motion::PageUp => {
                let page = if matches!(motion, Motion::HalfDown | Motion::HalfUp) { h / 2 } else { h.saturating_sub(1) };
                let delta = (page.max(1) * n) as isize;
                let delta = if matches!(motion, Motion::HalfUp | Motion::PageUp) { -delta } else { delta };
                {
                    let mut v = self.view(diff, None);
                    v.scroll = v.scroll.saturating_add_signed(delta).min(v.lines.total().saturating_sub(h));
                }
                self.step(delta);
            }
            Motion::ScrollDown => return self.scroll_lines(diff, n as isize),
            Motion::ScrollUp => return self.scroll_lines(diff, -(n as isize)),
            Motion::PlaceTop | Motion::PlaceCenter | Motion::PlaceBottom => {
                let mut v = self.view(diff, None);
                let cursor = self.cursor_line(&v);
                v.scroll = match motion {
                    Motion::PlaceTop => cursor,
                    Motion::PlaceCenter => cursor.saturating_sub(h / 2),
                    _ => cursor.saturating_sub(h - 1),
                }
                .min(v.lines.total().saturating_sub(1));
                v.offset = cursor - v.scroll;
                return;
            }
            Motion::ScrollLeft => return self.hscroll = self.hscroll.saturating_sub(HSCROLL_STEP * n),
            Motion::ScrollRight => return self.hscroll += HSCROLL_STEP * n,
        }
        self.ensure_visible(diff);
    }

    /// Moves to the first hunk, or the first source row when nothing changed.
    pub fn first_hunk(&mut self, diff: &FileDiff) {
        match self.doc.targets.first().copied() {
            Some(target) => self.go_target(diff, target),
            None => {
                self.cursor = Position { row: self.doc.stacked_rows.first().copied().unwrap_or(0), col: 0 };
                self.normalize_side();
                let mut v = self.view(diff, None);
                v.scroll = 0;
                let cursor = self.cursor_line(&v);
                self.settle(&mut v, cursor);
            }
        }
    }

    /// `[` / `]`: previous or next hunk, `count` times, clamped at the ends.
    /// Returns a hint when it stopped at a boundary.
    pub fn hunk(&mut self, diff: &FileDiff, forward: bool, count: usize) -> Option<&'static str> {
        let targets = &self.doc.targets;
        if targets.is_empty() {
            return Some("No changed hunks in this file");
        }
        let unit = self.unit_of(self.cursor.row);
        let n = count.max(1);
        let last = targets.len() - 1;
        let (index, hint) = if forward {
            match targets.iter().position(|t| self.unit_of(t.row) > unit) {
                Some(i) if i + n - 1 <= last => (i + n - 1, None),
                Some(_) => (last, Some("Last hunk")),
                None => return Some("Last hunk"),
            }
        } else {
            match targets.iter().rposition(|t| self.unit_of(t.row) < unit) {
                Some(i) if i >= n - 1 => (i + 1 - n, None),
                Some(_) => (0, Some("First hunk")),
                None => return Some("First hunk"),
            }
        };
        let target = targets[index];
        self.go_target(diff, target);
        hint
    }

    fn go_target(&mut self, diff: &FileDiff, target: Target) {
        self.cursor = Position { row: target.row, col: 0 };
        self.old_side = target.old_side;
        self.snap_to_side();
        let h = self.height();
        let mut v = self.view(diff, None);
        let cursor = self.cursor_line(&v);
        let anchor = v.lines.first.get(self.unit_of(target.anchor)).copied().unwrap_or(cursor);
        // Show the hunk header above the change when both fit.
        v.scroll = if anchor <= cursor && cursor < anchor + h { anchor } else { cursor };
        self.settle(&mut v, cursor);
    }

    /// Moves to `pos` (the nearest source row), centered.
    fn seek(&mut self, diff: &FileDiff, pos: Position) {
        if self.doc.keys.is_empty() {
            return;
        }
        let row = pos.row.min(self.doc.keys.len() - 1);
        let rows = &self.doc.stacked_rows;
        let row = rows.get(rows.partition_point(|&r| r < row)).or(rows.last()).copied().unwrap_or(row);
        self.cursor = Position { row, col: pos.col };
        self.normalize_side();
        let h = self.height();
        let mut v = self.view(diff, None);
        let cursor = self.cursor_line(&v);
        v.scroll = cursor.saturating_sub(h / 2);
        self.settle(&mut v, cursor);
    }

    /// The row showing source line `line` on the given side. In stacked
    /// layout a new-side miss falls back to deleted lines.
    pub fn source_row(&self, line: u32, old_side: bool) -> Option<usize> {
        let keys = &self.doc.keys;
        let side = |r: &RowKey| is_source(r.0) && if old_side { r.1 == Some(line) } else { r.2 == Some(line) };
        keys.iter().position(side).or_else(|| {
            (!self.split_active()).then(|| keys.iter().position(|k| is_source(k.0) && k.1 == Some(line))).flatten()
        })
    }

    /// `<leader>h` / `<leader>l`: focus the old or new side of a split diff.
    pub fn set_side(&mut self, diff: &FileDiff, old: bool) {
        self.old_side = old;
        if !self.split_active() {
            return;
        }
        if self.selection.is_some_and(|s| s.old_side != Some(old)) {
            self.selection = None;
        }
        self.snap_to_side();
        self.ensure_visible(diff);
    }

    /// `S`: the next draw rebuilds the layout around the cursor.
    pub fn toggle_split(&mut self) {
        self.split = !self.split;
        self.normalize_side();
        self.snap_to_side();
    }

    /// `v` / `V`: start, switch, or end a character or line selection.
    pub fn visual(&mut self, lines: bool) {
        match self.selection {
            Some(s) if s.lines == lines => self.selection = None,
            Some(s) => self.selection = Some(Selection { lines, ..s }),
            None if self.kind(self.cursor.row).is_some_and(is_source) => {
                let old_side = self.split_active().then_some(self.old_side);
                self.selection = Some(Selection { anchor: self.clamped(), lines, old_side });
            }
            None => {}
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Escape: clears the selection, then search highlights. `false` when
    /// there was nothing to clear.
    pub fn escape(&mut self) -> bool {
        if self.selection.take().is_some() {
            return true;
        }
        std::mem::take(&mut self.highlight)
    }

    fn in_selection(&self, s: &Selection, row: usize) -> bool {
        s.old_side.map_or_else(|| self.kind(row).is_some_and(is_source), |old| self.on_side(row, old))
    }

    /// The selected text (or the cursor's line) with its rows, and whether
    /// it was taken from the old side. `None` on headers and notices.
    pub fn capture(&self) -> Option<(Vec<(usize, String)>, bool)> {
        let cursor = self.clamped();
        let Some(s) = self.selection else {
            let source = self.kind(cursor.row).is_some_and(is_source);
            let old_side = self.split_active() && self.old_side;
            return source.then(|| (vec![(cursor.row, self.doc.texts[cursor.row].clone())], old_side));
        };
        let (lo, hi) = (s.anchor.min(cursor), s.anchor.max(cursor));
        let rows = (lo.row..=hi.row)
            .filter(|&r| self.in_selection(&s, r))
            .map(|r| {
                let text = &self.doc.texts[r];
                let text = if s.lines {
                    text.clone()
                } else {
                    text.chars().enumerate().filter(|&(col, _)| s.contains(cursor, Position { row: r, col })).map(|(_, c)| c).collect()
                };
                (r, text)
            })
            .collect();
        Some((rows, s.old_side.unwrap_or(false)))
    }

    /// `/`: compiles and runs a new search; empty input repeats the last one.
    /// An invalid pattern keeps the previous search and cursor.
    pub fn search(&mut self, diff: &FileDiff, pattern: &str) -> Result<(), String> {
        if !pattern.is_empty() {
            let search = Search::new(pattern)?;
            self.hits = navigation::matches(&self.doc.texts, self.doc.stacked_rows.iter().copied(), &search.regex);
            self.search = Some(search);
        }
        self.search_next(diff, false, 1)
    }

    /// `n` / `N`: the next or previous match on the focused side, wrapping.
    pub fn search_next(&mut self, diff: &FileDiff, backward: bool, count: usize) -> Result<(), String> {
        let Some(search) = &self.search else { return Err("No previous search".into()) };
        self.highlight = true;
        let eligible = |h: &&Hit| !self.split_active() || self.on_side(h.row, self.old_side);
        let mut at = (self.cursor.row, self.cursor.col.min(self.len(self.cursor.row)));
        for _ in 0..count.max(1) {
            let next = if backward {
                let mut hits = self.hits.iter().rev().filter(eligible);
                hits.clone().find(|h| (h.row, h.start) < at).or_else(|| hits.next())
            } else {
                let mut hits = self.hits.iter().filter(eligible);
                hits.clone().find(|h| (h.row, h.start) > at).or_else(|| hits.next())
            };
            let Some(hit) = next else { return Err(format!("Pattern not found: {}", search.pattern)) };
            at = (hit.row, hit.start);
        }
        self.cursor = Position { row: at.0, col: at.1 };
        self.ensure_visible(diff);
        Ok(())
    }

    /// The source cell under a mouse position, and its side.
    fn hit_test(&self, diff: &FileDiff, x: u16, y: u16) -> Option<(Position, bool)> {
        let body = self.body();
        if x < body.x || x >= body.right() || y < body.y || y >= body.bottom() || self.doc.keys.is_empty() {
            return None;
        }
        let v = self.view(diff, None);
        let line = v.scroll + (y - body.y) as usize;
        if line >= v.lines.total() {
            return None;
        }
        let (unit, sub) = v.lines.locate(line);
        let sub = sub.checked_sub(v.cards_height(unit))?;
        let g = self.geom();
        let (row, old, pane) = if self.split_active() {
            let s = self.doc.split[unit];
            if x < body.x + g.width {
                (s.old?, true, body.x)
            } else if x > body.x + g.width {
                (s.new?, false, body.x + g.width + 1)
            } else {
                return None;
            }
        } else {
            (unit, self.kind(unit) == Some(RowKind::Del), body.x)
        };
        if !self.kind(row).is_some_and(is_source) {
            return None;
        }
        let mut cell = x.saturating_sub(pane + g.text_off) as usize;
        let mut col = 0;
        if let Some(w) = v.wraps.get(unit).filter(|_| !self.split_active()) {
            col = w.starts[sub];
            if sub > 0 {
                cell = cell.saturating_sub(w.indent);
            }
        } else {
            cell += self.hscroll;
        }
        for c in self.doc.texts[row].chars().skip(col) {
            let w = c.width().unwrap_or(0);
            if cell < w {
                break;
            }
            cell -= w;
            col += 1;
        }
        Some((Position { row, col: col.min(self.len(row).saturating_sub(1)) }, old))
    }

    /// A click: moves the cursor there. `false` outside source text.
    pub fn click(&mut self, diff: &FileDiff, x: u16, y: u16) -> bool {
        let Some((pos, old)) = self.hit_test(diff, x, y) else { return false };
        self.cursor = pos;
        if self.split_active() {
            self.old_side = old;
        }
        self.selection = None;
        self.press = Some((pos, old));
        self.ensure_visible(diff);
        true
    }

    /// A drag after a click selects characters on the same side.
    pub fn drag(&mut self, diff: &FileDiff, x: u16, y: u16) {
        let (Some((anchor, side)), Some((pos, old))) = (self.press, self.hit_test(diff, x, y)) else { return };
        let split = self.split_active();
        if split && old != side {
            return;
        }
        if self.selection.is_none() {
            self.selection = Some(Selection { anchor, lines: false, old_side: split.then_some(side) });
        }
        self.cursor = pos;
        self.ensure_visible(diff);
    }

    pub fn release(&mut self) {
        self.press = None;
    }

    pub fn draw(&self, area: Rect, diff: &FileDiff, notes: &[Note], p: &Palette, focused: bool, buf: &mut Buffer) {
        if area.is_empty() || self.doc.keys.is_empty() {
            return;
        }
        let v = self.view(diff, Some(notes));
        let split = self.split_active();
        let body = self.body();
        let g = self.geom();
        let paint = Paint { diff, view: &v, p, g, focused, cursor: self.clamped() };
        if split {
            for (x, old) in [(area.x, true), (area.x + g.width + 1, false)] {
                let style = if self.old_side == old {
                    Style::new().fg(p.blue).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(p.faint)
                };
                buf.set_stringn(x, area.y, if old { " Old" } else { " New" }, g.width as usize, style);
            }
        }
        for line in 0..body.height {
            let visual = v.scroll + line as usize;
            if visual >= v.lines.total() {
                break;
            }
            let (unit, sub) = v.lines.locate(visual);
            let y = body.y + line;
            if let Some(cards) = v.cards.get(&unit)
                && sub < v.cards_height(unit)
            {
                let mut k = sub;
                for card in cards {
                    if k < card.height() {
                        let pane = if card.old_side || !split { area.x } else { area.x + g.width + 1 };
                        draw_card(buf, pane, y, g, card, k, p);
                        break;
                    }
                    k -= card.height();
                }
                continue;
            }
            let sub = sub - v.cards_height(unit);
            if !split {
                self.draw_row(buf, &paint, area.x, area.width, y, unit, sub, None, v.wraps.get(unit));
                continue;
            }
            let SplitRow { old, new } = self.doc.split[unit];
            if let Some(r) = old.filter(|&r| old == new && !self.kind(r).is_some_and(is_source)) {
                self.draw_row(buf, &paint, area.x, area.width, y, r, 0, None, None);
                continue;
            }
            buf.set_string(area.x + g.width, y, "│", Style::new().fg(p.rule));
            for (x, row, old_side) in [(area.x, old, true), (area.x + g.width + 1, new, false)] {
                match row {
                    Some(r) => self.draw_row(buf, &paint, x, g.width, y, r, 0, Some(old_side), None),
                    None => buf.set_style(Rect::new(x, y, g.width, 1), Style::new().bg(p.sel_bg)),
                }
            }
        }
    }

    /// One visual line of diff row `r`: gutter, sign, text (line `sub` of
    /// its wrap), then cursor, search, and selection on top. `side` is the
    /// split pane, `None` when stacked or spanning both panes.
    #[allow(clippy::too_many_arguments)]
    fn draw_row(
        &self,
        buf: &mut Buffer,
        c: &Paint,
        x: u16,
        width: u16,
        y: u16,
        r: usize,
        sub: usize,
        side: Option<bool>,
        wrap: Option<&Wrapped>,
    ) {
        let (p, g) = (c.p, c.g);
        let row = &c.diff.rows[r];
        let (row_bg, gutter_bg, emph_bg, sign, sign_fg) = match row.kind {
            RowKind::Add => (Some(p.add_bg), Some(p.add_gutter), Some(p.add_emph), "+", p.green),
            RowKind::Del => (Some(p.del_bg), Some(p.del_gutter), Some(p.del_emph), "−", p.red),
            RowKind::Hunk => (Some(p.hunk_bg), Some(p.hunk_bg), None, " ", p.faint),
            _ => (None, None, None, " ", p.faint),
        };
        let base = row_bg.map_or(Style::new(), |bg| Style::new().bg(bg));
        buf.set_style(Rect::new(x, y, width, 1), base);
        let annotated = c.view.annotated.get(r).copied().unwrap_or(false);
        let gutter = if annotated {
            Style::new().bg(p.annotation).fg(p.annotation_fg)
        } else {
            gutter_bg.map_or(Style::new(), |bg| Style::new().bg(bg))
        };
        buf.set_style(Rect::new(x, y, g.gutter_w.min(width), 1), gutter);
        let right = (x + width).saturating_sub(1);
        let gw = g.num_w as usize;
        if sub == 0 {
            let number = match side {
                Some(true) => row.old,
                Some(false) => row.new,
                None if row.kind == RowKind::Del => row.old,
                None => row.new,
            };
            let text = match (row.kind, number) {
                (RowKind::Hunk, _) => format!("{:>gw$}", "⋯"),
                (_, Some(n)) => format!("{n:>gw$}"),
                _ => String::new(),
            };
            let fg = if annotated {
                p.annotation_fg
            } else if changed(row.kind) {
                p.muted
            } else {
                p.faint
            };
            buf.set_stringn(x + 1, y, &text, gw.min(width.saturating_sub(1) as usize), gutter.fg(fg));
            if x + g.gutter_w < right {
                buf.set_string(x + g.gutter_w, y, sign, base.fg(sign_fg).add_modifier(Modifier::BOLD));
            }
        }
        let text_x = x + g.text_off;
        if text_x >= right {
            return;
        }
        match row.kind {
            RowKind::Hunk => {
                let (header, func) = split_hunk(&row.segs[0].text);
                let (nx, _) = buf.set_stringn(text_x, y, header, (right - text_x) as usize, base.fg(p.faint));
                if nx < right {
                    buf.set_stringn(nx, y, func, (right - nx) as usize, base.fg(p.muted));
                }
                return;
            }
            RowKind::Meta => {
                let style = base.fg(p.faint).add_modifier(Modifier::ITALIC);
                buf.set_stringn(text_x, y, &row.segs[0].text, (right - text_x) as usize, style);
                return;
            }
            _ => {}
        }
        let mine = side.is_none_or(|s| s == self.old_side);
        let cursor_here = c.focused && mine && c.cursor.row == r;
        if cursor_here {
            buf.set_string(x, y, "▸", gutter.fg(p.blue));
        }
        let hits: &[Hit] = if self.highlight && mine {
            let start = self.hits.partition_point(|h| h.row < r);
            let end = start + self.hits[start..].partition_point(|h| h.row == r);
            &self.hits[start..end]
        } else {
            &[]
        };
        let selection = self.selection.filter(|s| {
            self.in_selection(s, r) && (side.is_none() || s.old_side.is_none() || s.old_side == side)
        });
        let (from, to, indent) = match wrap {
            Some(w) => (w.starts[sub], w.starts.get(sub + 1).copied().unwrap_or(usize::MAX), if sub > 0 { w.indent } else { 0 }),
            None => (0, usize::MAX, 0),
        };
        let mut skip = if side.is_some() { self.hscroll } else { 0 };
        let mut cx = text_x + indent as u16;
        let mut col = 0;
        'segments: for seg in &row.segs {
            let mut style = base.fg(seg.fg.map_or(p.fg, |i| {
                let (r, g, b) = p.ansi[usize::from(i).min(15)];
                Color::Rgb(r, g, b)
            }));
            if seg.bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            if seg.emph && let Some(bg) = emph_bg {
                style = style.bg(bg);
            }
            for ch in seg.text.chars() {
                if col >= to {
                    break 'segments;
                }
                if col >= from {
                    let w = ch.width().unwrap_or(0);
                    if skip > 0 {
                        skip = skip.saturating_sub(w);
                        col += 1;
                        continue;
                    }
                    if cx + w as u16 > right {
                        break 'segments;
                    }
                    let pos = Position { row: r, col };
                    let cell = if selection.is_some_and(|s| s.contains(c.cursor, pos)) {
                        Style::new().fg(p.selection_fg).bg(p.blue).add_modifier(Modifier::BOLD)
                    } else if hits.iter().any(|h| h.start <= col && col < h.end) {
                        Style::new().fg(p.search_fg).bg(p.yellow).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                    } else if cursor_here && col == c.cursor.col {
                        style.add_modifier(Modifier::REVERSED)
                    } else {
                        style
                    };
                    let mut bytes = [0; 4];
                    buf.set_stringn(cx, y, ch.encode_utf8(&mut bytes), w, cell);
                    cx += w as u16;
                }
                col += 1;
            }
        }
        if cursor_here && sub == 0 && self.len(r) == 0 && side.is_none_or(|_| self.hscroll == 0) {
            buf.set_style(Rect::new(text_x, y, 1, 1), base.add_modifier(Modifier::REVERSED));
        }
    }
}

/// Line `k` of a note card: a labeled top border, padded body lines, and a
/// violet bar that joins the card to the annotated gutters below.
fn draw_card(buf: &mut Buffer, pane: u16, y: u16, g: Geom, card: &Card, k: usize, p: &Palette) {
    let x = pane + g.gutter_w;
    let right = pane + g.width;
    if x >= right {
        return;
    }
    let max = (right - x) as usize;
    let w = card.width;
    let label = Style::new().fg(p.annotation).bg(p.annotation_bg).add_modifier(Modifier::BOLD);
    let text = Style::new().fg(p.fg).bg(p.annotation_bg);
    let n = card.lines.len();
    if k == 0 {
        buf.set_stringn(x, y, format!("╭─ NOTE {}╮", "─".repeat(w.saturating_sub(9))), max, label);
    } else if k == n + 3 {
        let join = (x - pane) + w as u16;
        buf.set_style(Rect::new(pane, y, join.min(g.width), 1), Style::new().bg(p.annotation));
    } else {
        let body = if k == 1 || k == n + 2 { "" } else { &card.lines[k - 2] };
        let interior = format!("  {body}{}", " ".repeat(w.saturating_sub(4 + body.width())));
        let (nx, _) = buf.set_stringn(x, y, "│", max, label);
        let (nx, _) = buf.set_stringn(nx, y, &interior, (right.saturating_sub(nx)) as usize, text);
        if nx < right {
            buf.set_stringn(nx, y, "│", 1, label);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{DRow, Seg};

    fn row(kind: RowKind, old: Option<u32>, new: Option<u32>, text: &str) -> DRow {
        DRow { kind, old, new, segs: vec![Seg { text: text.into(), fg: None, bold: false, emph: false }] }
    }

    fn diff(rows: Vec<DRow>) -> FileDiff {
        let max_lineno = rows.iter().filter_map(|r| r.old.max(r.new)).max().unwrap_or(1);
        FileDiff { revision: 1, path: "x.rs".into(), context: 3, rows, max_lineno }
    }

    fn reader(d: &FileDiff, width: u16) -> Reader {
        let mut reader = Reader::default();
        reader.resize(Rect::new(0, 0, width, 12));
        reader.load(d, 1, None);
        reader
    }

    /// Two hunks, the second deleting only, separated by context.
    fn hunks() -> FileDiff {
        use RowKind::*;
        diff(vec![
            row(Hunk, None, None, "@@ -1,4 +1,4 @@ fn one"),
            row(Ctx, Some(1), Some(1), "a"),
            row(Del, Some(2), None, "b"),
            row(Add, None, Some(2), "B"),
            row(Ctx, Some(3), Some(3), "c"),
            row(Hunk, None, None, "@@ -20,3 +20,1 @@ fn two"),
            row(Ctx, Some(20), Some(20), "t"),
            row(Del, Some(21), None, "u"),
            row(Del, Some(22), None, "v"),
        ])
    }

    #[test]
    fn native_review_navigation() {
        let d = hunks();
        for width in [60, 160] {
            let mut r = reader(&d, width);
            if width == 160 {
                r.toggle_split();
                assert!(r.split());
                r.first_hunk(&d);
            }
            // A new file opens on its first change, header above it.
            let first = if r.split() { 3 } else { 2 };
            assert_eq!(r.cursor().row, first);
            assert!(!r.split() || !r.old_side(), "a hunk with additions opens on the new side");
            assert_eq!(r.view.borrow().scroll, 0);
            assert_eq!(r.hunk(&d, true, 1), None);
            assert_eq!((r.cursor().row, r.old_side()), (7, true), "deleted-only hunk lands on the old side");
            assert_eq!(r.hunk(&d, true, 1), Some("Last hunk"));
            assert_eq!(r.cursor().row, 7);
            assert_eq!(r.hunk(&d, false, 5), Some("First hunk"));
            assert_eq!(r.cursor().row, first);
            assert_eq!(r.hunk(&d, false, 1), Some("First hunk"));
            assert_eq!(r.hunk(&d, true, 2), Some("Last hunk"));
            assert_eq!(r.cursor().row, 7);
            r.motion(&d, Motion::Top, 1);
            assert_eq!(r.cursor().row, 1);
            r.motion(&d, Motion::Bottom, 1);
            assert_eq!(r.cursor().row, 8);
        }
        // Whole-file mode: runs of changes separated by context.
        use RowKind::*;
        let mut whole = diff(vec![
            row(Hunk, None, None, "@@ -1,9 +1,8 @@"),
            row(Ctx, Some(1), Some(1), "1"),
            row(Add, None, Some(2), "2"),
            row(Ctx, Some(2), Some(3), "3"),
            row(Ctx, Some(3), Some(4), "4"),
            row(Del, Some(4), None, "5"),
        ]);
        whole.context = crate::app::FULL_CONTEXT;
        let mut r = reader(&whole, 60);
        assert_eq!(r.cursor().row, 2);
        assert_eq!(r.hunk(&whole, true, 1), None);
        assert_eq!((r.cursor().row, r.old_side()), (5, true));
        // No changes: first source row; notices only: nothing to select.
        let same = diff(vec![row(Ctx, Some(1), Some(1), "x"), row(Ctx, Some(2), Some(2), "y")]);
        let mut r = reader(&same, 60);
        assert_eq!(r.cursor().row, 0);
        assert_eq!(r.hunk(&same, true, 1), Some("No changed hunks in this file"));
        let binary = diff(vec![row(Meta, None, None, "Binary file — no text diff")]);
        let r = reader(&binary, 60);
        assert!(r.capture().is_none());
    }

    #[test]
    fn native_selection_and_split_coordinates() {
        use RowKind::*;
        let d = diff(vec![
            row(Hunk, None, None, "@@ -9,3 +9,2 @@"),
            row(Del, Some(9), None, "aé中z"),
            row(Del, Some(10), None, "old two"),
            row(Del, Some(11), None, "old three"),
            row(Add, None, Some(9), "new one"),
            row(Ctx, Some(12), Some(10), "shared"),
        ]);
        let mut r = reader(&d, 160);
        r.motion(&d, Motion::Right, 2);
        r.visual(false);
        r.motion(&d, Motion::Left, 1);
        let (rows, old) = r.capture().unwrap();
        assert_eq!(rows, vec![(1, "é中".to_string())]);
        assert!(!old);
        r.clear_selection();
        // Unequal runs: three deletions pair with one addition, then filler.
        r.toggle_split();
        assert!(r.split());
        assert_eq!(r.doc.split[1], SplitRow { old: Some(1), new: Some(4) });
        assert_eq!(r.doc.split[2], SplitRow { old: Some(2), new: None });
        assert_eq!(r.doc.split[4], SplitRow { old: Some(5), new: Some(5) });
        r.set_side(&d, true);
        r.motion(&d, Motion::Top, 1);
        assert_eq!(r.cursor().row, 1);
        r.visual(true);
        r.motion(&d, Motion::Down, 4);
        assert_eq!(r.cursor().row, 5, "filler rows are not selectable");
        let (rows, old) = r.capture().unwrap();
        assert!(old);
        assert_eq!(rows.iter().map(|(row, _)| *row).collect::<Vec<_>>(), vec![1, 2, 3, 5]);
        assert_eq!(rows[3].1, "shared", "gutters, cards and filler stay out of captured text");
        let before = r.capture();
        for width in [40, 100, 160] {
            r.resize(Rect::new(0, 0, width, 12));
            assert_eq!(r.capture(), before, "resize does not mutate the capture");
        }
        r.clear_selection();
        // Switching sides keeps the shared row; old-only rows keep old numbers.
        r.set_side(&d, false);
        assert_eq!(r.cursor().row, 5);
        r.set_side(&d, true);
        r.motion(&d, Motion::Up, 2);
        assert_eq!(r.cursor().row, 2);
        r.set_side(&d, false);
        assert_eq!(r.cursor().row, 4, "nearest new-side row");
        assert_eq!(r.source_row(10, true), Some(2));
        assert_eq!(r.source_row(10, false), Some(5));
        assert_eq!(r.source_row(11, false), None, "split jumps stay on the focused side");
        r.split = false;
        assert_eq!(r.source_row(11, false), Some(3), "stacked falls back to deleted lines");
        // A note card is a visual row: it never shifts source coordinates.
        r.split = true;
        let note = Note {
            id: 1, path: "x.rs".into(), line: Some(9), old_side: true, locations: vec![(Some(9), None)],
            revision: 1, target: String::new(), preview: Vec::new(), historical: false, comment: "why?".into(),
        };
        let mut buf = Buffer::empty(Rect::new(0, 0, 160, 12));
        r.draw(buf.area, &d, std::slice::from_ref(&note), &Palette::from(None), true, &mut buf);
        let screen: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(screen.contains("NOTE") && screen.contains("why?"));
        r.set_side(&d, true);
        r.motion(&d, Motion::Top, 1);
        assert_eq!(r.cursor().row, 1);
    }

    #[test]
    fn native_regex_search() {
        use RowKind::*;
        let d = diff(vec![
            row(Hunk, None, None, "@@ -1,3 +1,3 @@ foo"),
            row(Del, Some(1), None, "let Foo = 1;"),
            row(Add, None, Some(1), "let é中foo = 2;"),
            row(Ctx, Some(2), Some(2), "bar | baz"),
        ]);
        let mut r = reader(&d, 60);
        r.search(&d, "foo").unwrap();
        assert_eq!(r.cursor(), Position { row: 1, col: 4 }, "smart case ignores case for lowercase patterns");
        r.search_next(&d, false, 1).unwrap();
        assert_eq!(r.cursor(), Position { row: 2, col: 6 }, "byte offsets map to characters");
        r.search(&d, "Foo").unwrap();
        assert_eq!(r.cursor(), Position { row: 1, col: 4 }, "uppercase makes it case-sensitive, wrapping");
        r.search(&d, "bar|baz").unwrap();
        assert_eq!(r.cursor(), Position { row: 3, col: 0 });
        r.search_next(&d, false, 1).unwrap();
        assert_eq!(r.cursor().col, 6);
        r.search_next(&d, true, 2).unwrap();
        assert_eq!(r.cursor().col, 6, "N reverses and wraps");
        r.search(&d, r"FOO\s\S").unwrap_err();
        r.search(&d, r"foo\s\S").unwrap();
        assert_eq!(r.cursor().row, 1, "escapes like \\S don't make the search case-sensitive");
        r.search(&d, "bar|baz").unwrap();
        r.search_next(&d, false, 1).unwrap();
        assert!(r.search(&d, "[").is_err());
        assert_eq!(r.cursor().col, 6, "invalid pattern keeps the cursor");
        assert_eq!(r.search.as_ref().unwrap().pattern, "bar|baz");
        r.search(&d, "").unwrap();
        assert_eq!(r.cursor().col, 0, "empty input repeats the previous search");
        r.search(&d, "^").unwrap();
        let mut rows = Vec::new();
        for _ in 0..4 {
            rows.push(r.cursor().row);
            r.search_next(&d, false, 1).unwrap();
        }
        assert_eq!(rows, vec![1, 2, 3, 1], "zero-width matches advance without looping");
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 12));
        r.draw(buf.area, &d, &[], &Palette::from(None), false, &mut buf);
        assert!(buf.content.iter().all(|c| c.bg != Palette::from(None).yellow), "zero-width hits are not highlighted");
        // Split: matches on the other side and in note cards are skipped.
        let mut r = reader(&d, 160);
        r.toggle_split();
        r.set_side(&d, true);
        r.search(&d, "foo").unwrap();
        assert_eq!(r.cursor().row, 1);
        r.search_next(&d, false, 1).unwrap();
        assert_eq!(r.cursor().row, 1, "never the opposite side");
        assert!(r.escape() && !r.highlight);
        assert!(!r.escape());
    }
}
