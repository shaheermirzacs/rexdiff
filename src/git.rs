//! Reads the repository on its own thread: the list of changed files, the
//! diff (with syntax colors and word-level highlights) of the selected file,
//! and the local branches. It also runs the writes the panel asks for: stage,
//! commit, checkout, and push.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use std::collections::{HashMap, HashSet};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use git2::{BranchType, Delta, DiffOptions, Oid, Patch, Repository, Sort, Status, StatusOptions, Tree};

use crate::highlight::{Highlighter, Span};

/// The history screen shows at most this many commits.
const MAX_HISTORY: usize = 1000;

/// Larger diffs are truncated (nobody reads 50k lines in a pane).
const MAX_ROWS: usize = 50_000;

#[derive(Clone)]
pub struct FileEntry {
    pub path: String,
    pub status: char,
    pub adds: usize,
    pub dels: usize,
    pub binary: bool,
    pub staged: Staged,
}

/// How much of a file's change is in the index (`git add`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Staged {
    No,
    /// Staged, then changed again in the working tree.
    Partly,
    Fully,
}

pub struct Snapshot {
    pub branch: String,
    pub files: Vec<FileEntry>,
}

#[derive(Clone)]
pub struct Comparison {
    pub is_commit: bool,
    pub base: Option<Oid>,
    pub tip: Oid,
    pub label: String,
}

