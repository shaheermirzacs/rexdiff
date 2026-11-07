# rexdiff

Your git changes in a [Rex](https://superlogical.com/rex) pane, right next to
your terminal: a tree of the files that changed on top, and a colored diff of
the selected file below.

```
 Changes  6 files   ⎇ main   ✓ 2/6 staged                       +31 −13
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
 ↑↓ navigate   ←→ folders   space stage/fold   c commit   x whole file   q quit
```

In a wide pane (full screen or zoomed) the tree moves to the left and the diff
to the right.

## Features

- **Tree of changed files only.** Folders with a single subfolder collapse into
  one row (`src/components/ui/`), each file shows its status (`A`, `M`, `D`)
  and `+N −N`, and folders add up their files.
- **Mark files as reviewed.** Press space on a file to `git add` it, and again
  to unstage it. Each file shows `✓` (staged), `◐` (staged, then changed
  again), or `○` (not staged); reviewed files fade, folders get a `✓` when
  everything in them is staged, and the header counts your progress.
- **Commit from the panel.** Press `c`, type the message, and Enter commits
  what's staged. It runs the real `git`, so your hooks and commit signing work
  as usual.
- **Branches.** Press `2` for a list of your local branches with how far each
  one is ahead/behind its upstream. Space checks one out, `p` pulls it,
  Shift+P pushes it, `m` merges it into the branch you're on, and Enter shows
  its commit history.
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
- **Keyboard and mouse.** Arrow keys, click, scroll, and a draggable divider
  between the tree and the diff.

## Requirements

- [Rex](https://superlogical.com/rex) on macOS. (Linux should work but is
  untested.)
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

### Keys

| Key | Action |
| --- | --- |
| `↑` `↓` / `j` `k` | Previous / next row (folders and files) |
| `←` `→` / `h` `l` | Previous / next folder. An open folder lands on its first file |
| `space` / `Enter` | On a file: stage it, or unstage it if it's fully staged. On a folder: open or close it |
| `Tab` / `Shift-Tab` | Next / previous file, wrapping around |
| `f` / `b`, `PgDn` / `PgUp` | Scroll the diff one page |
| `J` / `K` | Scroll the diff one line |
| `g` / `G` | Top / bottom of the diff |
| `c` | Commit what's staged (see below) |
| `x` | Toggle between changed hunks only and the whole file |
| `+` / `-` | Make the tree bigger / smaller |
| `1` / `2` | Changes / Branches screen (or click the tabs in the header) |
| `r` | Reload the repository |
| `q` | Quit (the pane closes) |

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

### Branches

Press `2` (or click **Branches** in the header) to see your local branches,
most recently committed first:

```
 ● feature/frete   ✓ origin/feature/frete   frete: ajuste          2 min ago
   main            ↑5 ↓1 origin/main        pedido: só commit      1 hour ago
   old-idea        not pushed               try another layout     3 weeks ago
```

`●` marks the branch you're on. Next to each one: `✓` when it matches its
upstream, `↑N` commits to push, `↓N` commits to pull, or `not pushed` when it
has no upstream yet. The ahead/behind counts use what git last fetched.

| Key | Action |
| --- | --- |
| `↑` `↓` / `j` `k` | Move |
| `space` | Check out the branch (`git switch`) |
| `Enter` | Show the branch's commit history |
| `p` | Pull the branch |
| `P` (Shift+P) | Push the branch |
| `m` | Merge the branch into the one you're on (updating it from its upstream first) |
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

### Layout

- **Stacked** (default): tree on top, diff below. The tree takes up to about a
  third of the height.
- **Side by side**: when the panel is at least 160 columns wide (for example,
  zoomed or full screen), the tree goes on the left and the diff on the right.
  It switches back when the panel gets narrower.

## How it works

1. `rexdiff` (or the shortcut) runs `rex split` to create a pane next to the
   current one, running `rexdiff view` in that folder. It records which pane it
   opened in `~/.local/state/rexdiff/`, so `hide` and `toggle` can close it.
2. `rexdiff view` is the panel itself, a terminal UI built with
   [ratatui](https://ratatui.rs):
   - a worker thread reads the repository with [git2](https://crates.io/crates/git2)
     (libgit2): the list of changed files compared with `HEAD` (staged,
     unstaged, and untracked), and the diff of the selected file;
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
stage/unstage with space (the same as `git add` and `git restore --staged`),
commits with `c` (`git commit`), and checkout/pull/push on the Branches screen
(`git switch`, `git pull`, and `git push`).

If the panel ever crashes, the error is written to
`~/.local/state/rexdiff/panic.log`.

### Code map

| File | What it does |
| --- | --- |
| `src/main.rs` | Command line: `show`, `hide`, `toggle`, `view`, and the Rex integration |
| `src/app.rs` | The panel: state, keyboard/mouse, layout, and drawing |
| `src/git.rs` | Background worker: changed files, diffs, word-level highlights |
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
