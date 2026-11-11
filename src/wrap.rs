//! Wraps diff lines to the pane width.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::git::{DRow, RowKind};

/// Where each visual line of a diff row starts, as char indices, and the
/// indent of continuation lines.
pub struct Wrapped {
    pub starts: Vec<usize>,
    pub indent: usize,
}

/// Grapheme clusters of `text`, with their length in chars and width in
/// cells. Wrapping, painting, panning, and clicks all measure with this, so
/// combining marks and joined emoji take the cells the terminal gives them.
pub fn clusters(text: &str) -> impl Iterator<Item = (&str, usize, usize)> {
    text.graphemes(true)
        .map(|g| (g, g.chars().count(), g.width()))
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
    for (g, n, w) in row.segs.iter().flat_map(|s| clusters(&s.text)) {
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
        i += n;
        if g == " " && i > leading {
            brk = Some((i, col));
        }
    }
    Wrapped { starts, indent }
}

/// Visual lines of a sequence of display units, for scrolling and knowing
/// what's on screen.
#[derive(Default)]
pub struct WrapCache {
    /// `first[u]` is the first visual line of unit `u`; the last element is
    /// the total.
    pub first: Vec<usize>,
}

impl WrapCache {
    pub fn total(&self) -> usize {
        self.first.last().copied().unwrap_or(0)
    }

    pub fn rebuild(&mut self, lines: impl Iterator<Item = usize>) {
        self.first.clear();
        let mut acc = 0;
        for n in lines {
            self.first.push(acc);
            acc += n;
        }
        self.first.push(acc);
    }

    /// The unit and the line inside it shown at visual line `v`.
    pub fn locate(&self, v: usize) -> (usize, usize) {
        if self.first.len() < 2 {
            return (0, 0);
        }
        let units = self.first.len() - 1;
        let u = self
            .first
            .partition_point(|&s| s <= v)
            .saturating_sub(1)
            .min(units - 1);
        (u, v.saturating_sub(self.first[u]))
    }
}
