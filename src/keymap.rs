//! The one table of key bindings. Dispatch, the help overlay, and the
//! command line all read it, so listed keys cannot drift from bound keys.
//! The Changes screen has a single set: a key does the same thing whichever
//! pane has focus, acting on the focused list where that matters.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::reader::Motion;

/// Everything a key or command can do. Handlers pick the pane from the focus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Quit,
    Command,
    Help,
    Changes,
    Branches,
    WorkingTree,
    Annotate,
    AnnotateFile,
    Commit,
    Send,
    Notes,
    Context,
    Layout,
    Picker,
    PrevHunk,
    NextHunk,
    PrevFile,
    NextFile,
    Top,
    Bottom,
    Up,
    Down,
    PageUp,
    PageDown,
    HalfUp,
    HalfDown,
    FocusNext,
    FocusPrev,
    Leader,
    Sidebar,
    OldSide,
    NewSide,
    Back,
    Reload,
    Left,
    Right,
    StageOrFold,
    Activate,
    Discard,
    History,
    BranchDiff,
    Base,
    AllFiles,
    ScrollUp,
    ScrollDown,
    Grow,
    Shrink,
    Move(Motion),
    Search,
    SearchNext,
    SearchPrev,
    VisualChar,
    VisualLine,
    DeleteNote,
    Checkout,
    OpenHistory,
    Pull,
    Push,
    Merge,
    DeleteBranch,
    ForceDeleteBranch,
    Fetch,
    OpenCommit,
}

impl Action {
    /// Held keys repeat movement only, never application actions.
    pub fn repeats(self) -> bool {
        use Action::*;
        matches!(
            self,
            PrevHunk
                | NextHunk
                | PrevFile
                | NextFile
                | Up
                | Down
                | PageUp
                | PageDown
                | HalfUp
                | HalfDown
                | Left
                | Right
                | ScrollUp
                | ScrollDown
                | Move(_)
                | SearchNext
                | SearchPrev
        )
    }
}

/// Where a binding applies: every screen, or one of them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Global,
    Changes,
    Branches,
    History,
}

impl Scope {
    /// The tables consulted for a screen, most specific first.
    fn layers(self) -> [Scope; 2] {
        [self, Scope::Global]
    }

    pub fn title(self) -> &'static str {
        match self {
            Scope::Branches => "Branches",
            Scope::History => "History",
            Scope::Global | Scope::Changes => "Changes",
        }
    }
}

/// A key press with exact modifiers, or the second key of `Space x`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Press(KeyCode, KeyModifiers),
    Leader(char),
}

impl Key {
    /// Shift is implied by uppercase letters and Shift-Tab; other modifiers
    /// must match exactly, so Ctrl+p never pulls and Ctrl+d never deletes.
    pub fn from_event(k: &KeyEvent) -> Self {
        let mut modifiers = k.modifiers;
        if matches!(k.code, KeyCode::Char(_) | KeyCode::BackTab) {
            modifiers.remove(KeyModifiers::SHIFT);
        }
        Key::Press(k.code, modifiers)
    }

