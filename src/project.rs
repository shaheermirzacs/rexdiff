use nucleo::{Config, Nucleo, Utf32String};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver},
};

pub struct Browser {
    requests: mpsc::Sender<String>,
    replies: Receiver<(String, Vec<crate::git::FileEntry>)>,
    pub entries: std::collections::BTreeMap<String, Vec<crate::git::FileEntry>>,
}
impl Browser {
    pub fn new(root: PathBuf) -> Self {
        let (tx, requests) = mpsc::channel::<String>();
        let (replies, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for dir in requests {
                let path = root.join(&dir);
                let Ok(real) = path.canonicalize() else {
                    continue;
                };
                if !real.starts_with(&root) {
                    continue;
                }
                let Ok(children) = std::fs::read_dir(path) else {
                    continue;
                };
                let mut entries = Vec::new();
                for child in children.flatten() {
                    if child.file_name() == ".git" {
                        continue;
                    }
                    let Ok(kind) = child.file_type() else {
                        continue;
                    };
                    let path = child
                        .path()
                        .strip_prefix(&root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    if !kind.is_dir() && contained(&root, &path).is_err() {
                        continue;
                    }
                    entries.push(crate::git::FileEntry {
                        path,
                        status: if kind.is_dir() { '\0' } else { ' ' },
                        adds: 0,
                        dels: 0,
                        binary: false,
                        staged: crate::git::Staged::No,
                    });
                }
                if replies.send((dir, entries)).is_err() {
                    break;
                }
            }
        });
        let out = Self {
            requests: tx,
            replies: rx,
            entries: Default::default(),
        };
        out.request("");
        out
    }
    pub fn request(&self, dir: &str) {
        let _ = self.requests.send(dir.into());
    }
    pub fn refresh(&self) {
        for dir in self.entries.keys() {
            self.request(dir);
        }
    }
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        for (path, entries) in self.replies.try_iter() {
            self.entries.insert(path, entries);
            changed = true;
        }
        changed
    }
}

pub fn contained(root: &Path, path: &str) -> Result<PathBuf, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let file = root.join(path).canonicalize().map_err(|e| e.to_string())?;
    if !file.starts_with(&root)
        || file
            .strip_prefix(&root)
            .unwrap()
            .components()
            .any(|c| c.as_os_str() == ".git")
    {
        return Err(
            "Choose a file inside the project, outside Git administrative directories".into(),
        );
    }
    if !file.is_file() {
        return Err("Not a regular file".into());
    }
    Ok(file)
}

