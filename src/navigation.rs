//! Regex search and word motions over the reader's cached row text.

use regex::{Regex, RegexBuilder};

use crate::annotations::Position;

/// One accepted search pattern, compiled once.
pub struct Search {
    pub pattern: String,
    pub regex: Regex,
}

impl Search {
    /// Smart case: case-insensitive unless the pattern has an uppercase
    /// letter outside an escape (`\S`, `\W` don't count).
    pub fn new(pattern: &str) -> Result<Self, String> {
        let mut escaped = false;
        let upper = pattern.chars().any(|c| {
            let literal = !escaped;
            escaped = !escaped && c == '\\';
            literal && c.is_uppercase()
        });
        let regex = RegexBuilder::new(pattern)
            .case_insensitive(!upper)
            .build()
            .map_err(|e| {
                e.to_string()
                    .lines()
                    .last()
                    .unwrap_or("invalid pattern")
                    .trim()
                    .to_string()
            })?;
        Ok(Self {
            pattern: pattern.into(),
            regex,
        })
    }
}

/// A match in row `row`, from char `start` to char `end` (exclusive); empty
/// when the pattern matched zero characters.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hit {
    pub row: usize,
    pub start: usize,
    pub end: usize,
}

/// Every match in `rows` (indices into `texts`), in order. The regex reports
/// byte offsets; hits are converted to character offsets.
pub fn matches(texts: &[String], rows: impl Iterator<Item = usize>, regex: &Regex) -> Vec<Hit> {
    let mut hits = Vec::new();
    for row in rows {
        let text = &texts[row];
        let (mut byte, mut col) = (0, 0);
        let mut to_char = |b: usize| {
            col += text[byte..b].chars().count();
            byte = b;
            col
        };
        for m in regex.find_iter(text) {
            let start = to_char(m.start());
            let end = to_char(m.end());
            hits.push(Hit { row, start, end });
        }
    }
    hits
}

fn class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

/// A place in the text of `rows`, walked one character at a time; each row
/// ends with a virtual newline at its byte length.
struct Cursor<'a> {
    texts: &'a [String],
    rows: &'a [usize],
    k: usize,
    byte: usize,
}

impl Cursor<'_> {
    fn text(&self) -> &str {
        &self.texts[self.rows[self.k]]
    }

    fn char(&self) -> char {
        self.text()[self.byte..].chars().next().unwrap_or('\n')
    }

    fn next(&mut self) -> bool {
        let text = self.text();
        if let Some(c) = text[self.byte..].chars().next() {
            self.byte += c.len_utf8();
        } else if self.k + 1 < self.rows.len() {
            self.k += 1;
            self.byte = 0;
        } else {
            return false;
        }
        true
    }

    fn prev(&mut self) -> bool {
        if self.byte > 0 {
            self.byte = self.text()[..self.byte]
                .char_indices()
                .next_back()
                .map_or(0, |(i, _)| i);
        } else if self.k > 0 {
            self.k -= 1;
            self.byte = self.text().len();
        } else {
            return false;
        }
        true
    }

    fn peek(&self, forward: bool) -> Option<char> {
        let mut next = Cursor {
            texts: self.texts,
            rows: self.rows,
            k: self.k,
            byte: self.byte,
        };
        let moved = if forward { next.next() } else { next.prev() };
        moved.then(|| next.char())
    }
}

/// `w`, `b`, or `e` repeated `count` times through `rows` (display order),
/// without collecting the document's characters.
pub fn word(
    texts: &[String],
    rows: &[usize],
    cursor: Position,
    motion: char,
    count: usize,
) -> Position {
    if rows.is_empty() {
        return cursor;
    }
    let k = rows
        .partition_point(|&r| r < cursor.row)
        .min(rows.len() - 1);
    let text = &texts[rows[k]];
    let byte = if rows[k] == cursor.row {
        text.char_indices()
            .nth(cursor.col)
            .map_or(text.len(), |(i, _)| i)
    } else {
        0
    };
    let mut c = Cursor {
        texts,
        rows,
        k,
        byte,
    };
    for _ in 0..count {
        match motion {
            'b' => {
                c.prev();
                while class(c.char()) == 0 && c.prev() {}
                while c.peek(false).is_some_and(|p| class(p) == class(c.char())) {
                    c.prev();
                }
            }
            'e' => {
                c.next();
                while class(c.char()) == 0 && c.next() {}
                while c.peek(true).is_some_and(|n| class(n) == class(c.char())) {
                    c.next();
                }
            }
            _ => {
                let start = class(c.char());
                while class(c.char()) == start && c.next() {}
                while class(c.char()) == 0 && c.next() {}
            }
        }
    }
    Position {
        row: rows[c.k],
        col: c.text()[..c.byte].chars().count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_word_boundaries_cross_rows() {
        let texts = vec![
            "éÉ hello_world!".to_string(),
            "@@ hunk".to_string(),
            "  next".to_string(),
        ];
        let rows = [0, 2];
        assert_eq!(word(&texts, &rows, Position::default(), 'w', 1).col, 3);
        assert_eq!(
            word(&texts, &rows, Position { row: 0, col: 3 }, 'e', 1).col,
            13
        );
        assert_eq!(
            word(&texts, &rows, Position { row: 0, col: 14 }, 'w', 1),
            Position { row: 2, col: 2 }
        );
        assert_eq!(
            word(&texts, &rows, Position { row: 2, col: 2 }, 'b', 1),
            Position { row: 0, col: 14 }
        );
        assert_eq!(
            word(&texts, &rows, Position { row: 2, col: 2 }, 'b', 2),
            Position { row: 0, col: 3 }
        );
        assert_eq!(
            word(&texts, &rows, Position { row: 2, col: 2 }, 'b', 3),
            Position::default()
        );
    }
}
