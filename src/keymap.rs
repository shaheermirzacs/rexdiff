//! The one table of key bindings. Dispatch, the help overlay, and the
//! command line all read it, so listed keys cannot drift from bound keys.

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
    Stage,
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
    Count(u8),
    MotionPrefix,
    Search,
    SearchNext,
    SearchPrev,
    VisualChar,
    VisualLine,
    EditNote,
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
            PrevHunk | NextHunk | PrevFile | NextFile | Up | Down | PageUp | PageDown | HalfUp | HalfDown
                | Left | Right | ScrollUp | ScrollDown | Move(_) | SearchNext | SearchPrev
        )
    }
}

/// Where a binding applies. `Changes` covers all three review panes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Global,
    Changes,
    Tree,
    Reader,
    Notes,
    Branches,
    History,
}

impl Scope {
    /// The tables consulted for a focused scope, most specific first.
    fn layers(self) -> [Scope; 3] {
        match self {
            Scope::Tree | Scope::Reader | Scope::Notes => [self, Scope::Changes, Scope::Global],
            _ => [self, self, Scope::Global],
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Scope::Tree => "Changes · file tree",
            Scope::Reader => "Changes · file reader",
            Scope::Notes => "Changes · notes",
            Scope::Branches => "Branches",
            Scope::History => "History",
            Scope::Global | Scope::Changes => "Changes",
        }
    }
}

/// A key press with exact modifiers, or the second key of `Space x` / `z x`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Press(KeyCode, KeyModifiers),
    Leader(char),
    Z(char),
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
            Key::Z(c) => format!("z{c}"),
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
    Binding { scope, key, action, help }
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
use Scope::{Branches, Changes, Global, History, Notes, Reader, Tree};

