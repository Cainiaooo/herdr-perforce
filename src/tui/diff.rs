//! Inline overlay rendering for the Content pane Diff view.
//!
//! Layout matches Codex CLI / VS Code inline: the current file is the canvas,
//! `+/-` live in the gutter, add/delete use muted full-line tints, and paired
//! replacements highlight the changed tokens more brightly.

use std::collections::BTreeSet;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::domain::{FileDiff, FoldRange, IntraSpan, OverlayKind, OverlayLine};

use super::{syntax, wrap};

const ADD_LINE_BG: Color = Color::Rgb(33, 58, 43);
const DEL_LINE_BG: Color = Color::Rgb(74, 34, 29);
const ADD_WORD_BG: Color = Color::Rgb(46, 104, 64);
const DEL_WORD_BG: Color = Color::Rgb(122, 48, 42);
const FOLD_FG: Color = Color::Rgb(125, 174, 199);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffToolbarAction {
    PrevHunk,
    NextHunk,
    ToggleFolds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToolbarHit {
    pub action: DiffToolbarAction,
    pub x: u16,
    pub width: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VisibleItem {
    Line(usize),
    Fold(FoldRange),
}

#[derive(Debug, Clone)]
pub(crate) struct DiffViewState {
    pub model: FileDiff,
    pub expanded: BTreeSet<usize>,
    pub current_hunk: usize,
    pub gutter_width: usize,
    pub truncated: Option<String>,
    highlighted: Option<Vec<Line<'static>>>,
}

impl DiffViewState {
    pub(crate) fn new(filename: String, model: FileDiff, truncated: Option<String>) -> Self {
        let max_number = model
            .lines
            .iter()
            .flat_map(|line| [line.old_no, line.new_no])
            .flatten()
            .max()
            .unwrap_or(1);
        let number_width = max_number.max(1).ilog10() as usize + 1;
        let highlighted = highlight_new_file(&filename, &model.lines);
        Self {
            model,
            expanded: BTreeSet::new(),
            current_hunk: 0,
            gutter_width: number_width + 2,
            truncated,
            highlighted,
        }
    }

    pub(crate) fn visible_items(&self) -> Vec<VisibleItem> {
        let mut items = Vec::new();
        let mut index = 0;
        while index < self.model.lines.len() {
            if let Some(fold) = self
                .model
                .folds
                .iter()
                .find(|fold| fold.start == index && !self.expanded.contains(&fold.id))
            {
                items.push(VisibleItem::Fold(*fold));
                index = fold.end;
                continue;
            }
            items.push(VisibleItem::Line(index));
            index += 1;
        }
        items
    }

    pub(crate) fn body_lines(&self) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = self
            .visible_items()
            .into_iter()
            .map(|item| match item {
                VisibleItem::Line(index) => paint_overlay_line(
                    &self.model.lines[index],
                    self.gutter_width,
                    self.highlighted
                        .as_ref()
                        .and_then(|lines| {
                            self.model.lines[index]
                                .new_no
                                .and_then(|number| lines.get(number - 1))
                        })
                        .cloned(),
                ),
                VisibleItem::Fold(fold) => {
                    paint_fold_line(&self.model.lines, fold, self.gutter_width)
                }
            })
            .collect();
        if let Some(reason) = &self.truncated {
            lines.push(Line::from(vec![
                Span::styled(" ".repeat(self.gutter_width), Color::DarkGray),
                Span::styled(reason.clone(), Color::Yellow),
            ]));
        }
        if self.model.lines.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(" ".repeat(self.gutter_width), Color::DarkGray),
                Span::styled("(no textual differences)", Color::DarkGray),
            ]));
        }
        lines
    }

    pub(crate) fn hunk_count(&self) -> usize {
        self.model.hunks.len()
    }

    pub(crate) fn folds_all_expanded(&self) -> bool {
        !self.model.folds.is_empty()
            && self
                .model
                .folds
                .iter()
                .all(|fold| self.expanded.contains(&fold.id))
    }

    pub(crate) fn toggle_folds(&mut self) {
        if self.model.folds.is_empty() {
            return;
        }
        if self.folds_all_expanded() {
            self.expanded.clear();
        } else {
            self.expanded = self.model.folds.iter().map(|fold| fold.id).collect();
        }
    }

    pub(crate) fn expand_fold(&mut self, id: usize) {
        self.expanded.insert(id);
    }

    pub(crate) fn step_hunk(&mut self, delta: isize) -> bool {
        if self.model.hunks.is_empty() {
            return false;
        }
        let last = self.model.hunks.len() - 1;
        let next = if delta < 0 {
            self.current_hunk.saturating_sub(delta.unsigned_abs())
        } else {
            self.current_hunk.saturating_add(delta as usize).min(last)
        };
        if next == self.current_hunk && delta != 0 {
            return false;
        }
        self.current_hunk = next;
        true
    }

    pub(crate) fn current_hunk_overlay_index(&self) -> Option<usize> {
        self.model.hunks.get(self.current_hunk).copied()
    }

    pub(crate) fn visual_row_for_overlay(&self, overlay_index: usize, width: usize) -> usize {
        let mut visual = 0;
        for item in self.visible_items() {
            let line = match item {
                VisibleItem::Line(index) if index == overlay_index => {
                    return visual;
                }
                VisibleItem::Line(index) => {
                    paint_overlay_line(&self.model.lines[index], self.gutter_width, None)
                }
                VisibleItem::Fold(fold) => {
                    if overlay_index >= fold.start && overlay_index < fold.end {
                        return visual;
                    }
                    paint_fold_line(&self.model.lines, fold, self.gutter_width)
                }
            };
            visual += wrap::wrap_line_with_gutter(&line, width.max(1), self.gutter_width).len();
        }
        visual
    }

    pub(crate) fn fold_at_visual_row(&self, visual_row: usize, width: usize) -> Option<usize> {
        let mut visual = 0;
        for item in self.visible_items() {
            let line = match item {
                VisibleItem::Line(index) => {
                    paint_overlay_line(&self.model.lines[index], self.gutter_width, None)
                }
                VisibleItem::Fold(fold) => {
                    paint_fold_line(&self.model.lines, fold, self.gutter_width)
                }
            };
            let height = wrap::wrap_line_with_gutter(&line, width.max(1), self.gutter_width).len();
            if visual_row >= visual && visual_row < visual + height {
                return match item {
                    VisibleItem::Fold(fold) if fold.expandable => Some(fold.id),
                    _ => None,
                };
            }
            visual += height;
        }
        None
    }
}