    pub fn label(self) -> String {
        let code = |code: KeyCode| match code {
            KeyCode::Char(' ') => "Space".into(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Enter => "Enter".into(),
            KeyCode::Esc => "Esc".into(),
            KeyCode::Tab => "Tab".into(),
            KeyCode::BackTab => "Shift+Tab".into(),
            KeyCode::Backspace => "Backspace".into(),
            KeyCode::Delete => "Delete".into(),
            KeyCode::Up => "↑".into(),
            KeyCode::Down => "↓".into(),
            KeyCode::Left => "←".into(),
            KeyCode::Right => "→".into(),
            KeyCode::Home => "Home".into(),
            KeyCode::End => "End".into(),
            KeyCode::PageUp => "PgUp".into(),
            KeyCode::PageDown => "PgDn".into(),
            other => format!("{other:?}"),
        };
        match self {
            Key::Press(c, m) if m.contains(KeyModifiers::CONTROL) => format!("Ctrl+{}", code(c)),
            Key::Press(c, _) => code(c),
            Key::Leader(c) => format!("Space {c}"),
        }
    }
}

pub struct Binding {
    pub scope: Scope,
    pub key: Key,
    pub action: Action,
    pub help: &'static str,
}

const fn b(scope: Scope, key: Key, action: Action, help: &'static str) -> Binding {
    Binding {
        scope,
        key,
        action,
        help,
    }
}
const fn ch(c: char) -> Key {
    Key::Press(KeyCode::Char(c), KeyModifiers::NONE)
}
const fn ctrl(c: char) -> Key {
    Key::Press(KeyCode::Char(c), KeyModifiers::CONTROL)
}
const fn code(c: KeyCode) -> Key {
    Key::Press(c, KeyModifiers::NONE)
}

use Action as A;
use Scope::{Branches, Changes, Global, History};

// A `static`, not a `const`: `help` compares bindings by address.
pub static BINDINGS: &[Binding] = &[
    b(Global, ch('q'), A::Quit, "Quit"),
    b(Global, ctrl('c'), A::Quit, "Quit"),
    b(Global, ch('?'), A::Help, "Help: every key on this screen"),
    b(Global, ch(':'), A::Command, "Command line (Tab completes)"),
    b(Global, ch('1'), A::Changes, "Changes screen"),
    b(Global, ch('2'), A::Branches, "Branches screen"),
    b(Global, ch('W'), A::WorkingTree, "Working-tree changes"),
    // Changes: one set for the tree, the reader, and the notes.
    b(Changes, ch('j'), A::Down, "Down in the focused pane"),
    b(
        Changes,
        code(KeyCode::Down),
        A::Down,
        "Down in the focused pane",
    ),
    b(Changes, ch('k'), A::Up, "Up in the focused pane"),
    b(Changes, code(KeyCode::Up), A::Up, "Up in the focused pane"),
    b(
        Changes,
        ch('h'),
        A::Left,
        "Left (tree: collapse / parent folder)",
    ),
    b(
        Changes,
        code(KeyCode::Left),
        A::Left,
        "Left (tree: collapse / parent folder)",
    ),
    b(
        Changes,
        ch('l'),
        A::Right,
        "Right (tree: expand / enter folder)",
    ),
    b(
        Changes,
        code(KeyCode::Right),
        A::Right,
        "Right (tree: expand / enter folder)",
    ),
    b(Changes, ch('g'), A::Top, "Top of the focused pane"),
    b(
        Changes,
        code(KeyCode::Home),
        A::Top,
        "Top of the focused pane",
    ),
    b(Changes, ch('G'), A::Bottom, "Bottom of the focused pane"),
    b(
        Changes,
        code(KeyCode::End),
        A::Bottom,
        "Bottom of the focused pane",
    ),
    b(Changes, ctrl('d'), A::HalfDown, "Half page down"),
    b(Changes, ctrl('u'), A::HalfUp, "Half page up"),
    b(Changes, ctrl('f'), A::PageDown, "Page down"),
    b(Changes, code(KeyCode::PageDown), A::PageDown, "Page down"),
    b(Changes, ctrl('b'), A::PageUp, "Page up"),
    b(Changes, code(KeyCode::PageUp), A::PageUp, "Page up"),
    b(
        Changes,
        code(KeyCode::Enter),
        A::Activate,
        "Open file / fold folder / edit note",
    ),
    b(
        Changes,
        code(KeyCode::Tab),
        A::FocusNext,
        "Next pane (opens the file from the tree)",
    ),
    b(
        Changes,
        code(KeyCode::BackTab),
        A::FocusPrev,
        "Previous pane",
    ),
    b(
        Changes,
        ch('z'),
        A::StageOrFold,
        "Stage file / fold folder (tree); fold hunk (file)",
    ),
    b(
        Changes,
        ch('a'),
        A::Annotate,
        "Annotate the file's line or selection",
    ),
    b(Changes, ch('A'), A::AnnotateFile, "Annotate the whole file"),
    b(
        Changes,
        ch('L'),
        A::Notes,
        "Show / focus notes; again hides",
    ),
    b(Changes, ch('s'), A::Send, "Send notes to an agent"),
    b(
        Changes,
        code(KeyCode::Delete),
        A::DeleteNote,
        "Delete the selected note (notes)",
    ),
    b(
        Changes,
        code(KeyCode::Backspace),
        A::DeleteNote,
        "Delete the selected note (notes)",
    ),
    b(Changes, ch('['), A::PrevHunk, "Previous hunk"),
    b(Changes, ch(']'), A::NextHunk, "Next hunk"),
    b(Changes, ch('{'), A::PrevFile, "Previous file"),
    b(Changes, ch('}'), A::NextFile, "Next file"),
    b(
        Changes,
        ch('/'),
        A::Search,
        "Search the file (regex, smart case)",
    ),
    b(Changes, ch('n'), A::SearchNext, "Next match"),
    b(Changes, ch('N'), A::SearchPrev, "Previous match"),
    b(Changes, ch('w'), A::Move(Motion::WordNext), "Next word"),
    b(Changes, ch('b'), A::Move(Motion::WordBack), "Previous word"),
    b(Changes, ch('e'), A::Move(Motion::WordEnd), "End of word"),
    b(Changes, ch('0'), A::Move(Motion::LineStart), "Line start"),
    b(
        Changes,
        ch('^'),
        A::Move(Motion::FirstNonBlank),
        "First non-blank",
    ),
    b(Changes, ch('$'), A::Move(Motion::LineEnd), "Line end"),
    b(Changes, ch('v'), A::VisualChar, "Select characters"),
    b(Changes, ch('V'), A::VisualLine, "Select lines"),
    b(Changes, ch('J'), A::ScrollDown, "Scroll the file down"),
    b(Changes, ctrl('e'), A::ScrollDown, "Scroll the file down"),
    b(Changes, ch('K'), A::ScrollUp, "Scroll the file up"),
    b(Changes, ctrl('y'), A::ScrollUp, "Scroll the file up"),
    b(Changes, ch('x'), A::Context, "Whole-file context"),
    b(Changes, ch('S'), A::Layout, "Split / stacked layout"),
    b(Changes, ch(' '), A::Leader, "Leader prefix"),
    b(
        Changes,
        Key::Leader('e'),
        A::Sidebar,
        "Hide / show the file tree",
    ),
    b(
        Changes,
        Key::Leader('h'),
        A::OldSide,
        "Old side of a split diff",
    ),
    b(
        Changes,
        Key::Leader('l'),
        A::NewSide,
        "New side of a split diff",
    ),
    b(Changes, ch('c'), A::Commit, "Commit staged changes"),
    b(
        Changes,
        ch('d'),
        A::Discard,
        "Discard the file's changes (y confirms)",
    ),
    b(
        Changes,
        ch('D'),
        A::BranchDiff,
        "Branch diff from the merge base",
    ),
    b(Changes, ch('B'), A::Base, "Comparison base"),
    b(
        Changes,
        ch('H'),
        A::History,
        "Commits of the checked-out branch",
    ),
    b(Changes, ch('t'), A::AllFiles, "Changed / all project files"),
    b(Changes, ctrl('p'), A::Picker, "Project file picker"),
    b(Changes, ch('r'), A::Reload, "Reload the repository"),
    b(Changes, ch('+'), A::Grow, "Bigger tree"),
    b(Changes, ch('='), A::Grow, "Bigger tree"),
    b(Changes, ch('-'), A::Shrink, "Smaller tree"),
    b(
        Changes,
        code(KeyCode::Esc),
        A::Back,
        "Clear selection / search, close notes, back",
    ),
    // Branches.
    b(Branches, ch('j'), A::Down, "Down"),
    b(Branches, code(KeyCode::Down), A::Down, "Down"),
    b(Branches, ch('k'), A::Up, "Up"),
    b(Branches, code(KeyCode::Up), A::Up, "Up"),
    b(Branches, ch('g'), A::Top, "First branch"),
    b(Branches, code(KeyCode::Home), A::Top, "First branch"),
    b(Branches, ch('G'), A::Bottom, "Last branch"),
    b(Branches, code(KeyCode::End), A::Bottom, "Last branch"),
    b(
        Branches,
        ch(' '),
        A::Checkout,
        "Check out (remote: tracking branch)",
    ),
    b(
        Branches,
        code(KeyCode::Enter),
        A::OpenHistory,
        "Commit history",
    ),
    b(Branches, ch('p'), A::Pull, "Pull"),
    b(Branches, ch('P'), A::Push, "Push"),
    b(Branches, ch('m'), A::Merge, "Merge into the current branch"),
    b(
        Branches,
        ch('d'),
        A::DeleteBranch,
        "Delete (y/Enter confirms)",
    ),
    b(
        Branches,
        ch('D'),
        A::ForceDeleteBranch,
        "Force-delete (y/Enter confirms)",
    ),
    b(Branches, ch('f'), A::Fetch, "Fetch all remotes"),
    b(Branches, ch('r'), A::Reload, "Reload branches"),
    b(Branches, code(KeyCode::Esc), A::Back, "Back to Changes"),
    // History.
    b(History, ch('j'), A::Down, "Down"),
    b(History, code(KeyCode::Down), A::Down, "Down"),
    b(History, ch('k'), A::Up, "Up"),
    b(History, code(KeyCode::Up), A::Up, "Up"),
    b(History, ch('f'), A::PageDown, "Page down"),
    b(History, code(KeyCode::PageDown), A::PageDown, "Page down"),
    b(History, ch('b'), A::PageUp, "Page up"),
    b(History, code(KeyCode::PageUp), A::PageUp, "Page up"),
    b(History, ch('g'), A::Top, "Newest commit"),
    b(History, code(KeyCode::Home), A::Top, "Newest commit"),
    b(History, ch('G'), A::Bottom, "Oldest commit"),
    b(History, code(KeyCode::End), A::Bottom, "Oldest commit"),
    b(History, code(KeyCode::Enter), A::OpenCommit, "Commit diff"),
    b(
        History,
        ch('D'),
        A::BranchDiff,
        "Branch diff from the merge base",
    ),
    b(History, ch('B'), A::Base, "Comparison base"),
    b(History, code(KeyCode::Esc), A::Back, "Back to Branches"),
    b(
        History,
        code(KeyCode::Backspace),
        A::Back,
        "Back to Branches",
    ),
    b(History, ch('h'), A::Back, "Back to Branches"),
    b(History, code(KeyCode::Left), A::Back, "Back to Branches"),
];

/// The binding a key resolves to in `scope`.
pub fn lookup(scope: Scope, key: Key) -> Option<&'static Binding> {
    scope
        .layers()
        .into_iter()
        .find_map(|layer| BINDINGS.iter().find(|b| b.scope == layer && b.key == key))
}