// A `static`, not a `const`: `help` compares bindings by address.
pub static BINDINGS: &[Binding] = &[
    b(Global, ch('q'), A::Quit, "Quit"),
    b(Global, ctrl('c'), A::Quit, "Quit"),
    b(Global, ch('?'), A::Help, "Help for this screen"),
    b(Global, ch(':'), A::Command, "Command line (Tab completes)"),
    b(Global, ch('1'), A::Changes, "Changes screen"),
    b(Global, ch('2'), A::Branches, "Branches screen"),
    b(Global, ch('W'), A::WorkingTree, "Working-tree changes"),
    // Shared by the tree, reader and notes.
    b(Changes, ch('j'), A::Down, "Down"),
    b(Changes, code(KeyCode::Down), A::Down, "Down"),
    b(Changes, ch('k'), A::Up, "Up"),
    b(Changes, code(KeyCode::Up), A::Up, "Up"),
    b(Changes, ch('g'), A::Top, "Top of the focused list or file"),
    b(Changes, code(KeyCode::Home), A::Top, "Top of the focused list or file"),
    b(Changes, ch('G'), A::Bottom, "Bottom of the focused list or file (NG: source line N)"),
    b(Changes, code(KeyCode::End), A::Bottom, "Bottom of the focused list or file (NG: source line N)"),
    b(Changes, ctrl('d'), A::HalfDown, "Half page down"),
    b(Changes, ctrl('u'), A::HalfUp, "Half page up"),
    b(Changes, ctrl('f'), A::PageDown, "Page down"),
    b(Changes, code(KeyCode::PageDown), A::PageDown, "Page down"),
    b(Changes, ctrl('b'), A::PageUp, "Page up"),
    b(Changes, code(KeyCode::PageUp), A::PageUp, "Page up"),
    b(Changes, ch('a'), A::Annotate, "Annotate line or selection (file in the tree)"),
    b(Changes, ch('A'), A::AnnotateFile, "Annotate the whole file"),
    b(Changes, ch('c'), A::Commit, "Commit staged changes"),
    b(Changes, ch('s'), A::Send, "Send notes to an agent"),
    b(Changes, ch('L'), A::Notes, "Show / focus notes; again hides"),
    b(Changes, ch('x'), A::Context, "Whole-file context"),
    b(Changes, ch('S'), A::Layout, "Split / stacked layout"),
    b(Changes, ctrl('p'), A::Picker, "Project file picker"),
    b(Changes, ch('['), A::PrevHunk, "Previous hunk"),
    b(Changes, ch(']'), A::NextHunk, "Next hunk"),
    b(Changes, ch('{'), A::PrevFile, "Previous file"),
    b(Changes, ch('}'), A::NextFile, "Next file"),
    b(Changes, code(KeyCode::Tab), A::FocusNext, "Next pane (opens the file in the tree)"),
    b(Changes, code(KeyCode::BackTab), A::FocusPrev, "Previous pane"),
    b(Changes, ch(' '), A::Leader, "Leader prefix"),
    b(Changes, Key::Leader('e'), A::Sidebar, "Hide / show the file tree"),
    b(Changes, Key::Leader('h'), A::OldSide, "Old side of a split diff"),
    b(Changes, Key::Leader('l'), A::NewSide, "New side of a split diff"),
    b(Changes, code(KeyCode::Esc), A::Back, "Back"),
    // The file tree.
    b(Tree, ch('h'), A::Left, "Collapse / parent folder"),
    b(Tree, code(KeyCode::Left), A::Left, "Collapse / parent folder"),
    b(Tree, ch('l'), A::Right, "Expand / enter folder"),
    b(Tree, code(KeyCode::Right), A::Right, "Expand / enter folder"),
    b(Tree, code(KeyCode::Enter), A::Activate, "Open file / fold folder"),
    b(Tree, ch('z'), A::Stage, "Stage / unstage file; fold folder"),
    b(Tree, ch('d'), A::Discard, "Discard changes (y confirms)"),
    b(Tree, ch('J'), A::ScrollDown, "Scroll the file down"),
    b(Tree, ch('K'), A::ScrollUp, "Scroll the file up"),
    b(Tree, ch('H'), A::History, "Commits of the checked-out branch"),
    b(Tree, ch('D'), A::BranchDiff, "Branch diff from the merge base"),
    b(Tree, ch('B'), A::Base, "Comparison base"),
    b(Tree, ch('t'), A::AllFiles, "Changed / all project files"),
    b(Tree, ch('r'), A::Reload, "Reload the repository"),
    b(Tree, ch('+'), A::Grow, "Bigger tree"),
    b(Tree, ch('='), A::Grow, "Bigger tree"),
    b(Tree, ch('-'), A::Shrink, "Smaller tree"),
    // The file reader.
    b(Reader, ch('h'), A::Move(Motion::Left), "Left"),
    b(Reader, code(KeyCode::Left), A::Move(Motion::Left), "Left"),
    b(Reader, ch('l'), A::Move(Motion::Right), "Right"),
    b(Reader, code(KeyCode::Right), A::Move(Motion::Right), "Right"),
    b(Reader, ch('w'), A::Move(Motion::WordNext), "Next word"),
    b(Reader, ch('b'), A::Move(Motion::WordBack), "Previous word"),
    b(Reader, ch('e'), A::Move(Motion::WordEnd), "End of word"),
    b(Reader, ch('0'), A::Move(Motion::LineStart), "Line start"),
    b(Reader, ch('^'), A::Move(Motion::FirstNonBlank), "First non-blank"),
    b(Reader, ch('$'), A::Move(Motion::LineEnd), "Line end"),
    b(Reader, ctrl('e'), A::Move(Motion::ScrollDown), "Scroll down a line"),
    b(Reader, ctrl('y'), A::Move(Motion::ScrollUp), "Scroll up a line"),
    b(Reader, ch('z'), A::MotionPrefix, "Scroll prefix"),
    b(Reader, Key::Z('t'), A::Move(Motion::PlaceTop), "Cursor line to the top"),
    b(Reader, Key::Z('z'), A::Move(Motion::PlaceCenter), "Cursor line to the middle"),
    b(Reader, Key::Z('b'), A::Move(Motion::PlaceBottom), "Cursor line to the bottom"),
    b(Reader, Key::Z('h'), A::Move(Motion::ScrollLeft), "Scroll left (split)"),
    b(Reader, Key::Z('l'), A::Move(Motion::ScrollRight), "Scroll right (split)"),
    b(Reader, ch('v'), A::VisualChar, "Select characters"),
    b(Reader, ch('V'), A::VisualLine, "Select lines"),
    b(Reader, ch('/'), A::Search, "Regex search (smart case)"),
    b(Reader, ch('n'), A::SearchNext, "Next match"),
    b(Reader, ch('N'), A::SearchPrev, "Previous match"),
    b(Reader, ch('1'), A::Count(1), "Count prefix"),
    b(Reader, ch('2'), A::Count(2), "Count prefix"),
    b(Reader, ch('3'), A::Count(3), "Count prefix"),
    b(Reader, ch('4'), A::Count(4), "Count prefix"),
    b(Reader, ch('5'), A::Count(5), "Count prefix"),
    b(Reader, ch('6'), A::Count(6), "Count prefix"),
    b(Reader, ch('7'), A::Count(7), "Count prefix"),
    b(Reader, ch('8'), A::Count(8), "Count prefix"),
    b(Reader, ch('9'), A::Count(9), "Count prefix"),
    b(Reader, code(KeyCode::Esc), A::Back, "Clear selection, then search, then back"),
    // The notes list.
    b(Notes, code(KeyCode::Enter), A::EditNote, "Edit the note"),
    b(Notes, ch('d'), A::DeleteNote, "Delete the note"),
    b(Notes, ch('q'), A::Back, "Close notes"),
    b(Notes, code(KeyCode::Esc), A::Back, "Close notes"),
    // Branches.
    b(Branches, ch('j'), A::Down, "Down"),
    b(Branches, code(KeyCode::Down), A::Down, "Down"),
    b(Branches, ch('k'), A::Up, "Up"),
    b(Branches, code(KeyCode::Up), A::Up, "Up"),
    b(Branches, ch('g'), A::Top, "First branch"),
    b(Branches, code(KeyCode::Home), A::Top, "First branch"),
    b(Branches, ch('G'), A::Bottom, "Last branch"),
    b(Branches, code(KeyCode::End), A::Bottom, "Last branch"),
    b(Branches, ch(' '), A::Checkout, "Check out (remote: tracking branch)"),
    b(Branches, code(KeyCode::Enter), A::OpenHistory, "Commit history"),
    b(Branches, ch('p'), A::Pull, "Pull"),
    b(Branches, ch('P'), A::Push, "Push"),
    b(Branches, ch('m'), A::Merge, "Merge into the current branch"),
    b(Branches, ch('d'), A::DeleteBranch, "Delete (y/Enter confirms)"),
    b(Branches, ch('D'), A::ForceDeleteBranch, "Force-delete (y/Enter confirms)"),
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
    b(History, ch('D'), A::BranchDiff, "Branch diff from the merge base"),
    b(History, ch('B'), A::Base, "Comparison base"),
    b(History, code(KeyCode::Esc), A::Back, "Back to Branches"),
    b(History, code(KeyCode::Backspace), A::Back, "Back to Branches"),
    b(History, ch('h'), A::Back, "Back to Branches"),
    b(History, code(KeyCode::Left), A::Back, "Back to Branches"),
];