pub(crate) fn stats_spans(diff: &FileDiff) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!("+{}", diff.added),
            Style::default().fg(Color::Green),
        ),
        Span::raw(" "),
        Span::styled(
            format!("-{}", diff.removed),
            Style::default().fg(Color::Red),
        ),
    ]
}

pub(crate) fn toolbar_line(
    state: &DiffViewState,
    _width: usize,
) -> (Line<'static>, Vec<ToolbarHit>) {
    let hunks = state.hunk_count();
    let hunk_label = if hunks == 0 {
        "hunk 0/0".to_owned()
    } else {
        format!("hunk {}/{hunks}", state.current_hunk + 1)
    };
    let fold_label = if state.model.folds.is_empty() {
        None
    } else if state.folds_all_expanded() {
        Some("Fold unchanged")
    } else {
        Some("Expand all")
    };

    let mut spans = Vec::new();
    let mut hits = Vec::new();
    let mut x = 0usize;
    let push_button = |spans: &mut Vec<Span<'static>>,
                       hits: &mut Vec<ToolbarHit>,
                       x: &mut usize,
                       action: DiffToolbarAction,
                       label: &str,
                       enabled: bool| {
        let text = format!(" {label} ");
        let width = text.len();
        hits.push(ToolbarHit {
            action,
            x: *x as u16,
            width: width as u16,
        });
        let style = if enabled {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Gray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray).bg(Color::Indexed(236))
        };
        spans.push(Span::styled(text, style));
        *x += width;
        spans.push(Span::raw(" "));
        *x += 1;
    };

    let can_prev = hunks > 0 && state.current_hunk > 0;
    let can_next = hunks > 0 && state.current_hunk + 1 < hunks;
    push_button(
        &mut spans,
        &mut hits,
        &mut x,
        DiffToolbarAction::PrevHunk,
        "Prev",
        can_prev,
    );
    push_button(
        &mut spans,
        &mut hits,
        &mut x,
        DiffToolbarAction::NextHunk,
        "Next",
        can_next,
    );
    spans.push(Span::styled(hunk_label, Color::DarkGray));
    x += spans.last().map(|span| span.content.len()).unwrap_or(0);
    if let Some(label) = fold_label {
        spans.push(Span::raw("  "));
        x += 2;
        push_button(
            &mut spans,
            &mut hits,
            &mut x,
            DiffToolbarAction::ToggleFolds,
            label,
            true,
        );
    }
    (Line::from(spans), hits)
}

pub(crate) fn hit_action(hits: &[ToolbarHit], column: u16) -> Option<DiffToolbarAction> {
    hits.iter().find_map(|hit| {
        (column >= hit.x && column < hit.x.saturating_add(hit.width)).then_some(hit.action)
    })
}

