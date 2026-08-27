//! Width-aware wrapping for styled File and Diff content.

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthChar;

const TAB_WIDTH: usize = 4;

/// Split one styled source line into display rows without dropping styles or
/// characters. Break at the last fitting space when possible, otherwise hard
/// break a word that is wider than the pane.
pub fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![line.clone()];
    }
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|span| {
            let style = span.style;
            span.content
                .chars()
                .map(move |character| (character, style))
        })
        .collect();
    if cells.is_empty() {
        return vec![line.clone()];
    }

    let mut rows = Vec::new();
    let mut start = 0;
    while start < cells.len() {
        let mut end = start;
        let mut used = 0;
        let mut last_space = None;
        while end < cells.len() {
            let (character, _) = cells[end];
            let cell_width = character_width(character, used);
            if used + cell_width > width && end > start {
                break;
            }
            used += cell_width;
            end += 1;
            if character == ' ' {
                last_space = Some(end);
            }
        }
        let cut = match last_space {
            Some(boundary) if end < cells.len() && cells[end].0 != ' ' && boundary > start => {
                boundary
            }
            _ => end,
        };
        rows.push(row_from(&cells[start..cut], line.style));
        start = cut;
    }
    rows
}

/// Wrap a numbered source line while keeping its first span as a fixed gutter.
/// Continuation rows receive an equally wide blank gutter, so their text begins
/// at the same column as the first row instead of underneath the line number.
pub fn wrap_line_with_gutter(
    line: &Line<'static>,
    width: usize,
    gutter_width: usize,
) -> Vec<Line<'static>> {
    if gutter_width == 0 || width <= gutter_width || line.spans.is_empty() {
        return wrap_line(line, width);
    }
    let gutter = line.spans[0].clone();
    let mut body = Line::from(line.spans[1..].to_vec());
    body.style = line.style;
    wrap_line(&body, width - gutter_width)
        .into_iter()
        .enumerate()
        .map(|(index, body_row)| {
            let prefix = if index == 0 {
                gutter.clone()
            } else {
                Span::styled(" ".repeat(gutter_width), gutter.style)
            };
            let mut spans = vec![prefix];
            spans.extend(body_row.spans);
            let mut row = Line::from(spans);
            row.style = body_row.style;
            row
        })
        .collect()
}

fn row_from(cells: &[(char, Style)], line_style: Style) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut column = 0;
    for (character, style) in cells {
        let text = if *character == '\t' {
            " ".repeat(character_width(*character, column))
        } else {
            character.to_string()
        };
        match spans.last_mut() {
            Some(previous) if previous.style == *style => previous.content.to_mut().push_str(&text),
            _ => spans.push(Span::styled(text, *style)),
        }
        column += character_width(*character, column);
    }
    let mut row = Line::from(spans);
    row.style = line_style;
    row
}

fn character_width(character: char, column: usize) -> usize {
    if character == '\t' {
        TAB_WIDTH - (column % TAB_WIDTH)
    } else {
        character.width().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Stylize};

    fn texts(rows: &[Line<'static>]) -> Vec<String> {
        rows.iter()
            .map(|row| row.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn long_lines_wrap_without_losing_content() {
        let rows = wrap_line(&Line::raw("the quick brown fox jumps"), 10);
        assert_eq!(texts(&rows), vec!["the quick ", "brown fox ", "jumps"]);
        assert!(rows.iter().all(|row| row.width() <= 10));
    }

    #[test]
    fn hard_breaks_long_words_and_counts_wide_glyphs() {
        let word = wrap_line(&Line::raw("supercalifragilistic"), 7);
        assert_eq!(texts(&word).concat(), "supercalifragilistic");
        assert!(word.iter().all(|row| row.width() <= 7));

        let wide = wrap_line(&Line::raw("日本語テキスト"), 6);
        assert_eq!(texts(&wide).concat(), "日本語テキスト");
        assert!(wide.iter().all(|row| row.width() <= 6));
    }

    #[test]
    fn styles_and_tabs_survive_wrapping() {
        let line = Line::from(vec![
            Span::styled("aaaa ", Style::default().fg(Color::Red)),
            Span::styled("bbbb cccc", Style::default().fg(Color::Green)),
        ])
        .on_blue();
        let rows = wrap_line(&line, 6);
        assert_eq!(texts(&rows).concat(), "aaaa bbbb cccc");
        assert!(rows.iter().all(|row| row.style.bg == Some(Color::Blue)));
        assert_eq!(
            texts(&wrap_line(&Line::raw("\t1234"), 4)),
            vec!["    ", "1234"]
        );
    }

    #[test]
    fn numbered_continuations_keep_a_blank_aligned_gutter() {
        let line = Line::from(vec![
            Span::styled("12  ", Style::default().fg(Color::DarkGray)),
            Span::styled("alpha beta gamma", Style::default().fg(Color::Green)),
        ]);
        let rows = wrap_line_with_gutter(&line, 12, 4);
        assert_eq!(texts(&rows), vec!["12  alpha ", "    beta ", "    gamma"]);
        assert!(rows.iter().all(|row| row.width() <= 12));
        assert!(
            rows.iter()
                .skip(1)
                .all(|row| row.spans[0].content == "    ")
        );
    }
}