impl Comparison {
    pub fn revision(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        ("comparison", self.base, self.tip).hash(&mut hash);
        hash.finish()
    }
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
#[derive(Clone, PartialEq, Eq)]
pub struct Seg {
    pub text: String,
    pub fg: Option<u8>,
    pub bold: bool,
    pub emph: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct DRow {
    pub kind: RowKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub segs: Vec<Seg>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub revision: u64,
    pub path: String,
    pub context: u32,
    pub rows: Vec<DRow>,
    pub max_lineno: u32,
}

pub enum Job {
    Reload,
    Refresh,
    Diff { path: String, context: u32 },
    Review { request: u64, commit: Option<String>, base: String },
    ReviewDiff { comparison: Comparison, path: String, context: u32 },
    /// `git add` the file (`true`) or take it back out of the index (`false`).
    Stage { path: String, add: bool },
    /// `git commit -m message`.
    Commit { message: String },
    /// Throw away changes: `restore` paths go back to `HEAD` (index and disk),
    /// `new` paths (not in `HEAD`) are unstaged and deleted. `label` names
    /// what was discarded, for the result message.
    Discard { restore: Vec<String>, new: Vec<String>, label: String },
    /// List the local branches.
    Branches,
    /// `git switch <name>`.
    Checkout { name: String, remote: bool },
    /// `git fetch --all --prune`.
    Fetch,
    /// Push the branch to its upstream (or to `origin`, setting the upstream).
    Push { name: String },
    /// Pull the branch from its upstream.
    Pull { name: String },
    /// Update the branch from its upstream, then merge it into the current one.
    Merge { name: String },
    /// The commit history of a local branch.
    History { branch: String },
    /// `git branch -d` (or `-D` with `force`).
    DeleteBranch { name: String, force: bool },
}

/// One commit, for the history screen.
pub struct CommitInfo {
    pub oid: String,
    pub short: String,
    pub subject: String,
    pub author: String,
    pub time: i64,
    /// On the branch but not on its upstream yet.
    pub unpushed: bool,
    pub merge: bool,
    /// Branches and tags pointing at it (`main`, `origin/main`, `tag: v1.0`).
    pub refs: Vec<String>,
}

/// A local branch, for the Branches screen.
pub struct BranchInfo {
    pub name: String,
    pub current: bool,
    /// A remote branch (`origin/feature-y`) that no local branch tracks yet.
    pub remote: bool,
    /// Where it's checked out, when that's another worktree (`git worktree`).
    pub worktree: Option<PathBuf>,
    /// Its upstream (`origin/main`) and how many commits it's ahead and behind.
    pub upstream: Option<(String, usize, usize)>,
    /// The last commit's subject and time (seconds since the epoch).
    pub subject: String,
    pub time: i64,
}

pub enum Msg {
    Review { request: u64, result: Result<(Comparison, Snapshot), String> },
    Snapshot(Result<Snapshot, String>),
    Diff(FileDiff),
    Error(String),
    Branches(Result<Vec<BranchInfo>, String>),
    History { branch: String, commits: Result<Vec<CommitInfo>, String> },
    /// A long-running step is under way ("Pushing main to origin/main…").
    Progress(String),
    /// A commit, checkout, or push finished: a summary, or what went wrong.
    Done(Result<String, String>),
}

pub fn run_worker(cwd: PathBuf, rx: Receiver<Job>, tx: Sender<Msg>) {
    let hl = Highlighter::new();
    let mut repo: Option<Repository> = None;
    let mut cache: Vec<(String, u64, u32, FileDiff)> = Vec::new();
    let mut pending = std::collections::VecDeque::new();

    while let Some(first) = pending.pop_front().or_else(|| rx.recv().ok()) {
        // Coalesce queued requests: only the latest of each kind matters.
        // Stage requests all run, in order.
        let mut refresh = false;
        let mut diff_job = None;
        let mut stage_jobs = Vec::new();
        let mut commit_job = None;
        let mut branches = false;
        let mut history_job = None;
        let mut discard_job = None;
        let mut actions = Vec::new();
        let mut reviews = Vec::new();
        for job in std::iter::once(first).chain(rx.try_iter()) {
            match job {
                Job::Review { .. } | Job::ReviewDiff { .. } => reviews.push(job),
                Job::Reload => { cache.clear(); refresh = true; }
                Job::Branches => branches = true,
                Job::History { branch } => history_job = Some(branch),
                Job::Discard { restore, new, label } => discard_job = Some((restore, new, label)),
                Job::Checkout { .. }
                | Job::Fetch
                | Job::Push { .. }
                | Job::Pull { .. }
                | Job::Merge { .. }
                | Job::DeleteBranch { .. } => {
                    actions.push(job)
                }
                Job::Refresh => refresh = true,
                Job::Diff { path, context } => diff_job = Some((path, context)),
                Job::Stage { path, add } => stage_jobs.push((path, add)),
                Job::Commit { message } => commit_job = Some(message),
            }
        }

        if repo.is_none() {
            repo = Repository::discover(&cwd).ok();
        }
        let Some(r) = repo.as_ref() else {
            let _ = tx.send(Msg::Snapshot(Err("This folder is not a git repository".into())));
            continue;
        };

        for review in reviews {
            match review {
                Job::Review { request, commit, base } => {
                    let result = comparison(r, commit.as_deref(), &base)
                        .and_then(|c| comparison_snapshot(r, &c).map(|s| (c, s)))
                        .map_err(|e| e.message().to_string());
                    let _ = tx.send(Msg::Review { request, result });
                }
                Job::ReviewDiff { comparison, path, context } => {
                    let diff = file_diff_source(r, &hl, &path, context, &tx, || {
                        pending.extend(rx.try_iter());
                        !pending.iter().any(|j| matches!(j, Job::Diff {..} | Job::Review {..} | Job::ReviewDiff {..} | Job::Reload))
                    }, Some(&comparison));
                    if !pending.iter().any(|j| matches!(j, Job::Diff {..} | Job::Review {..} | Job::ReviewDiff {..} | Job::Reload)) {
                        let _ = tx.send(Msg::Diff(diff));
                    }
                }
                _ => unreachable!(),
            }
        }

        for (path, add) in stage_jobs {
            if let Err(e) = stage(r, &path, add) {
                let _ = tx.send(Msg::Error(format!("Could not {} {path}: {}", if add { "stage" } else { "unstage" }, e.message())));
            }
            refresh = true;
        }

        if let Some((restore, new, label)) = discard_job {
            let done = match r.workdir() {
                Some(dir) => discard(dir, &restore, &new, &label),
                None => Err("This repository has no working tree".into()),
            };
            let _ = tx.send(Msg::Done(done));
            refresh = true;
        }

        if let Some(message) = commit_job {
            let done = match r.workdir() {
                Some(dir) => commit(dir, &message),
                None => Err("This repository has no working tree".into()),
            };
            let _ = tx.send(Msg::Done(done));
            refresh = true;
        }

        for job in actions {
            let Some(dir) = r.workdir() else { continue };
            let done = match job {
                Job::Checkout { name, remote } => checkout(dir, &name, remote),
                Job::Fetch => fetch(dir, &tx),
                Job::DeleteBranch { name, force } => delete_branch(dir, &name, force),
                Job::Push { name } => push(dir, &name, &tx),
                Job::Pull { name } => pull(dir, &name, &tx),
                Job::Merge { name } => merge_into_current(dir, &name, &tx),
                _ => continue,
            };
            let _ = tx.send(Msg::Done(done));
            refresh = true;
            branches = true;
        }

        if let Some(branch) = history_job {
            let commits = history(r, &branch).map_err(|e| e.message().to_string());
            if tx.send(Msg::History { branch, commits }).is_err() {
                return;
            }
        }

        if branches {
            let list = list_branches(r).map_err(|e| e.message().to_string());
            if tx.send(Msg::Branches(list)).is_err() {
                return;
            }
        }

        if refresh {
            let snap = snapshot(r).map_err(|e| e.message().to_string());
            if tx.send(Msg::Snapshot(snap)).is_err() {
                return;
            }
        }
        if let Some((path, context)) = diff_job {
            let revision = crate::annotations::revision(r.workdir().unwrap_or(&cwd), &path);
            cache.retain(|(p, rev, _, _)| p == &path && *rev == revision);
            if let Some((_, _, _, d)) = cache.iter().find(|(_,_,c,_)| *c == context) {
                let _ = tx.send(Msg::Diff(d.clone()));
                continue;
            }
            let diff = file_diff(r, &hl, &path, context, &tx, || {
                pending.extend(rx.try_iter());
                !pending.iter().any(|j| matches!(j, Job::Diff {..} | Job::Review {..} | Job::ReviewDiff {..} | Job::Reload))
            });
            if pending.iter().any(|j| matches!(j, Job::Diff {..} | Job::Review {..} | Job::ReviewDiff {..} | Job::Reload)) { continue; }
            cache.push((path, revision, context, diff.clone()));
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

/// Local branches, most recently committed first.
fn list_branches(repo: &Repository) -> Result<Vec<BranchInfo>, git2::Error> {
    let worktrees = repo.workdir().map(worktree_branches).unwrap_or_default();
    let mut out = Vec::new();
    for b in repo.branches(Some(BranchType::Local))? {
        let (branch, _) = b?;
        let Some(name) = branch.name()?.map(str::to_string) else { continue };
        let commit = branch.get().peel_to_commit().ok();
        let (subject, time) = commit
            .map(|c| (c.summary().ok().flatten().unwrap_or_default().to_string(), c.time().seconds()))
            .unwrap_or_default();
        let upstream = branch.upstream().ok().and_then(|u| {
            let uname = u.name().ok().flatten()?.to_string();
            let (ahead, behind) = repo.graph_ahead_behind(branch.get().target()?, u.get().target()?).ok()?;
            Some((uname, ahead, behind))
        });
        let current = branch.is_head();
        let worktree = if current { None } else { worktrees.get(&name).cloned() };
        out.push(BranchInfo { name, current, remote: false, worktree, upstream, subject, time });
    }
    out.sort_by(|a, b| b.time.cmp(&a.time).then_with(|| a.name.cmp(&b.name)));

    // Then the remote branches no local branch tracks yet, so they can be
    // checked out (`origin/HEAD` is just an alias, so it's left out).
    let tracked: HashSet<String> = out.iter().filter_map(|b| b.upstream.as_ref().map(|u| u.0.clone())).collect();
    let mut remotes = Vec::new();
    for b in repo.branches(Some(BranchType::Remote))? {
        let (branch, _) = b?;
        let Some(name) = branch.name()?.map(str::to_string) else { continue };
        if name.ends_with("/HEAD") || tracked.contains(&name) {
            continue;
        }
        let (subject, time) = branch
            .get()
            .peel_to_commit()
            .map(|c| (c.summary().ok().flatten().unwrap_or_default().to_string(), c.time().seconds()))
            .unwrap_or_default();
        remotes.push(BranchInfo { name, current: false, remote: true, worktree: None, upstream: None, subject, time });
    }
    remotes.sort_by(|a, b| b.time.cmp(&a.time).then_with(|| a.name.cmp(&b.name)));
    out.extend(remotes);
    Ok(out)
}

/// The newest commits of a local branch, marking the ones its upstream doesn't
/// have and the refs that point at each.
fn history(repo: &Repository, name: &str) -> Result<Vec<CommitInfo>, git2::Error> {
    let branch = repo
        .find_branch(name, BranchType::Local)
        .or_else(|_| repo.find_branch(name, BranchType::Remote)).ok();
    let tip = repo.revparse_single(name)?.peel_to_commit()?.id();
    let unpushed: HashSet<Oid> = match branch.and_then(|b| b.upstream().ok()).and_then(|u| u.get().target()) {
        Some(up) => {
            let mut walk = repo.revwalk()?;
            walk.push(tip)?;
            walk.hide(up)?;
            walk.take(MAX_HISTORY).filter_map(Result::ok).collect()
        }
        None => HashSet::new(),
    };
    let labels = ref_labels(repo);

    let mut walk = repo.revwalk()?;
    walk.push(tip)?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)?;
    let mut out = Vec::new();
    for oid in walk.take(MAX_HISTORY) {
        let oid = oid?;
        let c = repo.find_commit(oid)?;
        let author = c.author();
        out.push(CommitInfo {
            oid: oid.to_string(),
            short: c.as_object().short_id()?.as_str().unwrap_or_default().to_string(),
            subject: sanitize(c.summary().ok().flatten().unwrap_or_default()),
            author: author.name().ok().unwrap_or("?").to_string(),
            time: c.time().seconds(),
            unpushed: unpushed.contains(&oid),
            merge: c.parent_count() > 1,
            refs: labels.get(&oid).cloned().unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Branches (local and remote) and tags, by the commit they point at.
fn ref_labels(repo: &Repository) -> HashMap<Oid, Vec<String>> {
    let mut map: HashMap<Oid, Vec<String>> = HashMap::new();
    let Ok(refs) = repo.references() else { return map };
    for r in refs.flatten() {
        let Some(short) = r.shorthand().ok().map(str::to_string) else { continue };
        if short == "HEAD" || short.ends_with("/HEAD") {
            continue;
        }
        let Ok(commit) = r.peel_to_commit() else { continue };
        let label = if r.is_tag() { format!("tag: {short}") } else { short };
        map.entry(commit.id()).or_default().push(label);
    }
    // `main` and `origin/main` on the same commit: just `main` (the Branches
    // screen already says it's in sync).
    for labels in map.values_mut() {
        let locals: Vec<String> = labels.iter().filter(|l| !l.contains('/') || !is_remote_label(repo, l)).cloned().collect();
        labels.retain(|l| {
            !is_remote_label(repo, l)
                || !locals.iter().any(|local| l.split_once('/').is_some_and(|(_, rest)| rest == local))
        });
    }
    map
}

/// `origin/main` is a remote label; `feature/x` (a local branch) is not.
fn is_remote_label(repo: &Repository, label: &str) -> bool {
    label.split_once('/').is_some_and(|(remote, _)| repo.find_remote(remote).is_ok())
}

/// Deletes a local branch. Without `force`, git refuses when the branch has
/// commits that aren't on its upstream or the current branch.
fn delete_branch(dir: &Path, name: &str, force: bool) -> Result<String, String> {
    run_git(dir, &["branch", if force { "-D" } else { "-d" }, name])?;
    Ok(format!("Deleted branch {name}"))
}

/// Branch name → the worktree folder it's checked out in, from
/// `git worktree list --porcelain` (blocks of `worktree <path>` and
/// `branch refs/heads/<name>`).
fn worktree_branches(dir: &Path) -> HashMap<String, PathBuf> {
    let Ok(out) = run_git(dir, &["worktree", "list", "--porcelain"]) else { return HashMap::new() };
    let mut map = HashMap::new();
    let mut path = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(p));
        } else if let (Some(b), Some(p)) = (line.strip_prefix("branch refs/heads/"), &path) {
            map.insert(b.to_string(), p.clone());
        }
    }
    map
}

/// Checks out a local branch, or creates a local branch tracking a remote one
/// (`origin/feature-y` → `feature-y`) and switches to it.
fn checkout(dir: &Path, name: &str, remote: bool) -> Result<String, String> {
    if remote {
        let local = name.split_once('/').map_or(name, |(_, rest)| rest);
        run_git(dir, &["switch", "--track", name]).map_err(|e| {
            if e.contains("already exists") {
                format!("A local branch {local} already exists: check that one out")
            } else {
                e
            }
        })?;
        return Ok(format!("Switched to {local}, tracking {name}"));
    }
    run_git(dir, &["switch", name])?;
    Ok(format!("Switched to {name}"))
}

/// Downloads what's new on every remote and drops remote branches that were
/// deleted there.
fn fetch(dir: &Path, tx: &Sender<Msg>) -> Result<String, String> {
    let _ = tx.send(Msg::Progress("Fetching from all remotes…".into()));
    run_git(dir, &["fetch", "--all", "--prune"])?;
    Ok("Fetched from all remotes".into())
}

/// Pushes `name` to its upstream, or to `origin` with `--set-upstream` when it
/// doesn't have one. Works for any local branch, not just the current one.
fn push(dir: &Path, name: &str, tx: &Sender<Msg>) -> Result<String, String> {
    let config = |key: &str| run_git(dir, &["config", "--get", &format!("branch.{name}.{key}")]).ok();
    let (args, target) = match (config("remote"), config("merge")) {
        (Some(remote), Some(merge)) => {
            let target = format!("{remote}/{}", merge.trim_start_matches("refs/heads/"));
            (vec!["push".to_string(), remote, format!("{name}:{merge}")], target)
        }
        _ => {
            let remotes = run_git(dir, &["remote"]).unwrap_or_default();
            let remote = if remotes.lines().any(|r| r == "origin") {
                "origin".to_string()
            } else {
                remotes.lines().next().map(str::to_string).ok_or("No remote to push to")?
            };
            let target = format!("{remote}/{name}");
            (vec!["push".into(), "-u".into(), remote, name.to_string()], target)
        }
    };
    let _ = tx.send(Msg::Progress(format!("Pushing {name} to {target}…")));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run_git(dir, &args)?;
    Ok(format!("Pushed {name} to {target}"))
}

/// Pulls `name` from its upstream. The current branch gets a regular
/// `git pull` (merge or rebase, as configured). Another branch can only be
/// fast-forwarded, since merging needs a checkout.
fn pull(dir: &Path, name: &str, tx: &Sender<Msg>) -> Result<String, String> {
    let config = |key: &str| run_git(dir, &["config", "--get", &format!("branch.{name}.{key}")]).ok();
    let (Some(remote), Some(merge)) = (config("remote"), config("merge")) else {
        return Err(format!("{name} has no upstream to pull from (push it first)"));
    };
    let target = format!("{remote}/{}", merge.trim_start_matches("refs/heads/"));
    let current = run_git(dir, &["symbolic-ref", "--short", "HEAD"]).ok();
    let _ = tx.send(Msg::Progress(format!("Pulling {target} into {name}…")));

    if current.as_deref() == Some(name) {
        let out = run_git(dir, &["pull"])?;
        if out.contains("Already up to date") {
            return Ok(format!("{name} is already up to date"));
        }
    } else {
        fast_forward(dir, name, &remote, &merge)
            .map_err(|e| if e == DIVERGED { format!("{name} has diverged from {target}: check it out to pull") } else { e })?;
    }
    Ok(format!("Pulled {target} into {name}"))
}

const DIVERGED: &str = "diverged";

/// Moves the local branch `name` (not checked out) up to its upstream with
/// `git fetch <remote> <merge>:<name>`, which only succeeds as a fast-forward.
/// Fails with `DIVERGED` when the branch has commits the remote doesn't.
fn fast_forward(dir: &Path, name: &str, remote: &str, merge: &str) -> Result<(), String> {
    run_git(dir, &["fetch", remote, &format!("{merge}:refs/heads/{name}")]).map(|_| ()).map_err(|e| {
        if e.contains("non-fast-forward") || e.contains("rejected") { DIVERGED.to_string() } else { e }
    })
}

/// Brings `name` into the branch you're on: first updates `name` from its
/// upstream (when it has one), then `git merge name`.
fn merge_into_current(dir: &Path, name: &str, tx: &Sender<Msg>) -> Result<String, String> {
    let current = run_git(dir, &["symbolic-ref", "--short", "HEAD"])
        .map_err(|_| "HEAD is detached: check out a branch first".to_string())?;
    if current == name {
        return Err(format!("{name} is the branch you're on: pick another one to merge"));
    }

    let config = |key: &str| run_git(dir, &["config", "--get", &format!("branch.{name}.{key}")]).ok();
    if let (Some(remote), Some(merge)) = (config("remote"), config("merge")) {
        let target = format!("{remote}/{}", merge.trim_start_matches("refs/heads/"));
        let _ = tx.send(Msg::Progress(format!("Updating {name} from {target}…")));
        fast_forward(dir, name, &remote, &merge).map_err(|e| {
            if e == DIVERGED {
                format!("{name} has diverged from {target}: check it out and pull first")
            } else {
                format!("Could not update {name}: {e}")
            }
        })?;
    }

    let _ = tx.send(Msg::Progress(format!("Merging {name} into {current}…")));
    let out = run_git(dir, &["merge", name])?;
    if out.contains("Already up to date") {
        return Ok(format!("{current} already has everything from {name}"));
    }
    Ok(format!("Merged {name} into {current}"))
}

/// Throws away changes. Files `HEAD` has go back to it, in the index and on
/// disk; new files are taken out of the index and deleted, along with folders
/// that end up empty.
fn discard(dir: &Path, restore: &[String], new: &[String], label: &str) -> Result<String, String> {
    if !restore.is_empty() {
        let mut args = vec!["restore", "--source=HEAD", "--staged", "--worktree", "--"];
        args.extend(restore.iter().map(String::as_str));
        run_git(dir, &args)?;
    }
    if !new.is_empty() {
        let mut args = vec!["rm", "--cached", "--quiet", "--ignore-unmatch", "--"];
        args.extend(new.iter().map(String::as_str));
        run_git(dir, &args)?;
        for path in new {
            let file = dir.join(path);
            match std::fs::remove_file(&file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("Could not delete {path}: {e}")),
            }
            // Remove the folders this left empty, up to the repository root.
            let mut parent = file.parent();
            while let Some(d) = parent {
                if d == dir || std::fs::remove_dir(d).is_err() {
                    break;
                }
                parent = d.parent();
            }
        }
    }
    Ok(format!("Discarded {label}"))
}

/// `git commit -m message` with what's staged.
fn commit(dir: &Path, message: &str) -> Result<String, String> {
    run_git(dir, &["commit", "-m", message])?;
    let hash = run_git(dir, &["rev-parse", "--short", "HEAD"]).unwrap_or_default();
    Ok(format!("Committed {hash}"))
}

/// Runs a git command and returns its output, or the most useful line of its
/// error. It runs in its own session, detached from the panel's terminal, so a
/// hook that asks for input can't take over the screen (it fails instead).
fn run_git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        // A pull that merges takes the default message instead of opening an editor.
        .env("GIT_MERGE_AUTOEDIT", "no");
    // SAFETY: setsid only touches the child process, between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let out = cmd.output().map_err(|e| format!("could not run git: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("hint:"))
        .collect();
    let key = ["conflict", "error", "fatal", "rejected", "nothing to commit", "no changes added"];
    let line = lines
        .iter()
        .find(|l| key.iter().any(|k| l.to_lowercase().contains(k)))
        .or(lines.last())
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|| format!("git {} failed", args[0]));
    Err(friendly(&line))
}

/// Plain words for the git errors people hit most when switching, pushing, and pulling.
fn friendly(line: &str) -> String {
    let l = line.to_lowercase();
    if l.contains("[rejected]") && (l.contains("fetch first") || l.contains("non-fast-forward")) {
        "Push rejected: the remote has new commits, pull first".into()
    } else if l.contains("not fully merged") {
        "This branch has commits that aren't merged anywhere: press D to delete it anyway".into()
    } else if l.contains("would be overwritten by") {
        "Your changes would be overwritten: commit or stash them first".into()
    } else if l.contains("conflict") {
        "Merge conflicts: fix them and commit, or run `git merge --abort`".into()
    } else if l.contains("divergent branches") {
        "Branches diverged: run `git config pull.rebase false` (merge) or `true` (rebase)".into()
    } else if l.contains("permission denied") || l.contains("authentication failed") {
        format!("Authentication failed ({line})")
    } else if l.contains("could not read from remote") || l.contains("could not resolve host") {
        format!("Could not reach the remote ({line})")
    } else {
        line.to_string()
    }
}

/// `git add <path>` / `git restore --staged <path>`.
fn stage(repo: &Repository, path: &str, add: bool) -> Result<(), git2::Error> {
    if add {
        let mut idx = repo.index()?;
        idx.read(false)?;
        let on_disk = repo.workdir().is_some_and(|w| w.join(path).symlink_metadata().is_ok());
        if on_disk {
            idx.add_path(Path::new(path))?;
        } else {
            // Deleted file: staging it means removing it from the index.
            idx.remove_path(Path::new(path))?;
        }
        idx.write()
    } else {
        // Put the HEAD version back in the index (or drop the entry for a file
        // HEAD doesn't have, including in a repository without commits).
        let head = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        repo.reset_default(head.as_ref().map(|c| c.as_object()), [path])
    }
}

/// Index state of every changed path.
fn staged_states(repo: &Repository) -> HashMap<String, Staged> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true).include_ignored(false);
    let Ok(statuses) = repo.statuses(Some(&mut opts)) else { return HashMap::new() };
    let index = Status::INDEX_NEW
        | Status::INDEX_MODIFIED
        | Status::INDEX_DELETED
        | Status::INDEX_RENAMED
        | Status::INDEX_TYPECHANGE;
    let worktree = Status::WT_NEW
        | Status::WT_MODIFIED
        | Status::WT_DELETED
        | Status::WT_RENAMED
        | Status::WT_TYPECHANGE;
    statuses
        .iter()
        .filter_map(|e| {
            let st = e.status();
            let state = match (st.intersects(index), st.intersects(worktree)) {
                (true, false) => Staged::Fully,
                (true, true) => Staged::Partly,
                _ => Staged::No,
            };
            Some((e.path().ok()?.to_string(), state))
        })
        .collect()
}