fn highlight_new_file(filename: &str, lines: &[OverlayLine]) -> Option<Vec<Line<'static>>> {
    let mut numbered = Vec::new();
    let mut expected = 1usize;
    for line in lines {
        let Some(number) = line.new_no else {
            continue;
        };
        while expected < number {
            numbered.push(String::new());
            expected += 1;
        }
        if number == expected {
            numbered.push(line.text.clone());
            expected += 1;
        }
    }
    if numbered.is_empty() {
        return None;
    }
    let text = numbered.join("\n");
    syntax::highlight(filename, &text, numbered.len())
}

fn paint_overlay_line(
    line: &OverlayLine,
    gutter_width: usize,
    syntax_line: Option<Line<'static>>,
) -> Line<'static> {
    let number = match line.kind {
        OverlayKind::Delete => line.old_no,
        OverlayKind::Insert | OverlayKind::Context => line.new_no.or(line.old_no),
    }
    .unwrap_or(0);
    let number_width = gutter_width.saturating_sub(2);
    let (sign, line_bg, sign_fg) = match line.kind {
        OverlayKind::Insert => ('+', Some(ADD_LINE_BG), Color::Green),
        OverlayKind::Delete => ('-', Some(DEL_LINE_BG), Color::Red),
        OverlayKind::Context => (' ', None, Color::DarkGray),
    };
    let gutter = format!("{number:>number_width$} {sign}");
    let gutter_style = match line.kind {
        OverlayKind::Insert => Style::default().fg(sign_fg).bg(ADD_LINE_BG),
        OverlayKind::Delete => Style::default().fg(sign_fg).bg(DEL_LINE_BG),
        OverlayKind::Context => Style::default().fg(Color::DarkGray),
    };
    let mut spans = vec![Span::styled(gutter, gutter_style)];
    spans.extend(paint_body(line, syntax_line));
    let mut rendered = Line::from(spans);
    if let Some(background) = line_bg {
        rendered.style = Style::default().bg(background);
    }
    rendered
}

fn paint_body(line: &OverlayLine, syntax_line: Option<Line<'static>>) -> Vec<Span<'static>> {
    let word_bg = match line.kind {
        OverlayKind::Insert => Some(ADD_WORD_BG),
        OverlayKind::Delete => Some(DEL_WORD_BG),
        OverlayKind::Context => None,
    };
    let dim = line.kind == OverlayKind::Delete;
    let syntax_spans = syntax_line.filter(|syntax| {
        syntax
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
            == line.text
    });
    let base = match syntax_spans {
        Some(syntax) => syntax.spans,
        None => vec![Span::styled(
            line.text.clone(),
            match line.kind {
                OverlayKind::Insert => Style::default().fg(Color::Green),
                OverlayKind::Delete => Style::default().fg(Color::Red),
                OverlayKind::Context => Style::default(),
            },
        )],
    };
    let mut painted = apply_intra_spans(base, &line.intra, word_bg);
    if dim {
        for span in &mut painted {
            span.style = span.style.add_modifier(Modifier::DIM);
        }
    }
    painted
}

fn apply_intra_spans(
    spans: Vec<Span<'static>>,
    intra: &[IntraSpan],
    word_bg: Option<Color>,
) -> Vec<Span<'static>> {
    if intra.is_empty() || word_bg.is_none() {
        return spans;
    }
    let word_bg = word_bg.expect("checked");
    let mut output = Vec::new();
    let mut cursor = 0usize;
    for span in spans {
        let text = span.content.clone().into_owned();
        let start = cursor;
        let end = cursor + text.len();
        cursor = end;
        let mut offset = 0usize;
        while offset < text.len() {
            let abs = start + offset;
            let changed = intra
                .iter()
                .find(|span| abs >= span.start && abs < span.end);
            let next = if let Some(range) = changed {
                range.end.min(end)
            } else {
                intra
                    .iter()
                    .filter(|span| span.start > abs)
                    .map(|span| span.start)
                    .min()
                    .unwrap_or(end)
                    .min(end)
            };
            let piece = text[offset..next.saturating_sub(start)].to_owned();
            if !piece.is_empty() {
                let mut style = span.style;
                if changed.is_some() {
                    style = style.bg(word_bg).add_modifier(Modifier::BOLD);
                }
                output.push(Span::styled(piece, style));
            }
            offset = next.saturating_sub(start);
        }
    }
    output
}

fn paint_fold_line(lines: &[OverlayLine], fold: FoldRange, gutter_width: usize) -> Line<'static> {
    let first = lines
        .get(fold.start)
        .and_then(|line| line.new_no.or(line.old_no));
    let last = lines
        .get(fold.end.saturating_sub(1))
        .and_then(|line| line.new_no.or(line.old_no));
    let range = match (first, last) {
        (Some(start), Some(end)) => format!(" · {start}–{end}"),
        _ => String::new(),
    };
    let hint = if fold.expandable { "  [expand]" } else { "" };
    Line::from(vec![
        Span::styled(" ".repeat(gutter_width), Color::DarkGray),
        Span::styled(
            format!("▸ {} unchanged lines{range}{hint}", fold.hidden()),
            Style::default().fg(FOLD_FG),
        ),
    ])
}

