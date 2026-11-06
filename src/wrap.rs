//! Wraps diff lines to the pane width.

use unicode_width::UnicodeWidthChar;

use crate::git::{DRow, RowKind};

/// Where each visual line of a diff row starts, as char indices, and the
/// indent of continuation lines.
pub struct Wrapped {
    pub starts: Vec<usize>,
    pub indent: usize,
}

/// Breaks after a space when it can; a word longer than the line is cut
/// where it fits. Continuations get the same indent as the code.
pub fn wrap_row(row: &DRow, width: usize) -> Wrapped {
    let mut starts = vec![0];
    // Hunk headers and notices take a single line (clipped if needed).
    if matches!(row.kind, RowKind::Hunk | RowKind::Meta) || width == 0 {
        return Wrapped { starts, indent: 0 };
    }

    let leading = row
        .segs
        .iter()
        .flat_map(|s| s.text.chars())
        .take_while(|c| *c == ' ')
        .count();
    // Too large an indent would leave little room for the text.
    let indent = if leading <= width / 2 { leading } else { 0 };

    let mut col = 0; // column in the current visual line
    let mut i = 0; // char index
    let mut brk: Option<(usize, usize)> = None; // (index, column) right after the last space
    for c in row.segs.iter().flat_map(|s| s.text.chars()) {
        let w = c.width().unwrap_or(0);
        if col + w > width && col > 0 {
            let line_start = *starts.last().expect("starts is never empty");
            let (at, at_col) = match brk {
                Some((b, bc)) if b > line_start && indent + (col - bc) + w <= width => (b, bc),
                _ => (i, col),
            };
            starts.push(at);
            col = indent + (col - at_col);
            brk = None;
        }
        col += w;
        i += 1;
        if c == ' ' && i > leading {
            brk = Some((i, col));
        }
    }
    Wrapped { starts, indent }
}

/// Visual lines of the whole diff, for scrolling and knowing what's on screen.
#[derive(Default)]
pub struct WrapCache {
    pub generation: u64,
    pub width: u16,
    /// `first[r]` is the first visual line of diff row `r`; the last element
    /// is the total.
    pub first: Vec<usize>,
}

impl WrapCache {
    pub fn total(&self) -> usize {
        self.first.last().copied().unwrap_or(0)
    }

    pub fn rebuild(&mut self, rows: &[DRow], width: u16, generation: u64) {
        if self.generation == generation && self.width == width && !self.first.is_empty() {
            return;
        }
        self.generation = generation;
        self.width = width;
        self.first.clear();
        self.first.reserve(rows.len() + 1);
        let mut acc = 0;
        for r in rows {
            self.first.push(acc);
            acc += wrap_row(r, width as usize).starts.len();
        }
        self.first.push(acc);
    }

    /// The diff row and sub-line shown at visual line `v`.
    pub fn locate(&self, v: usize) -> (usize, usize) {
        if self.first.len() < 2 {
            return (0, 0);
        }
        let rows = self.first.len() - 1;
        let r = self.first.partition_point(|&s| s <= v).saturating_sub(1).min(rows - 1);
        (r, v.saturating_sub(self.first[r]))
    }
}
