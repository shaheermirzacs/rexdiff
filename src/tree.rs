//! Folder tree built only from the files that changed.

use std::collections::{BTreeMap, HashSet};

use crate::git::FileEntry;

pub enum Item {
    Dir { path: String, label: String, collapsed: bool, adds: usize, dels: usize },
    File { idx: usize, name: String },
}

pub struct TreeRow {
    /// Tree guides (`│  `, `├─ `, `╰─ `) before the item.
    pub guide: String,
    pub item: Item,
}

impl TreeRow {
    /// The item's full path: used to find it again after a refresh.
    pub fn key<'a>(&'a self, files: &'a [FileEntry]) -> &'a str {
        match &self.item {
            Item::Dir { path, .. } => path,
            Item::File { idx, .. } => &files[*idx].path,
        }
    }
}

#[derive(Default)]
struct Node {
    dirs: BTreeMap<String, Node>,
    files: Vec<(String, usize)>,
}

impl Node {
    fn totals(&self, files: &[FileEntry]) -> (usize, usize) {
        let mut t = self
            .files
            .iter()
            .fold((0, 0), |(a, d), (_, i)| (a + files[*i].adds, d + files[*i].dels));
        for n in self.dirs.values() {
            let (a, d) = n.totals(files);
            t.0 += a;
            t.1 += d;
        }
        t
    }
}

pub fn build(files: &[FileEntry], collapsed: &HashSet<String>) -> Vec<TreeRow> {
    let mut root = Node::default();
    for (idx, f) in files.iter().enumerate() {
        let mut parts: Vec<&str> = f.path.split('/').collect();
        let name = parts.pop().unwrap_or_default().to_string();
        let mut node = &mut root;
        for p in parts {
            node = node.dirs.entry(p.to_string()).or_default();
        }
        node.files.push((name, idx));
    }
    let mut out = Vec::new();
    walk(&root, "", "", true, files, collapsed, &mut out);
    out
}

fn walk(
    node: &Node,
    parent: &str,
    prefix: &str,
    root: bool,
    files: &[FileEntry],
    collapsed: &HashSet<String>,
    out: &mut Vec<TreeRow>,
) {
    let count = node.dirs.len() + node.files.len();
    let mut i = 0;
    let mut next = |out_last: &mut bool| {
        i += 1;
        *out_last = i == count;
    };

    for (name, child) in &node.dirs {
        let mut last = false;
        next(&mut last);

        // Folders with a single subfolder collapse into one row: `src/components/ui`.
        let mut label = name.clone();
        let mut n = child;
        while n.files.is_empty() && n.dirs.len() == 1 {
            let (k, c) = n.dirs.iter().next().expect("one dir");
            label.push('/');
            label.push_str(k);
            n = c;
        }
        let path = if parent.is_empty() { label.clone() } else { format!("{parent}/{label}") };
        let is_collapsed = collapsed.contains(&path);
        let (adds, dels) = n.totals(files);
        let (guide, child_prefix) = guides(prefix, root, last);
        out.push(TreeRow {
            guide,
            item: Item::Dir { path: path.clone(), label, collapsed: is_collapsed, adds, dels },
        });
        if !is_collapsed {
            walk(n, &path, &child_prefix, false, files, collapsed, out);
        }
    }

    for (name, idx) in &node.files {
        let mut last = false;
        next(&mut last);
        let (guide, _) = guides(prefix, root, last);
        out.push(TreeRow { guide, item: Item::File { idx: *idx, name: name.clone() } });
    }
}

fn guides(prefix: &str, root: bool, last: bool) -> (String, String) {
    if root {
        return (String::new(), String::new());
    }
    let guide = format!("{prefix}{}", if last { "╰─ " } else { "├─ " });
    let child = format!("{prefix}{}", if last { "   " } else { "│  " });
    (guide, child)
}