pub(crate) fn pad_background(line: Line<'static>, width: usize) -> Line<'static> {
    let Some(background) = line.style.bg else {
        return line;
    };
    let used = line.width();
    if used >= width {
        return line;
    }
    let mut spans = line.spans;
    spans.push(Span::styled(
        " ".repeat(width - used),
        Style::default().bg(background),
    ));
    let mut padded = Line::from(spans);
    padded.style = line.style;
    padded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{FileDiffKind, build_file_diff};

    fn texts(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn gutter_uses_plus_and_minus_with_semantic_backgrounds() {
        let model = build_file_diff(
            &["new".into()],
            &["@@ -1 +1 @@".into(), "-old".into(), "+new".into()],
            Some(FileDiffKind::Edit),
            5,
        );
        let view = DiffViewState::new("a.rs".into(), model, None);
        let lines = view.body_lines();
        assert!(texts(&lines[0]).contains("-"));
        assert!(texts(&lines[1]).contains("+"));
        assert_eq!(lines[0].style.bg, Some(DEL_LINE_BG));
        assert_eq!(lines[1].style.bg, Some(ADD_LINE_BG));
        assert!(lines[0].spans[0].content.contains('-'));
        assert!(lines[1].spans[0].content.contains('+'));
    }

    #[test]
    fn fold_row_is_expandable_and_toolbar_can_jump_hunks() {
        let file: Vec<String> = (1..=30).map(|index| format!("line-{index}")).collect();
        let mut file = file;
        file[14] = "changed".into();
        let diff = vec![
            "@@ -13,5 +13,5 @@".into(),
            " line-13".into(),
            " line-14".into(),
            "-line-15".into(),
            "+changed".into(),
            " line-16".into(),
            " line-17".into(),
        ];
        let model = build_file_diff(&file, &diff, Some(FileDiffKind::Edit), 5);
        let mut view = DiffViewState::new("a.cpp".into(), model, None);
        assert!(!view.model.folds.is_empty());
        let body = view.body_lines();
        assert!(body.iter().any(
            |line| texts(line).contains("unchanged lines") && texts(line).contains("[expand]")
        ));
        let fold_id = view.model.folds[0].id;
        view.expand_fold(fold_id);
        assert!(view.expanded.contains(&fold_id));
        view.toggle_folds();
        assert!(view.folds_all_expanded());
        let (toolbar, hits) = toolbar_line(&view, 80);
        let toolbar_text = texts(&toolbar);
        assert!(toolbar_text.contains("Prev"));
        assert!(toolbar_text.contains("Next"));
        assert!(toolbar_text.contains("hunk 1/1"));
        assert!(
            hits.iter()
                .any(|hit| hit.action == DiffToolbarAction::PrevHunk)
        );
        assert_eq!(
            hit_action(&hits, hits[0].x),
            Some(DiffToolbarAction::PrevHunk)
        );
    }

    #[test]
    fn intra_line_span_gets_a_brighter_background() {
        let model = build_file_diff(
            &["CurrentHealth -= Applied;".into()],
            &[
                "@@ -1 +1 @@".into(),
                "-CurrentHealth -= Amount;".into(),
                "+CurrentHealth -= Applied;".into(),
            ],
            Some(FileDiffKind::Edit),
            5,
        );
        let view = DiffViewState::new("Health.cpp".into(), model, None);
        let lines = view.body_lines();
        let insert = &lines[1];
        assert!(
            insert
                .spans
                .iter()
                .any(|span| span.content.contains("Applied") && span.style.bg == Some(ADD_WORD_BG))
        );
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.content.contains("Amount") && span.style.bg == Some(DEL_WORD_BG))
        );
    }

    #[test]
    fn truncated_unified_diff_warning_is_rendered() {
        let model = build_file_diff(
            &["a".into(), "B".into(), "c".into(), "tail".into()],
            &[
                "@@ -1,3 +1,3 @@".into(),
                " a".into(),
                "-b".into(),
                "+B".into(),
                " c".into(),
                "truncated: 4000 line diff budget exceeded".into(),
            ],
            Some(FileDiffKind::Edit),
            5,
        );
        assert!(model.truncated.is_some());
        let view = DiffViewState::new("a.rs".into(), model.clone(), model.truncated.clone());
        assert!(
            view.body_lines()
                .iter()
                .any(|line| texts(line).contains("truncated: 4000 line diff budget exceeded"))
        );
        assert!(view.highlighted.is_some());
    }
}
