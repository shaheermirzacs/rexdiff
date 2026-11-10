use crate::{
    annotations::{Position, text},
    git::{FileDiff, RowKind},
};

pub fn matches(diff: &FileDiff, query: &str) -> Vec<(Position, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    let sensitive = query.chars().any(char::is_uppercase);
    let needle: Vec<char> = query.chars().collect();
    let mut found = Vec::new();
    for (row, line) in diff.rows.iter().enumerate() {
        if matches!(line.kind, RowKind::Hunk | RowKind::Meta) {
            continue;
        }
        let chars: Vec<char> = text(line).chars().collect();
        for (col, piece) in chars.windows(needle.len()).enumerate() {
            if piece
                .iter()
                .zip(&needle)
                .all(|(a, b)| if sensitive { a == b } else { a.to_lowercase().eq(b.to_lowercase()) })
            {
                found.push((Position { row, col }, needle.len()));
            }
        }
    }
    found
}

pub fn word(diff: &FileDiff, cursor: Position, motion: char, count: usize) -> Position {
    let mut chars = Vec::new();
    for (row, line) in diff.rows.iter().enumerate() {
        if matches!(line.kind, RowKind::Hunk | RowKind::Meta) {
            continue;
        }
        chars.extend(text(line).chars().enumerate().map(|(col, c)| (Position { row, col }, c)));
        chars.push((Position { row, col: text(line).chars().count() }, '\n'));
    }
    if chars.is_empty() {
        return cursor;
    }
    let class = |c: char| {
        if c.is_whitespace() {
            0
        } else if c.is_alphanumeric() || c == '_' {
            1
        } else {
            2
        }
    };
    let mut i = chars.partition_point(|(p, _)| *p < cursor).min(chars.len() - 1);
    for _ in 0..count {
        match motion {
            'b' => {
                i = i.saturating_sub(1);
                while i > 0 && class(chars[i].1) == 0 {
                    i -= 1;
                }
                while i > 0 && class(chars[i - 1].1) == class(chars[i].1) {
                    i -= 1;
                }
            }
            'e' => {
                i = (i + 1).min(chars.len() - 1);
                while i + 1 < chars.len() && class(chars[i].1) == 0 {
                    i += 1;
                }
                while i + 1 < chars.len() && class(chars[i + 1].1) == class(chars[i].1) {
                    i += 1;
                }
            }
            _ => {
                let c = class(chars[i].1);
                while i + 1 < chars.len() && class(chars[i].1) == c {
                    i += 1;
                }
                while i + 1 < chars.len() && class(chars[i].1) == 0 {
                    i += 1;
                }
            }
        }
    }
    chars[i].0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{DRow, Seg};
    #[test]
    fn unicode_search_and_word_boundaries() {
        let diff = FileDiff {
            revision: 0,
            path: "x".into(),
            context: 3,
            max_lineno: 1,
            rows: vec![DRow {
                kind: RowKind::Ctx,
                old: Some(1),
                new: Some(1),
                segs: vec![Seg { text: "éÉ hello_world!".into(), fg: None, bold: false, emph: false }],
            }],
        };
        assert_eq!(matches(&diff, "é").len(), 2);
        assert_eq!(matches(&diff, "É").len(), 1);
        assert_eq!(word(&diff, Position::default(), 'w', 1).col, 3);
        assert_eq!(word(&diff, Position { row: 0, col: 3 }, 'e', 1).col, 13);
    }
}
