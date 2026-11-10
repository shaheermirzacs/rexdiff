//! Review notes and delivery through the same Rex CLI used to open the panel.
use serde_json::Value;

use crate::git::DRow;

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
    pub fn range_label(&self) -> String {
        let old_side = self.old_side && self.locations.iter().all(|(old,_)| old.is_some());
        let old = ranges(self.locations.iter().filter_map(|&(old,new)| if new.is_none() || old_side { old } else { None }));
        let new = ranges(self.locations.iter().filter_map(|&(old,new)| if old.is_none() || !old_side { new } else { None }));
        match (old.is_empty(), new.is_empty()) {
            (false,false) => format!("old {old} · new {new}"),
            (false,true) => format!("old {old}"),
            (true,false) => format!("new {new}"),
            (true,true) => "whole file".into(),
        }
    }

    pub fn format(&self) -> String {
        format!("{}\nComment: {}", self.target, self.comment)
    }
}

fn ranges(lines: impl Iterator<Item=u32>) -> String {
    let mut lines: Vec<_> = lines.collect();
    lines.sort_unstable();
    lines.dedup();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let start = lines[i];
        let mut end = start;
        i += 1;
        while i < lines.len() && end.checked_add(1) == Some(lines[i]) { end = lines[i]; i += 1; }
        spans.push(if start == end { start.to_string() } else { format!("{start}–{end}") });
    }
    spans.join(", ")
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


pub fn text(row: &DRow) -> String {
    row.segs.iter().map(|s| s.text.as_str()).collect()
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

    #[test]
    fn ranges_distinguish_diff_sides_and_gaps() {
        let mut note=Note {id:1,path:"x.rs".into(),line:Some(44),old_side:true,
            locations:vec![(Some(44),None),(Some(45),None),(Some(46),None),(Some(47),None)],
            revision:0,target:String::new(),preview:Vec::new(),historical:false,comment:"why?".into()};
        assert_eq!(note.range_label(),"old 44–47");
        note.locations.push((Some(48),Some(46)));
        assert_eq!(note.range_label(),"old 44–48");
        note.locations.pop();
        note.locations.extend([(None,Some(45)),(Some(49),Some(46)),(None,Some(49))]);
        assert_eq!(note.range_label(),"old 44–47 · new 45–46, 49");
        note.locations=vec![(None,Some(45))];
        assert_eq!(note.range_label(),"new 45");
        note.locations.clear();
        assert_eq!(note.range_label(),"whole file");
        assert_eq!(ranges([3,2,2,1,7].into_iter()),"1–3, 7");
    }
}