/// The keys of `scope` with what they do, grouped by action, without keys
/// a more specific table takes over.
pub fn help(scope: Scope) -> Vec<(Vec<Key>, &'static str, Action)> {
    let mut out: Vec<(Vec<Key>, &'static str, Action)> = Vec::new();
    for layer in scope.layers() {
        for binding in BINDINGS.iter().filter(|b| b.scope == layer) {
            if lookup(scope, binding.key).is_none_or(|winner| !std::ptr::eq(winner, binding)) {
                continue;
            }
            match out
                .iter_mut()
                .find(|(_, help, action)| *help == binding.help && *action == binding.action)
            {
                Some((keys, _, _)) => keys.push(binding.key),
                None => out.push((vec![binding.key], binding.help, binding.action)),
            }
        }
    }
    out
}

/// What a command-line entry runs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Command {
    Run(Action),
    Source(u32),
    Edit(String),
    Base(String),
}

/// Command names for completion, with the key action each one runs.
pub const COMMANDS: &[(&str, Option<Action>)] = &[
    ("Source", None),
    ("base", None),
    ("branches", Some(A::Branches)),
    ("changes", Some(A::Changes)),
    ("commit", Some(A::Commit)),
    ("context", Some(A::Context)),
    ("diff", Some(A::Layout)),
    ("e", None),
    ("edit", None),
    ("h", Some(A::Help)),
    ("help", Some(A::Help)),
    ("notes", Some(A::Notes)),
    ("q", Some(A::Quit)),
    ("quit", Some(A::Quit)),
    ("reload", Some(A::Reload)),
];

/// Parses what follows `:`.
pub fn parse(input: &str) -> Result<Command, String> {
    let input = input.trim();
    let (name, argument) = input
        .split_once(char::is_whitespace)
        .map_or((input, ""), |(n, a)| (n, a.trim()));
    if let Ok(line) = name.parse::<u32>() {
        return Ok(Command::Source(line));
    }
    match name {
        "Source" => argument
            .parse()
            .map(Command::Source)
            .map_err(|_| "Usage: :Source N".into()),
        "e" | "edit" if !argument.is_empty() => Ok(Command::Edit(argument.into())),
        "e" | "edit" => Err("Usage: :e PATH".into()),
        // An empty base means the automatic default.
        "base" => Ok(Command::Base(argument.into())),
        _ => match COMMANDS.iter().find(|(n, _)| *n == name) {
            Some((_, Some(action))) if argument.is_empty() => Ok(Command::Run(*action)),
            _ => Err(format!("Unknown command: {input}")),
        },
    }
}

/// Command names starting with `prefix`, in table order.
pub fn completions(prefix: &str) -> Vec<&'static str> {
    COMMANDS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| name.starts_with(prefix))
        .collect()
}