fn comparison(repo: &Repository, commit: Option<&str>, base: &str) -> Result<Comparison, git2::Error> {
    if let Some(oid) = commit {
        let commit = repo.find_commit(Oid::from_str(oid)?)?;
        let base = if commit.parent_count() == 0 { None } else { Some(commit.parent_id(0)?) };
        return Ok(Comparison { is_commit: true, base, tip: commit.id(), label: format!("commit {} · {}", &oid[..7], sanitize(commit.summary().ok().flatten().unwrap_or_default())) });
    }
    let base = if base.trim().is_empty() {
        let remote_default = repo.references_glob("refs/remotes/*/HEAD")?.flatten()
            .find_map(|r| r.symbolic_target().ok().flatten().map(str::to_owned));
        remote_default.or_else(|| ["refs/remotes/origin/main", "refs/remotes/origin/master", "refs/heads/main", "refs/heads/master"]
            .into_iter().find(|name| repo.find_reference(name).is_ok()).map(str::to_owned))
            .ok_or_else(|| git2::Error::from_str("No default branch found; press B and enter a base branch"))?
    } else { base.trim().to_string() };
    let tip = repo.head()?.peel_to_commit()?.id();
    let target = repo.revparse_single(&base)?.peel_to_commit()?.id();
    let merge_base = repo.merge_base(target, tip)?;
    let label = base.strip_prefix("refs/remotes/").or_else(|| base.strip_prefix("refs/heads/")).unwrap_or(&base);
    Ok(Comparison { is_commit: false, base: Some(merge_base), tip, label: format!("{label}...{} · {}", branch_name(repo), &tip.to_string()[..7]) })
}

