# rexdiff

Your git changes in a [Rex](https://superlogical.com/rex) pane, right next to
your terminal: a tree of the files that changed and a colored diff of the
selected file. Review them, mark what you've read, commit, and handle your
branches (fetch, checkout, pull, push, merge, delete, history) without leaving
the panel.

```
 Changes  6 files   Branches   ⎇ main   ✓ 2/6 staged             +31 −13
──────────────────────────────────────────────────────────────────────
 ▾ app/                                                         +24 −11
 ├─ ▾ services/                                                 +18 −10
▎│  ├─ ✓ A frete.py                                                  +7
 │  ╰─ ○ M pagamento.py                                         +11 −10
 ╰─ ▾ views/ ✓                                                    +4 −1
    ╰─ ✓ M pedido.py                                              +4 −1
 ○ M README.md                                                    +7 −1

 ────────────────────────────────────────────────────────────────────

 app/services/pagamento.py                                   1–29 of 29
  ⋯   @@ -1,17 +1,18 @@
  1 − from decimal import Decimal
  1 + from decimal import Decimal, ROUND_HALF_UP
  2
  3   class Pagamento:
 ↑↓ navigate   ←→ folders   space stage/fold   c commit   x whole file   2 branches   q quit
```

The panel has three screens:

- **Changes** (`1`): the tree and the diff above. Review, stage, and commit.
- **Branches** (`2`): your local branches, plus remote ones you don't have yet.
  Fetch, check out, pull, push, merge, and delete.
- **History** (Enter on a branch): that branch's commits.

In a wide pane (full screen or zoomed) the Changes screen puts the tree on the
left and the diff on the right.

## Features

- **Tree of changed files only.** Folders with a single subfolder collapse into
  one row (`src/components/ui/`), each file shows its status (`A`, `M`, `D`)
  and `+N −N`, and folders add up their files.
- **Mark files as reviewed.** Press space on a file to `git add` it, and again
  to unstage it. Each file shows `✓` (staged), `◐` (staged, then changed
  again), or `○` (not staged); reviewed files fade, folders get a `✓` when
  everything in them is staged, and the header counts your progress.
- **Discard what you don't want.** `d` on a file or folder throws its changes
  away, after you confirm with `y`.
- **Commit from the panel.** Press `c`, type the message, and Enter commits
  what's staged. It runs the real `git`, so your hooks and commit signing work
  as usual.
- **Branches.** Press `2` for a list of your local branches with how far each
  one is ahead/behind its upstream, followed by remote branches you haven't
  checked out yet. `f` fetches, space checks one out, `p` pulls it,
  Shift+P pushes it, `m` merges it into the branch you're on, `d` deletes it,
  and Enter shows its commit history.
- **A diff that reads well.** Line numbers, full-width green/red backgrounds,
  syntax colors, and the exact words that changed highlighted inside each line.
  Long lines wrap to the pane width, keeping the code's indentation.
- **Follows your Rex setup.** Everything is drawn as terminal text, so it uses
  your Rex font (ligatures included). Colors come from your Rex theme: the
  panel asks the terminal for its foreground, background, and palette, and
  syntax colors use the theme's own ANSI colors. Switch themes and the panel
  follows.
- **Live.** It watches the repository and updates as soon as a file changes,
  including after `git add` or a commit.
- **Fast.** A single native binary. Only the visible lines are drawn, the diff
  is computed for the selected file only, and git is read in-process
  (libgit2) on a background thread, so the UI never blocks.
- **Keyboard and mouse.** Arrow keys, click (files, checkboxes, folders, and the
  tabs in the header), scroll, and a draggable divider between the tree and the
  diff.

## Requirements

- [Rex](https://superlogical.com/rex) on macOS. (Linux should work but is
  untested.)
- `git` on your `PATH`. Reading the repository doesn't need it, but commit,
  checkout, pull, push, and merge run the real `git` command.
- A Rust toolchain to build it: `brew install rust`, or
  [rustup](https://rustup.rs).

## Install

```sh
git clone https://github.com/victorvf/rexdiff
cd rexdiff
make install
```

`make install` builds a release binary and links `~/.local/bin/rexdiff` to it.
Make sure `~/.local/bin` is on your `PATH`. Since it's a link, running
`make build` again later updates the command in place.

### Keyboard shortcut (recommended)

Add a Rex action and bind it to a key. Copy
[`extras/rex/init.lua`](extras/rex/init.lua) into your Rex config at
`~/.config/rex/init.lua` (or append it if you already have one), then reload:

```sh
rex config reload
```

Now **Option + Shift + ?** opens rexdiff next to the focused pane, for the
folder that pane is in, and focuses it. Press it again (from either pane) to
close it. The action also shows up in the command palette as
"rexdiff: Toggle Changes".

Rex matches keys without Shift applied, so `?` is written as `alt+shift+/`.
Change the key in the last line of the file if you prefer another one.

## Usage

From any Rex terminal inside a git repository:

```sh
rexdiff                 # open the panel next to this pane (45% of the width)
rexdiff hide            # close it
rexdiff toggle          # open or close it
rexdiff --ratio 35      # narrower panel
rexdiff --below         # open below instead of to the right
rexdiff --focus         # focus the panel when it opens
rexdiff view            # run the panel in the current terminal
rexdiff help
```

Each terminal pane gets its own panel. Running `rexdiff` again while it's open
does nothing.

### Changes screen

| Key | Action |
| --- | --- |
| `↑` `↓` / `j` `k` | Previous / next row (folders and files) |
| `←` `→` / `h` `l` | Collapse/go to parent or expand/enter a folder |
| `space` | Stage/unstage a changed file; fold a folder. Unchanged/ignored files are read-only |
| `Enter` | Focus the selected file |
| `t` | Toggle Changed files / All files, including hidden and ignored project files |
| `Ctrl+p` | Fuzzy project-file picker (excludes Git-ignored files); type a path abbreviation, arrows or Ctrl+n/p to choose, Enter to open, Esc to cancel |
| `:e PATH` in the file | Open a readable project file; paths outside the project are rejected |
| `\` | Hide/show the left sidebar |
| `Tab` / `Shift-Tab` | Switch focus between the tree and file |
| `H` (Shift+h) | Browse commits in the checked-out branch; `Enter` opens the selected commit's diff |
| `D` (Shift+d), in Changes or History | View all committed branch changes from the merge base with the default branch |
| `B` (Shift+b), in Changes or History | Enter a different comparison base, such as `origin/main` or `develop` |
| `W` (Shift+w) | Return to staged, unstaged, and untracked working-tree changes |
| `↑` `↓` `←` `→` / `h` `j` `k` `l` in the file | Move the file cursor |
| `v` / `V` (Shift+v) in the file | Select text / whole lines; move to extend; `Esc` clears |
| `a` | Annotate the selection, or the whole file when nothing is selected. Arrow keys move the text cursor; `Shift+Enter` inserts a newline, `Enter` saves, `Esc` cancels |
| `s` | Paste queued annotations into an agent's input in this tab; `↑↓` and `Enter` pick when multiple agents are available |
| `Ctrl+f/b`, `Ctrl+d/u`, `PgDn` / `PgUp` | Full/half-page movement |
| `J` / `K` | Scroll the diff one line |
| `gg` / `G`, `42G`, `:42` | First/last/source line; a hidden line automatically opens whole-file mode |
| `0` / `^` / `$`, `w` / `b` / `e` | Line and word motions; counts such as `10j` repeat motions |
| `/` / `?`, `n` / `N` | Literal smart-case content search forward/backward; repeat/reverse with wraparound |
| `L` (Shift+l) | List pending annotations; j/k previews, Enter edits, d deletes, Esc/q closes |
| `c` | Commit what's staged (see below) |
| `d` | Discard the changes of the file or folder under the cursor (see below) |
| `x` | Toggle between changed hunks only and the whole file |
| `+` / `-` | Make the tree bigger / smaller |
| `1` / `2` in the tree | Changes / Branches screen (or click the tabs in the header). In the file, digits form motion counts |
| `r` | Reload the repository |
| `q` | Quit (the pane closes) |

Annotations are queued for the panel session. Add as many as needed, then press
`s` to paste them together into an agent's input without submitting. Only agents
in the panel's tab are offered. Failed delivery keeps the notes for retry; closing
the panel discards unsent notes. Selected diff text includes old/new line numbers,
so deleted lines remain identifiable. Use `x` to select beyond the changed hunks.
Gutter dots mark annotated lines. Notes keep their original snippets; after a
file changes, the list marks them stale rather than moving them to an inaccurate
location. Preview is limited to 8 MiB and 50,000 rows; binary and oversized files
can still receive whole-file annotations. Files are never edited by rexdiff.

The fuzzy picker ranks path abbreviations (for example `sapr` for `src/app.rs`),
highlights matched characters, and scans incrementally in the background. Its
search is separate from literal `/` and `?` searches inside a file.

With the mouse: click a file to show it, click its checkbox to stage or
unstage it, click a folder to open or close it, scroll over the tree or the
diff, and drag the divider to resize.

The diff always compares your working tree with `HEAD`, so staging a file
doesn't change what you see, only its checkbox.

### Commit

Stage the files you want with space, then press `c` to open the commit box in
the footer:

| Key | Action |
| --- | --- |
| `Enter` | Commit |
| `Esc` | Cancel |
| `Ctrl+W` / `Option+Backspace` | Delete the last word |
| `Ctrl+U` | Clear the message |

The footer shows the result, such as `✓ Committed 1a2b3c4`, or what went
wrong. Nothing is pushed.

`git` runs detached from the panel's terminal, so it can't stop to ask for
anything: a hook that needs input makes the commit fail with a message instead
of hanging.

### Discard

`d` on a file or folder asks, in red in the footer, before throwing its changes
away. Press `y` to go ahead; any other key cancels.

- A file the last commit has (modified, deleted, partly staged) goes back to
  that version, both in the index and on disk
  (`git restore --source=HEAD --staged --worktree`).
- A new file is taken out of the index and **deleted**, along with any folder
  that ends up empty. The prompt says how many new files will go, because
  there's no getting them back.
- On a folder, it does this to every changed file inside it.

### Branches

Press `2` (or click **Branches** in the header) to see your local branches,
most recently committed first:

```
 ● feature/frete   ✓ origin/feature/frete   frete: ajuste          2 min ago
   main            ↑5 ↓1 origin/main        pedido: só commit      1 hour ago
   old-idea        not pushed               try another layout     3 weeks ago
```

`●` marks the branch you're on and `◆` a branch that's checked out in another
worktree (`git worktree`), with that worktree's folder in parentheses:
`aux/worktree (sistema_2)`. Git only lets
a branch be checked out in one place, so space and `d` on a `◆` branch tell you
which worktree has it instead of trying. Next to each one: `✓` when it matches its
upstream, `↑N` commits to push, `↓N` commits to pull, or `not pushed` when it
has no upstream yet. The ahead/behind counts use what git last fetched; press
`f` to fetch.

Below your local branches come the remote ones that no local branch tracks
yet, dimmed and marked `remote` (`origin/pr/new-screen`). Space on one creates
a local branch that tracks it and switches to it, which is how you check out
someone's pull request: `f`, find the branch, space. Enter shows its history
and `m` merges it into your branch; pull, push, and delete ask you to check it
out first.

| Key | Action |
| --- | --- |
| `↑` `↓` / `j` `k` | Move |
| `space` | Check out the branch (`git switch`). On a remote branch, create a local one that tracks it (`git switch --track`) |
| `Enter` | Show the branch's commit history |
| `p` | Pull the branch |
| `P` (Shift+P) | Push the branch |
| `m` | Merge the branch into the one you're on (updating it from its upstream first) |
| `d` | Delete the local branch, after `y` |
| `D` (Shift+D) | Delete it even if it has unmerged commits, after `y` |
| `f` | Fetch from all remotes, dropping remote branches deleted there (`git fetch --all --prune`) |
| `Esc` / `1` | Back to Changes |

Push sends the branch to its upstream, or to `origin` with `--set-upstream`
when it doesn't have one yet (`git push -u origin <branch>`). It works on any
branch in the list, not just the current one. If the remote has commits you
don't have, the push is rejected and the footer tells you to pull first.
Checkout refuses when your uncommitted changes would be overwritten.

Pull on the branch you're on runs `git pull`, so it merges or rebases as your
git config says. On any other branch it can only fast-forward (merging needs a
checkout); if that branch has diverged, the footer asks you to check it out
first. Problems show up in the footer in plain words: merge conflicts (fix them
in the Changes screen, then commit), diverged branches with no `pull.rebase`
setting, or local changes in the way.

`m` brings another branch into the one you're on, the usual "update my feature
branch with `main`": on `feature/x`, press `m` on `main`. It first
fast-forwards `main` from `origin/main` (like `p`), then runs `git merge main`.
If your local `main` has diverged from `origin/main` it stops before merging and
tells you, so you never merge a stale `main` by accident. Conflicts are reported
the same way as with pull.

`d` deletes the local branch after you confirm with `y` (`git branch -d`). Git
refuses when the branch has commits that aren't on its upstream or on the
branch you're on, and the footer says so; `D` (Shift+D) deletes it anyway
(`git branch -D`), with a warning that those commits will be lost. You can't
delete the branch you're on, and the remote branch is left alone.

### History

Press Enter on a branch to see its commits, newest first (up to 1,000):

```
 ‹ feature/frete  ·  13 commits  ·  ↑1 not pushed
 ↑ 9005e63 (feature/frete) local: muda linha                Demo     just now
   31cd565 remoto: novo arquivo                             Other  5 min ago
   b829a13 (main) moeda: ajuste                             Demo  1 hour ago
```

Each row has the short hash, the branches and tags pointing at that commit,
the message, the author (when the pane is wide enough), and when it was made.
`↑` marks commits that aren't on the branch's upstream yet, the ones a push
would send. A local branch and its remote counterpart on the same commit show
as just the local name. Merge commits are dimmed.

Move with `↑` `↓` (`f`/`b` for a page, `g`/`G` for the ends) and go back to the
branches with `Esc`.

Press `H` from Changes to open the checked-out branch's history directly.
`Enter` opens the selected commit's changes against its first parent (or an
empty tree for the first commit). `Esc` in a commit diff returns to the list.
In a branch diff, `Esc` returns to working changes instead; `D` is omitted
from that view's footer because the branch diff is already open.

`D` from Changes or History shows the checked-out branch's combined changes,
like a PR/MR diff: merge base of the base branch and HEAD through HEAD. The
default base is the remote default branch when available, falling back to
`origin/main`, `origin/master`, `main`, or `master`. Press `B` to enter another
base; an empty base uses automatic selection again. These views use local Git
objects, so fetch through Branches (`2`, then `f`) to update remote references.

Review views never check out a commit or modify files or the index. Staging,
discarding, and committing are disabled there. Search, selection, annotations,
and the whole-file toggle still work; annotations include the compared commit
IDs. `W` returns to working changes; the project file picker and all-files tree
are available there. The chosen base lasts for this rexdiff session.

### Layout

- Below 160 columns, the tree sits above the diff. At 160 columns and wider,
  it moves to the left. Drag the divider or use `+`/`-` to resize the tree;
  `\` hides/shows it. `Tab` switches focus, marked by a thick blue border.
- All-files mode loads directory contents on expansion, leaving unchanged
  folders collapsed initially. Git administrative entries are excluded and
  symlink directories are not traversed.
- `x` caches both active-file representations, shows plain text before syntax
  highlighting completes, and keeps the previous view visible during loading.

## How it works

1. `rexdiff` (or the shortcut) runs `rex split` to create a pane next to the
   current one, running `rexdiff view` in that folder. It records which pane it
   opened in `~/.local/state/rexdiff/`, so `hide` and `toggle` can close it.
2. `rexdiff view` is the panel itself, a terminal UI built with
   [ratatui](https://ratatui.rs):
   - a worker thread reads the repository with [git2](https://crates.io/crates/git2)
     (libgit2): the list of changed files compared with `HEAD` (staged,
     unstaged, and untracked) and their index state, the diff of the selected
     file, the local branches with their ahead/behind counts, and a branch's
     history;
   - the same thread runs what you ask for. Stage and unstage go through
     libgit2; commit, checkout, pull, push, and merge run the `git` command, so
     hooks, commit signing, credential helpers, and your `pull.rebase` setting
     all apply. Those commands run detached from the panel's terminal, so they
     can never take over the screen to ask for input;
   - a file watcher (FSEvents on macOS, via [notify](https://crates.io/crates/notify))
     triggers a reload when something relevant changes, ignoring files covered
     by `.gitignore`;
   - syntax colors come from [syntect](https://crates.io/crates/syntect),
     mapped to the terminal's ANSI palette instead of a fixed theme, and the
     changed words come from a word-level diff ([similar](https://crates.io/crates/similar));
   - theme colors are read with terminal queries (OSC 10, 11, and 4), and the
     green/red backgrounds are mixed from them.
3. `q` ends the program and Rex closes the pane.

Reading the repository never takes git's index lock, so the panel can't get in
the way of your own `git` commands. The only writes are the ones you ask for:

| Where | Key | What runs |
| --- | --- | --- |
| Changes | `space` on a file | `git add` / `git restore --staged` |
| Changes | `c`, then Enter | `git commit -m <message>` |
| Changes | `d`, then `y` | `git restore --source=HEAD --staged --worktree` for files the last commit has; new files are unstaged and deleted |
| Branches | `space` | `git switch <branch>`, or `git switch --track <remote>/<branch>` on a remote branch |
| Branches | `f` | `git fetch --all --prune` |
| Branches | `p` | `git pull` on the current branch; `git fetch <remote> <branch>:<branch>` (fast-forward only) on any other |
| Branches | `P` | `git push`, or `git push -u origin <branch>` the first time |
| Branches | `m` | Fast-forward the branch from its upstream, then `git merge <branch>` |
| Branches | `d` / `D`, then `y` | `git branch -d <branch>` / `git branch -D <branch>` |

If the panel ever crashes, the error is written to
`~/.local/state/rexdiff/panic.log`.

### Code map

| File | What it does |
| --- | --- |
| `src/main.rs` | Command line: `show`, `hide`, `toggle`, `view`, and the Rex integration |
| `src/app.rs` | The panel: the Changes, Branches, and History screens, keyboard/mouse, layout, and drawing |
| `src/git.rs` | Background worker: changed files, diffs, word-level highlights, branches, history, and the git commands (stage, commit, checkout, pull, push, merge) |
| `src/highlight.rs` | Syntax highlighting with the terminal palette |
| `src/tree.rs` | Builds the folder tree from the changed files |
| `src/theme.rs` | Reads the terminal colors and derives the panel palette |
| `src/wrap.rs` | Wraps diff lines to the pane width |

## Limitations

- Syntax colors use syntect's built-in grammars: TypeScript uses JavaScript's,
  Vue and Svelte use HTML's.
- Renames show up as a deleted file plus an added one.
- Very large diffs are cut at 50,000 lines, and syntax colors are skipped above
  20,000.
- The ↑/↓ counts and the remote branches on the Branches screen are as of the
  last fetch; press `f` to update them.
- Pull requests from forks aren't on your remotes, so they don't show up as
  remote branches; use `gh pr checkout` for those.
- Commit messages are a single line.
- History shows up to 1,000 commits per branch.
- Commands can't answer prompts: an SSH key without an agent, or a hook that
  asks for input, makes them fail with a message instead.

## License

[MIT](LICENSE): use it, copy it, change it, and share it, as long as the
copyright and license notice stay with the code.
