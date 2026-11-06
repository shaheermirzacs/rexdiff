//! Syntax highlighting with syntect, using the terminal's ANSI palette: each
//! scope maps to a color index (0–15), so Rex draws it with the theme's colors.

use std::ops::Range;
use std::str::FromStr;

use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color, FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSettings,
};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// Beyond this the diff is shown without syntax colors (everything else stays the same).
const MAX_HIGHLIGHT_ROWS: usize = 20_000;
const MAX_HIGHLIGHT_LINE: usize = 1_000;

pub struct Span {
    pub range: Range<usize>,
    pub fg: Option<u8>,
    pub bold: bool,
}

pub struct Highlighter {
    ss: SyntaxSet,
    theme: Theme,
}

impl Highlighter {
    pub fn new() -> Self {
        Self { ss: SyntaxSet::load_defaults_newlines(), theme: ansi_theme() }
    }

    fn syntax_for(&self, path: &str) -> Option<&SyntaxReference> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        // The default set has no TypeScript/Vue/etc.; JS and HTML come close.
        let ext = match ext {
            "ts" | "tsx" | "mts" | "cts" | "jsx" | "mjs" | "cjs" => "js",
            "vue" | "svelte" => "html",
            "toml" | "ini" | "cfg" | "conf" => "ini",
            "zsh" | "bash" | "env" => "sh",
            e => e,
        };
        self.ss
            .find_syntax_by_extension(ext)
            .or_else(|| self.ss.find_syntax_by_extension(name))
            .or_else(|| self.ss.find_syntax_by_token(ext))
            .filter(|s| s.name != "Plain Text")
    }

    pub fn for_path(&self, path: &str, rows: usize) -> FileHl<'_> {
        let syntax = if rows > MAX_HIGHLIGHT_ROWS { None } else { self.syntax_for(path) };
        let mut f = FileHl { hl: self, syntax, new: None, old: None };
        f.reset();
        f
    }
}

/// Two highlighting states per file: the new side (context + additions) and the
/// old side (context + deletions), so multi-line strings and comments come out
/// right on both sides.
pub struct FileHl<'a> {
    hl: &'a Highlighter,
    syntax: Option<&'a SyntaxReference>,
    new: Option<HighlightLines<'a>>,
    old: Option<HighlightLines<'a>>,
}

impl<'a> FileHl<'a> {
    pub fn reset(&mut self) {
        if let Some(s) = self.syntax {
            self.new = Some(HighlightLines::new(s, &self.hl.theme));
            self.old = Some(HighlightLines::new(s, &self.hl.theme));
        }
    }

    pub fn new_side(&mut self, line: &str) -> Vec<Span> {
        run(self.new.as_mut(), &self.hl.ss, line)
    }

    pub fn old_side(&mut self, line: &str) -> Vec<Span> {
        run(self.old.as_mut(), &self.hl.ss, line)
    }

    pub fn both(&mut self, line: &str) -> Vec<Span> {
        let _ = run(self.old.as_mut(), &self.hl.ss, line);
        run(self.new.as_mut(), &self.hl.ss, line)
    }
}

fn run(h: Option<&mut HighlightLines<'_>>, ss: &SyntaxSet, line: &str) -> Vec<Span> {
    let Some(h) = h else { return Vec::new() };
    if line.len() > MAX_HIGHLIGHT_LINE {
        return Vec::new();
    }
    let with_nl = format!("{line}\n");
    let Ok(parts) = h.highlight_line(&with_nl, ss) else { return Vec::new() };
    let mut spans = Vec::with_capacity(parts.len());
    let mut at = 0;
    for (style, text) in parts {
        let end = (at + text.len()).min(line.len());
        if end > at {
            let fg = (style.foreground.a == 0).then_some(style.foreground.r);
            let bold = style.font_style.contains(FontStyle::BOLD);
            spans.push(Span { range: at..end, fg, bold });
        }
        at += text.len();
    }
    spans
}

/// "ANSI" theme: `a == 0` marks a terminal palette color in the `r` channel;
/// `a == 1` is the default text color.
fn ansi_theme() -> Theme {
    const DEFAULT: Color = Color { r: 0, g: 0, b: 0, a: 1 };
    let ansi = |i: u8| Color { r: i, g: 0, b: 0, a: 0 };
    let item = |sel: &str, fg: Color, bold: bool| ThemeItem {
        scope: ScopeSelectors::from_str(sel).expect("valid scope selector"),
        style: StyleModifier {
            foreground: Some(fg),
            background: None,
            font_style: bold.then_some(FontStyle::BOLD),
        },
    };

    Theme {
        name: Some("ansi".into()),
        author: None,
        settings: ThemeSettings { foreground: Some(DEFAULT), ..Default::default() },
        scopes: vec![
            item("comment, punctuation.definition.comment", ansi(8), false),
            item("string, punctuation.definition.string, markup.inline.raw, markup.raw", ansi(2), false),
            item("constant.numeric, constant.language, constant.character, constant.other", ansi(3), false),
            item("string.regexp, constant.character.escape, support.constant", ansi(6), false),
            item("keyword, storage, keyword.operator.word, keyword.operator.logical.python", ansi(5), false),
            item("keyword.operator, punctuation", DEFAULT, false),
            item(
                "entity.name.function, support.function, variable.function, meta.function-call.generic",
                ansi(4),
                false,
            ),
            item(
                "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, \
                 support.type, support.class, entity.other.inherited-class",
                ansi(3),
                false,
            ),
            item("variable.language, meta.decorator, entity.name.function.decorator", ansi(6), false),
            item("entity.name.tag", ansi(4), false),
            item("entity.other.attribute-name, support.type.property-name", ansi(3), false),
            item("markup.heading, entity.name.section", ansi(4), true),
            item("markup.bold", DEFAULT, true),
            item("markup.underline.link, markup.link", ansi(6), false),
        ],
    }
}