fn comparison_snapshot(repo: &Repository, comparison: &Comparison) -> Result<Snapshot, git2::Error> {
    let old = comparison.base.map(|id| repo.find_commit(id)?.tree()).transpose()?;
    let new = repo.find_commit(comparison.tip)?.tree()?;
    let mut opts = base_opts();
    opts.context_lines(0);
    let diff = repo.diff_tree_to_tree(old.as_ref(), Some(&new), Some(&mut opts))?;
    Ok(Snapshot { branch: comparison.label.clone(), files: diff_files(&diff, &HashMap::new())? })
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

    let staged = staged_states(repo);
    Ok(Snapshot { branch: branch_name(repo), files: diff_files(&diff, &staged)? })
}

fn diff_files(diff: &git2::Diff<'_>, staged: &HashMap<String, Staged>) -> Result<Vec<FileEntry>, git2::Error> {
    let mut files = Vec::with_capacity(diff.deltas().len());
    for i in 0..diff.deltas().len() {
        let delta = diff.get_delta(i).expect("delta in range");
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (adds, dels, binary) = match Patch::from_diff(diff, i)? {
            Some(p) => {
                let (_, a, d) = p.line_stats()?;
                (a, d, p.delta().flags().is_binary())
            }
            None => (0, 0, true),
        };
        let staged = staged.get(&path).copied().unwrap_or(Staged::No);
        files.push(FileEntry { path, status: status_char(delta.status()), adds, dels, binary, staged });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
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

fn file_diff(repo: &Repository, hl: &Highlighter, path: &str, context: u32, tx: &Sender<Msg>, keep_going: impl FnMut() -> bool) -> FileDiff {
    file_diff_source(repo, hl, path, context, tx, keep_going, None)
}

fn file_diff_source(repo: &Repository, hl: &Highlighter, path: &str, context: u32, tx: &Sender<Msg>, mut keep_going: impl FnMut() -> bool, comparison: Option<&Comparison>) -> FileDiff {
    let revision = comparison.map_or_else(|| crate::annotations::revision(repo.workdir().unwrap_or(Path::new(".")),path), Comparison::revision);
    let mut out = FileDiff { revision, path: path.to_string(), context, rows: Vec::new(), max_lineno: 0 };
    let raw = match comparison { Some(c) => collect_comparison_raw(repo, c, path, context), None => collect_raw(repo, path, context) };
    match raw {
        Ok(Some(raw)) => {
            out.max_lineno = raw.iter().filter_map(|r| r.new.max(r.old)).max().unwrap_or(0);
            out.rows = raw.iter().map(|r| DRow { kind: r.kind, old: r.old, new: r.new, segs: build_segs(&r.text, &[], &[]) }).collect();
            let _ = tx.send(Msg::Diff(out.clone()));
            let mut published = 0;
            out.rows = colorize(raw, hl, path, &mut keep_going, &mut |rows| {
                out.rows[published..rows.len()].clone_from_slice(&rows[published..]);
                published = rows.len();
                let _ = tx.send(Msg::Diff(out.clone()));
            });
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
    if let Some(root) = repo.workdir() {
        let file = root.join(path);
        if file.exists() {
            let safe = crate::project::contained(root, path).map_err(|e| git2::Error::from_str(&e))?;
            if std::fs::metadata(safe).map_err(|e| git2::Error::from_str(&e.to_string()))?.len() > 8 * 1024 * 1024 {
                return Err(git2::Error::from_str("File exceeds the 8 MiB preview limit"));
            }
        }
    }
    let tree = head_tree(repo);
    let mut opts = base_opts();
    opts.context_lines(context).pathspec(path).disable_pathspec_match(true);
    let diff = repo.diff_tree_to_workdir_with_index(tree.as_ref(), Some(&mut opts))?;
    if diff.deltas().len() == 0 {
        let root = repo.workdir().ok_or_else(|| git2::Error::from_str("No working tree"))?;
        let file = crate::project::contained(root, path).map_err(|e| git2::Error::from_str(&e))?;
        if std::fs::metadata(&file).map_err(|e| git2::Error::from_str(&e.to_string()))?.len() > 8 * 1024 * 1024 {
            return Err(git2::Error::from_str("File exceeds the 8 MiB preview limit"));
        }
        let bytes = std::fs::read(file).map_err(|e| git2::Error::from_str(&e.to_string()))?;
        if bytes.contains(&0) { return Ok(None); }
        let mut raw: Vec<_> = String::from_utf8_lossy(&bytes).lines().take(MAX_ROWS+1).enumerate().map(|(i,s)| Raw { kind: RowKind::Ctx, old: None, new: Some(i as u32+1), text: s.into() }).collect();
        if raw.len() > MAX_ROWS { raw.pop(); raw.push(Raw {kind:RowKind::Meta,old:None,new:None,text:format!("… file truncated to the first {MAX_ROWS} lines")}); }
        return Ok(Some(raw));
    }
    patch_rows(&diff)
}

fn collect_comparison_raw(repo: &Repository, comparison: &Comparison, path: &str, context: u32) -> Result<Option<Vec<Raw>>, git2::Error> {
    let old = comparison.base.map(|id| repo.find_commit(id)?.tree()).transpose()?;
    let new = repo.find_commit(comparison.tip)?.tree()?;
    for tree in old.iter().chain(std::iter::once(&new)) {
        if let Ok(entry) = tree.get_path(Path::new(path))
            && let Ok(blob) = repo.find_blob(entry.id())
            && blob.size() > 8 * 1024 * 1024 {
            return Err(git2::Error::from_str("File exceeds the 8 MiB preview limit"));
        }
    }
    let mut opts = base_opts();
    opts.context_lines(context).pathspec(path).disable_pathspec_match(true);
    let diff = repo.diff_tree_to_tree(old.as_ref(), Some(&new), Some(&mut opts))?;
    if diff.deltas().len() == 0 { return Ok(Some(Vec::new())); }
    patch_rows(&diff)
}

fn patch_rows(diff: &git2::Diff<'_>) -> Result<Option<Vec<Raw>>, git2::Error> {
    let Some(patch) = Patch::from_diff(diff, 0)? else { return Ok(None) };
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
fn colorize(raw: Vec<Raw>, hl: &Highlighter, path: &str, keep_going: &mut impl FnMut() -> bool, progress: &mut impl FnMut(&[DRow])) -> Vec<DRow> {
    let emph = word_emphasis(&raw);
    let mut file_hl = hl.for_path(path, raw.len());
    let mut rows = Vec::with_capacity(raw.len());
    let mut last_publish = std::time::Instant::now();
    for (i, (r, em)) in raw.into_iter().zip(emph).enumerate() {
        if i % 64 == 0 && !keep_going() { break; }
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
        rows.push(DRow { kind: r.kind, old: r.old, new: r.new, segs });
        if rows.len() == 64 || last_publish.elapsed() >= std::time::Duration::from_millis(100) {
            progress(&rows);
            last_publish = std::time::Instant::now();
        }
    }
    rows
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn commit_and_branch_comparisons_ignore_the_worktree() {
        let dir = std::env::temp_dir().join(format!("rexdiff-review-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Repository::init(&dir).unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        let commit = |files: &[(&str, &[u8])], parents: &[Oid]| {
            let mut builder = repo.treebuilder(None).unwrap();
            for (path, bytes) in files { builder.insert(path, repo.blob(bytes).unwrap(), 0o100644).unwrap(); }
            let tree = repo.find_tree(builder.write().unwrap()).unwrap();
            let parents: Vec<_> = parents.iter().map(|id| repo.find_commit(*id).unwrap()).collect();
            repo.commit(None, &sig, &sig, "fixture", &tree, &parents.iter().collect::<Vec<_>>()).unwrap()
        };
        let root = commit(&[("sample.rs", b"base\n"), ("gone.txt", b"deleted\n")], &[]);
        let first = commit(&[("sample.rs", b"first\n")], &[root]);
        let tip = commit(&[("sample.rs", b"second\n"), ("binary", b"\0binary"), ("新.txt", b"new\n")], &[first]);
        let main = commit(&[("sample.rs", b"base\n"), ("gone.txt", b"deleted\n"), ("main-only", b"main\n")], &[root]);
        repo.reference("refs/heads/main", main, true, "fixture").unwrap();
        repo.reference("refs/heads/feature", tip, true, "fixture").unwrap();
        repo.set_head("refs/heads/feature").unwrap();
        std::fs::write(dir.join("sample.rs"), "staged\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("sample.rs")).unwrap(); index.write().unwrap();
        std::fs::write(dir.join("sample.rs"), "dirty\n").unwrap();
        let index_before = std::fs::read(dir.join(".git/index")).unwrap();

        let branch = comparison(&repo, None, "").unwrap();
        assert_eq!(branch.base, Some(root));
        assert_eq!(branch.tip, tip);
        let snapshot = comparison_snapshot(&repo, &branch).unwrap();
        assert!(snapshot.files.iter().any(|f| f.path == "gone.txt" && f.status == 'D'));
        assert!(snapshot.files.iter().any(|f| f.path == "新.txt"));
        assert!(!snapshot.files.iter().any(|f| f.path == "main-only"));
        assert!(collect_comparison_raw(&repo, &branch, "binary", 3).unwrap().is_none());
        let raw = collect_comparison_raw(&repo, &branch, "sample.rs", 1_000_000).unwrap().unwrap();
        assert!(raw.iter().any(|r| r.kind == RowKind::Add && r.text == "second"));
        assert!(raw.iter().any(|r| r.kind == RowKind::Del && r.text == "base"));
        assert!(!raw.iter().any(|r| r.text == "dirty" || r.text == "staged"));
        let single = comparison(&repo, Some(&tip.to_string()), "").unwrap();
        assert_eq!(single.base, Some(first));
        assert!(!comparison_snapshot(&repo, &single).unwrap().files.iter().any(|f| f.path == "gone.txt"));
        let initial = comparison(&repo, Some(&root.to_string()), "").unwrap();
        assert!(initial.base.is_none());
        assert_eq!(comparison_snapshot(&repo, &initial).unwrap().files.len(), 2);
        let merge = commit(&[("sample.rs", b"merged\n")], &[tip, main]);
        assert_eq!(comparison(&repo, Some(&merge.to_string()), "").unwrap().base, Some(tip));
        assert!(comparison(&repo, None, "missing-base").is_err());
        assert!(comparison_snapshot(&repo, &comparison(&repo, None, "feature").unwrap()).unwrap().files.is_empty());
        assert_eq!(history(&repo, "HEAD").unwrap()[0].oid, tip.to_string());
        assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), index_before);
        assert_eq!(std::fs::read_to_string(dir.join("sample.rs")).unwrap(), "dirty\n");
        assert_eq!(repo.head().unwrap().target(), Some(tip));
        drop(index); drop(repo);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn progressive_load_cache_and_unchanged_file() {
        let dir = std::env::temp_dir().join(format!("rexdiff-load-{}-{}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir(&dir).unwrap();
        let repo = Repository::init(&dir).unwrap();
        let source: String = (0..2000).map(|i| format!("let item_{i} = {i};\n")).collect();
        std::fs::write(dir.join("sample.rs"),&source).unwrap();
        let mut index = repo.index().unwrap(); index.add_path(Path::new("sample.rs")).unwrap(); index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("test","test@example.com").unwrap();
        repo.commit(Some("HEAD"),&sig,&sig,"fixture",&tree,&[]).unwrap();
        std::fs::write(dir.join("sample.rs"),source.replace("item_1000 = 1000", "item_1000 = 42")).unwrap();
        let raw_start = Instant::now();
        let raw = collect_raw(&repo,"sample.rs",1_000_000).unwrap().unwrap();
        eprintln!("full extraction: {:?}",raw_start.elapsed());
        let hl = Highlighter::new();
        let color_start = Instant::now();
        let _rows = colorize(raw,&hl,"sample.rs",&mut || true,&mut |_| {});
        eprintln!("full highlighting: {:?}",color_start.elapsed());
        let (tx,rx) = std::sync::mpsc::channel();
        let (result,msgs) = std::sync::mpsc::channel();
        let cwd = dir.clone();
        let worker = std::thread::spawn(move || run_worker(cwd,rx,result));
        for context in [3,1_000_000] {
            tx.send(Job::Diff{path:"sample.rs".into(),context}).unwrap();
            let Msg::Diff(plain) = msgs.recv_timeout(Duration::from_secs(10)).unwrap() else { panic!() };
            assert!(plain.rows.iter().flat_map(|r| &r.segs).all(|s|s.fg.is_none()));
            tx.send(Job::Refresh).unwrap();
            let mut updates = 0;
            loop {
                match msgs.recv_timeout(Duration::from_secs(10)).unwrap() {
                    Msg::Diff(colored) => {
                        assert_eq!(plain.rows.len(),colored.rows.len());
                        assert!(colored.rows.iter().flat_map(|r| &r.segs).any(|s|s.fg.is_some()));
                        updates += 1;
                    }
                    Msg::Snapshot(_) => break,
                    _ => panic!(),
                }
            }
            assert!(updates >= if context == 3 { 1 } else { 2 });
        }
        let start = Instant::now();
        tx.send(Job::Diff{path:"sample.rs".into(),context:3}).unwrap();
        assert!(matches!(msgs.recv_timeout(Duration::from_secs(10)).unwrap(),Msg::Diff(_)));
        eprintln!("warm cached mode: {:?}",start.elapsed());
        assert!(msgs.recv_timeout(Duration::from_millis(30)).is_err());
        std::fs::write(dir.join(".gitignore"),"ignored.txt\n").unwrap();
        std::fs::write(dir.join("ignored.txt"),"read and annotate me\n").unwrap();
        let raw = collect_raw(&repo,"ignored.txt",3).unwrap().unwrap(); assert_eq!(raw[0].text,"read and annotate me");
        assert_eq!(raw[0].new,Some(1));
        drop(tx); worker.join().unwrap(); drop(tree); drop(index); drop(repo);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
