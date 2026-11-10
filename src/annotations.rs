//! Review notes and delivery through the same Rex CLI used to open the panel.
use serde_json::Value;

use crate::git::{DRow, FileDiff, RowKind};

#[derive(Clone)]
pub struct Note {
    pub id: u64,
    pub path: String,
    pub line: Option<u32>,
    pub old_side: bool,
    pub locations: Vec<(Option<u32>, Option<u32>)>,
    pub revision: u64,
    pub target: String,
    pub preview: Vec<DRow>,
    pub historical: bool,
    pub comment: String,
}
impl Note {
    pub fn format(&self) -> String {
        format!("{}\nComment: {}", self.target, self.comment)
    }
}
pub fn revision(root: &std::path::Path, path: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    let file = root.join(path);
    if let Ok(meta) = std::fs::metadata(&file) {
        meta.len().hash(&mut hash);
        meta.modified().ok().hash(&mut hash);
    }
    if let Ok(repo) = git2::Repository::discover(root) {
        repo.head().ok().and_then(|h| h.target()).hash(&mut hash);
    }
    hash.finish()
}

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub row: usize,
    pub col: usize,
}

pub struct Selection {
    pub anchor: Position,
    pub lines: bool,
}

impl Selection {
    pub fn contains(&self, cursor: Position, pos: Position) -> bool {
        let (lo, hi) = (self.anchor.min(cursor), self.anchor.max(cursor));
        if self.lines { (lo.row..=hi.row).contains(&pos.row) } else { (lo..=hi).contains(&pos) }
    }
}

pub fn text(row: &DRow) -> String {
    row.segs.iter().map(|s| s.text.as_str()).collect()
}

pub fn preview(diff: &FileDiff, selection: Option<&Selection>, cursor: Position) -> Vec<DRow> {
    let Some(selection) = selection else { return Vec::new() };
    diff.rows.iter().enumerate().filter_map(|(i, row)| {
        if matches!(row.kind, RowKind::Hunk | RowKind::Meta)
            || !(selection.anchor.row.min(cursor.row)..=selection.anchor.row.max(cursor.row)).contains(&i) {
            return None;
        }
        let mut row = row.clone();
        let mut col = 0;
        for seg in &mut row.segs {
            seg.text = seg.text.chars().filter(|_| {
                let selected = selection.contains(cursor, Position { row: i, col });
                col += 1;
                selected
            }).collect();
        }
        Some(row)
    }).take(3).collect()
}

pub fn target(path: &str, diff: &FileDiff, selection: Option<&Selection>, cursor: Position) -> String {
    let Some(selection) = selection else {
        return format!("{path} (whole file)");
    };
    let mut out = path.to_string();
    for (r, row) in diff.rows.iter().enumerate() {
        if matches!(row.kind, RowKind::Hunk | RowKind::Meta) {
            continue;
        }
        let content = text(row);
        let selected: String = content
            .chars()
            .enumerate()
            .filter(|(col, _)| selection.contains(cursor, Position { row: r, col: *col }))
            .map(|(_, c)| c)
            .collect();
        if !selected.is_empty()
            || (content.is_empty() && selection.contains(cursor, Position { row: r, col: 0 }))
        {
            let (side, number) = if row.kind == RowKind::Del { ("old", row.old) } else { ("new", row.new) };
            out.push_str(&format!("\n  {side}:{} | {selected}", number.unwrap_or(0)));
        }
    }
    out
}

pub struct Agent {
    pub id: String,
    pub label: String,
}

fn call(args: &[&str]) -> Result<Value, String> {
    let out = crate::rex().args(["--timeout", "3s"]).args(args).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("Invalid Rex response: {e}"))
}

pub fn agents() -> Result<Vec<Agent>, String> {
    let ctx = crate::rex_ctx()?;
    let data = call(&["block", "ls", "-s", &ctx.session, "--json"])?;
    let blocks = data["blocks"].as_array().ok_or("Rex did not return blocks")?;
    let window = blocks
        .iter()
        .find(|b| b["block_id"] == ctx.block)
        .map(|b| &b["window_id"])
        .ok_or("This pane is no longer in the Rex session")?;
    let mut agents = Vec::new();
    for block in blocks.iter().filter(|b| &b["window_id"] == window && b["block_id"] != ctx.block) {
        let Some(id) = block["block_id"].as_str() else {
            continue;
        };
        let Ok(process) = call(&["block", "call", "-s", &ctx.session, "-b", id, "process"]) else {
            continue;
        };
        let fg = &process["foreground"];
        let name = fg["argv0"]
            .as_str()
            .or(fg["name"].as_str())
            .unwrap_or("")
            .rsplit('/')
            .next()
            .unwrap_or("")
            .trim_start_matches('-');
        let status =
            call(&["block", "call", "-s", &ctx.session, "-b", id, "program_status"]).unwrap_or(Value::Null);
        let root = status["records"].as_array().and_then(|records| {
            records
                .iter()
                .find(|r| r["id"].is_null() && (r["owner"].is_null() || r["owner"]["pid"] == fg["pid"]))
        });
        let known = [
            "claude",
            "codex",
            "pi",
            "opencode",
            "gemini",
            "aider",
            "amp",
            "cursor-agent",
            "crush",
            "goose",
            "qwen",
            "vibe",
        ];
        if !fg.is_null() && (known.contains(&name) || root.is_some()) {
            let kind =
                root.and_then(|r| r["effective_app"].as_str()).filter(|s| !s.is_empty()).unwrap_or(name);
            let label = block["label"].as_str().filter(|s| !s.is_empty() && *s != "shell").unwrap_or(kind);
            agents.push(Agent { id: id.into(), label: format!("{label} · {kind} · {id}") });
        }
    }
    Ok(agents)
}

pub fn send(agent: &Agent, notes: &[String]) -> Result<(), String> {
    let ctx = crate::rex_ctx()?;
    // Recheck the destination in case the agent exited while the picker was open.
    if !agents()?.iter().any(|a| a.id == agent.id) {
        return Err("The selected agent is no longer available in this tab".into());
    }
    let payload = format!(
        "\x1b[200~Code review comments from rexdiff:\n\n{}\n\x1b[201~",
        notes.join("\n\n").replace('\x1b', "")
    );
    let out = crate::rex()
        .args(["--timeout", "3s", "send", "-s", &ctx.session, "-b", &agent.id, &payload])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().into()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Seg;

    #[test]
    fn reversed_unicode_selection_and_deleted_line_coordinates() {
        let diff = FileDiff {
            revision: 0,
            path: "x".into(),
            context: 3,
            max_lineno: 9,
            rows: vec![DRow {
                kind: RowKind::Del,
                old: Some(9),
                new: None,
                segs: vec![Seg { text: "aé中z".into(), fg: None, bold: false, emph: false }],
            }],
        };
        let sel = Selection { anchor: Position { row: 0, col: 2 }, lines: false };
        let captured = preview(&diff, Some(&sel), Position { row: 0, col: 1 });
        assert_eq!(text(&captured[0]), "é中");
        assert!(captured[0].kind == RowKind::Del);
        assert_eq!(captured[0].segs[0].fg, diff.rows[0].segs[0].fg);
        assert_eq!(
            target("/repo/x", &diff, Some(&sel), Position { row: 0, col: 1 }),
            "/repo/x\n  old:9 | é中"
        );
        assert_eq!(target("/repo/x", &diff, None, Position::default()), "/repo/x (whole file)");
        let sel = Selection { lines: true, ..sel };
        assert_eq!(target("/repo/x", &diff, Some(&sel), Position::default()), "/repo/x\n  old:9 | aé中z");
    }
}