/// The binding a key resolves to in `scope`.
pub fn lookup(scope: Scope, key: Key) -> Option<&'static Binding> {
    scope.layers().into_iter().find_map(|layer| BINDINGS.iter().find(|b| b.scope == layer && b.key == key))
}

/// The keys of `scope` with what they do, grouped by action, without keys
/// a more specific table takes over.
pub fn help(scope: Scope) -> Vec<(Vec<Key>, &'static str, Action)> {
    let mut out: Vec<(Vec<Key>, &'static str, Action)> = Vec::new();
    let mut seen = Vec::new();
    for layer in scope.layers() {
        if seen.contains(&layer) {
            continue;
        }
        seen.push(layer);
        for binding in BINDINGS.iter().filter(|b| b.scope == layer) {
            if lookup(scope, binding.key).is_none_or(|winner| !std::ptr::eq(winner, binding)) {
                continue;
            }
            match out.iter_mut().find(|(_, help, action)| *help == binding.help && *action == binding.action) {
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
    let (name, argument) = input.split_once(char::is_whitespace).map_or((input, ""), |(n, a)| (n, a.trim()));
    if let Ok(line) = name.parse::<u32>() {
        return Ok(Command::Source(line));
    }
    match name {
        "Source" => argument.parse().map(Command::Source).map_err(|_| "Usage: :Source N".into()),
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
    COMMANDS.iter().map(|(name, _)| *name).filter(|name| name.starts_with(prefix)).collect()
}