pub struct Picker {
    pub query: String,
    pub cursor: usize,
    pub user_selected: bool,
    pub matcher: Nucleo<String>,
    pub paths: Vec<String>,
    rx: Receiver<String>,
    cancel: Arc<AtomicBool>,
    pub scanning: bool,
    results: Vec<String>,
}
impl Picker {
    pub fn new(root: PathBuf) -> Self {
        let matcher = Nucleo::new(Config::DEFAULT.match_paths(), Arc::new(|| {}), Some(1), 1);
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        std::thread::spawn(move || {
            let Ok(repo) = git2::Repository::discover(&root) else {
                return;
            };
            let mut dirs = vec![root.clone()];
            while let Some(dir) = dirs.pop() {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(entries) = std::fs::read_dir(dir) else {
                    continue;
                };
                let mut entries: Vec<_> = entries.flatten().collect();
                entries.sort_by_key(|e| e.file_name());
                for entry in entries {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    if entry.file_name() == ".git" {
                        continue;
                    }
                    if repo.is_path_ignored(entry.path()).unwrap_or(true) {
                        continue;
                    }
                    let Ok(kind) = entry.file_type() else {
                        continue;
                    };
                    if kind.is_dir() {
                        dirs.push(entry.path());
                    } else if kind.is_file() || kind.is_symlink() {
                        let path = entry.path();
                        let Some(rel) = path.strip_prefix(&root).ok().and_then(Path::to_str) else {
                            continue;
                        };
                        if contained(&root, rel).is_ok() && tx.send(rel.into()).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Self {
            query: String::new(),
            cursor: 0,
            user_selected: false,
            matcher,
            paths: Vec::new(),
            rx,
            cancel,
            scanning: true,
            results: Vec::new(),
        }
    }
    pub fn poll(&mut self) {
        let injector = self.matcher.injector();
        for _ in 0..512 {
            match self.rx.try_recv() {
                Ok(path) => {
                    injector.push(path.clone(), |s, cols| {
                        cols[0] = Utf32String::from(s.as_str())
                    });
                    self.paths.push(path);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.scanning = false;
                    break;
                }
                Err(_) => break,
            }
        }
        let status = self.matcher.tick(0);
        if status.changed
            && self.matcher.snapshot().pattern().column_pattern(0).atoms
                == self.matcher.pattern.column_pattern(0).atoms
        {
            let selected = if self.user_selected {
                self.results.get(self.cursor).cloned()
            } else {
                None
            };
            self.results = self
                .matcher
                .snapshot()
                .matched_items(..)
                .map(|m| m.data.clone())
                .collect();
            if self.query.is_empty() {
                self.results.sort();
            }
            self.cursor = selected
                .and_then(|p| self.results.iter().position(|x| x == &p))
                .unwrap_or(0);
        }
    }
    pub fn update(&mut self) {
        self.matcher.pattern.reparse(
            0,
            &self.query,
            nucleo::pattern::CaseMatching::Smart,
            nucleo::pattern::Normalization::Smart,
            false,
        );
        self.cursor = 0;
        self.user_selected = false;
        self.results.clear();
    }
    pub fn matches(&self) -> &[String] {
        &self.results
    }

    pub fn highlighted(&self, path: &str) -> ratatui::text::Line<'static> {
        let mut matcher = nucleo::Matcher::new(Config::DEFAULT.match_paths());
        let mut indices = Vec::new();
        let hay = Utf32String::from(path);
        self.matcher.snapshot().pattern().column_pattern(0).indices(
            hay.slice(..),
            &mut matcher,
            &mut indices,
        );
        ratatui::text::Line::from(
            path.chars()
                .enumerate()
                .map(|(i, c)| {
                    let text = crate::git::sanitize(&c.to_string());
                    if indices.contains(&(i as u32)) {
                        ratatui::text::Span::styled(
                            text,
                            ratatui::style::Style::new().add_modifier(
                                ratatui::style::Modifier::BOLD
                                    | ratatui::style::Modifier::UNDERLINED,
                            ),
                        )
                    } else {
                        ratatui::text::Span::raw(text)
                    }
                })
                .collect::<Vec<_>>(),
        )
    }
}
impl Drop for Picker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_external_and_special_paths() {
        let root = std::env::current_dir().unwrap();
        assert!(contained(&root, "Cargo.toml").is_ok());
        assert!(contained(&root, "/dev/null").is_err());
        assert!(contained(&root, ".git/config").is_err());
        assert!(contained(&root, "src").is_err());
    }
    #[test]
    fn picker_excludes_ignored_files_and_directories() {
        let root = std::env::temp_dir().join(format!("rexdiff-picker-{}", std::process::id()));
        std::fs::create_dir_all(root.join("build")).unwrap();
        let repo = git2::Repository::init(&root).unwrap();
        std::fs::write(root.join(".gitignore"), "build/\n*.log\n").unwrap();
        std::fs::write(root.join("build/output.rs"), "").unwrap();
        std::fs::write(root.join("debug.log"), "").unwrap();
        std::fs::write(root.join("visible.rs"), "").unwrap();
        let mut picker = Picker::new(root.clone());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while picker.scanning {
            picker.poll();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(picker.paths.iter().any(|p| p == "visible.rs"));
        assert!(picker.paths.iter().any(|p| p == ".gitignore"));
        assert!(
            !picker
                .paths
                .iter()
                .any(|p| p == "debug.log" || p.starts_with("build/"))
        );
        drop(picker);
        drop(repo);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn fuzzy_subsequence_ranking_and_unicode() {
        use nucleo::pattern::{CaseMatching, Normalization, Pattern};
        let mut matcher = nucleo::Matcher::new(Config::DEFAULT.match_paths());
        let pattern = Pattern::parse("sapr", CaseMatching::Smart, Normalization::Smart);
        let result = pattern.match_list(
            ["src/app.rs", "something/a/path/really/long", "README.md"],
            &mut matcher,
        );
        assert_eq!(result[0].0, "src/app.rs");
        assert_eq!(result.len(), 2);
        assert_eq!(
            Pattern::parse("ét", CaseMatching::Smart, Normalization::Smart)
                .match_list(["école/test.rs"], &mut matcher)
                .len(),
            1
        );
    }
}
