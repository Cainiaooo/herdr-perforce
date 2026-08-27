//! Syntax highlighting for the standalone content pane.

use std::sync::OnceLock;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};

/// Avoid pathological regex backtracking on generated or minified files.
const MAX_HIGHLIGHT_LINE_LEN: usize = 2_000;

fn assets() -> &'static (SyntaxSet, Theme) {
    static ASSETS: OnceLock<(SyntaxSet, Theme)> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let syntaxes = two_face::syntax::extra_newlines();
        let mut themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .remove("base16-ocean.dark")
            .or_else(|| themes.themes.pop_first().map(|(_, theme)| theme))
            .unwrap_or_default();
        (syntaxes, theme)
    })
}

/// Highlight text by filename. Unknown formats deliberately fall back to the
/// caller's plain rendering rather than guessing.
pub fn highlight(name: &str, text: &str, max_lines: usize) -> Option<Vec<Line<'static>>> {
    let (syntaxes, theme) = assets();
    let extension = name.rsplit('.').next().unwrap_or_default();
    let syntax = syntaxes
        .find_syntax_by_extension(extension)
        .or_else(|| syntaxes.find_syntax_by_extension(name))
        .or_else(|| {
            text.lines()
                .next()
                .and_then(|line| syntaxes.find_syntax_by_first_line(line))
        })?;

    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut output = Vec::new();
    for raw in LinesWithEndings::from(text).take(max_lines) {
        if raw.len() > MAX_HIGHLIGHT_LINE_LEN {
            output.push(Line::raw(raw.trim_end_matches(['\n', '\r']).to_owned()));
            continue;
        }
        let Ok(regions) = highlighter.highlight_line(raw, syntaxes) else {
            output.push(Line::raw(raw.trim_end_matches(['\n', '\r']).to_owned()));
            continue;
        };
        let spans = regions
            .into_iter()
            .filter_map(|(style, chunk)| {
                let chunk = chunk.trim_end_matches(['\n', '\r']);
                if chunk.is_empty() {
                    return None;
                }
                let foreground = style.foreground;
                let mut rendered =
                    Style::default().fg(Color::Rgb(foreground.r, foreground.g, foreground.b));
                if style.font_style.contains(FontStyle::BOLD) {
                    rendered = rendered.add_modifier(Modifier::BOLD);
                }
                if style.font_style.contains(FontStyle::ITALIC) {
                    rendered = rendered.add_modifier(Modifier::ITALIC);
                }
                Some(Span::styled(chunk.to_owned(), rendered))
            })
            .collect::<Vec<_>>();
        output.push(Line::from(spans));
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_and_extended_formats_are_highlighted() {
        let rust = highlight("main.rs", "fn main() {}\n", 10).expect("Rust grammar");
        assert!(rust[0].spans.iter().any(|span| span.style.fg.is_some()));
        assert!(highlight("app.ts", "const value: string = \"ok\";\n", 10).is_some());
        assert!(highlight("Cargo.toml", "[package]\nname = \"demo\"\n", 10).is_some());
    }

    #[test]
    fn unknown_formats_and_long_lines_fail_softly() {
        assert!(highlight("data.qqzz", "plain\n", 10).is_none());
        let long = format!(
            "const value = \"{}\";\n",
            "x".repeat(MAX_HIGHLIGHT_LINE_LEN + 1)
        );
        let highlighted = highlight("bundle.js", &long, 10).expect("JavaScript grammar");
        assert_eq!(highlighted[0].to_string(), long.trim_end_matches('\n'));
    }
}
