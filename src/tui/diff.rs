//! Inline overlay rendering for the Content pane Diff view.
//!
//! Layout matches Codex CLI / VS Code inline: the current file is the canvas,
//! `+/-` live in the gutter, add/delete use muted full-line tints, and paired
//! replacements highlight the changed tokens more brightly.

use std::collections::{BTreeMap, BTreeSet};

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::domain::{
    EXPAND_CHUNK, FileDiff, FoldRange, IntraSpan, MIN_FOLD_HIDDEN, OverlayKind, OverlayLine,
};

use super::{syntax, wrap};

const ADD_LINE_BG: Color = Color::Rgb(33, 58, 43);
const DEL_LINE_BG: Color = Color::Rgb(110, 32, 38);
const ADD_WORD_BG: Color = Color::Rgb(46, 104, 64);
const DEL_WORD_BG: Color = Color::Rgb(182, 48, 52);
const DEL_FG: Color = Color::Rgb(255, 168, 168);
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
pub(crate) enum ExpandDirection {
    /// Reveal more lines at the bottom of a hidden range (the splitter above a hunk).
    Up,
    /// Reveal more lines at the top of a hidden range (the splitter below a hunk).
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FoldEdge {
    pub id: usize,
    pub direction: ExpandDirection,
    pub remaining: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VisibleItem {
    Line(usize),
    FoldEdge(FoldEdge),
}

#[derive(Debug, Clone)]
pub(crate) struct DiffViewState {
    pub model: FileDiff,
    pub expanded: BTreeSet<usize>,
    pub current_hunk: usize,
    pub gutter_width: usize,
    pub truncated: Option<String>,
    revealed_start: BTreeMap<usize, usize>,
    revealed_end: BTreeMap<usize, usize>,
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
            revealed_start: BTreeMap::new(),
            revealed_end: BTreeMap::new(),
            highlighted,
        }
    }

    fn fold_reveal(&self, fold: &FoldRange) -> (usize, usize, usize) {
        if self.expanded.contains(&fold.id) {
            return (fold.hidden(), 0, 0);
        }
        let hidden = fold.hidden();
        let from_start = self
            .revealed_start
            .get(&fold.id)
            .copied()
            .unwrap_or(0)
            .min(hidden);
        let from_end = self
            .revealed_end
            .get(&fold.id)
            .copied()
            .unwrap_or(0)
            .min(hidden.saturating_sub(from_start));
        (
            from_start,
            from_end,
            hidden.saturating_sub(from_start + from_end),
        )
    }

    pub(crate) fn visible_items(&self) -> Vec<VisibleItem> {
        let mut items = Vec::new();
        let mut index = 0;
        let line_count = self.model.lines.len();
        while index < line_count {
            if let Some(fold) = self.model.folds.iter().find(|fold| fold.start == index) {
                let (from_start, from_end, remaining) = self.fold_reveal(fold);
                let remaining_start = fold.start + from_start;
                let remaining_end = fold.end - from_end;
                for line_index in fold.start..remaining_start {
                    items.push(VisibleItem::Line(line_index));
                }
                if remaining > 0 {
                    let show_down = remaining_start > 0;
                    let show_up = remaining_end < line_count;
                    if show_down {
                        items.push(VisibleItem::FoldEdge(FoldEdge {
                            id: fold.id,
                            direction: ExpandDirection::Down,
                            remaining,
                            start: remaining_start,
                            end: remaining_end,
                        }));
                    }
                    if show_up {
                        items.push(VisibleItem::FoldEdge(FoldEdge {
                            id: fold.id,
                            direction: ExpandDirection::Up,
                            remaining,
                            start: remaining_start,
                            end: remaining_end,
                        }));
                    }
                    if !show_down && !show_up {
                        items.push(VisibleItem::FoldEdge(FoldEdge {
                            id: fold.id,
                            direction: ExpandDirection::Down,
                            remaining,
                            start: remaining_start,
                            end: remaining_end,
                        }));
                    }
                }
                for line_index in remaining_end..fold.end {
                    items.push(VisibleItem::Line(line_index));
                }
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
                VisibleItem::FoldEdge(edge) => {
                    paint_expand_line(&self.model.lines, edge, self.gutter_width)
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
                .all(|fold| self.fold_reveal(fold).2 == 0)
    }

    pub(crate) fn toggle_folds(&mut self) {
        if self.model.folds.is_empty() {
            return;
        }
        if self.folds_all_expanded() {
            self.expanded.clear();
            self.revealed_start.clear();
            self.revealed_end.clear();
        } else {
            self.expanded = self.model.folds.iter().map(|fold| fold.id).collect();
            self.revealed_start.clear();
            self.revealed_end.clear();
        }
    }

    pub(crate) fn expand_fold(&mut self, id: usize) {
        self.expanded.insert(id);
        self.revealed_start.remove(&id);
        self.revealed_end.remove(&id);
    }

    pub(crate) fn expand_edge(&mut self, id: usize, direction: ExpandDirection) -> bool {
        let Some(fold) = self.model.folds.iter().copied().find(|fold| fold.id == id) else {
            return false;
        };
        let (mut from_start, mut from_end, remaining) = self.fold_reveal(&fold);
        if remaining == 0 {
            return false;
        }
        let chunk = EXPAND_CHUNK.min(remaining);
        let leftover = remaining - chunk;
        if leftover < MIN_FOLD_HIDDEN {
            self.expand_fold(id);
            return true;
        }
        match direction {
            ExpandDirection::Down => from_start += chunk,
            ExpandDirection::Up => from_end += chunk,
        }
        self.revealed_start.insert(id, from_start);
        self.revealed_end.insert(id, from_end);
        true
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
                VisibleItem::FoldEdge(edge) => {
                    if overlay_index >= edge.start && overlay_index < edge.end {
                        return visual;
                    }
                    paint_expand_line(&self.model.lines, edge, self.gutter_width)
                }
            };
            visual += wrap::wrap_line_with_gutter(&line, width.max(1), self.gutter_width).len();
        }
        visual
    }

    pub(crate) fn fold_at_visual_row(&self, visual_row: usize, width: usize) -> Option<FoldEdge> {
        let mut visual = 0;
        for item in self.visible_items() {
            let line = match item {
                VisibleItem::Line(index) => {
                    paint_overlay_line(&self.model.lines[index], self.gutter_width, None)
                }
                VisibleItem::FoldEdge(edge) => {
                    paint_expand_line(&self.model.lines, edge, self.gutter_width)
                }
            };
            let height = wrap::wrap_line_with_gutter(&line, width.max(1), self.gutter_width).len();
            if visual_row >= visual && visual_row < visual + height {
                return match item {
                    VisibleItem::FoldEdge(edge) => Some(edge),
                    VisibleItem::Line(_) => None,
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
        OverlayKind::Delete => ('-', Some(DEL_LINE_BG), DEL_FG),
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
    let syntax_spans = syntax_line
        .filter(|_| line.kind != OverlayKind::Delete)
        .filter(|syntax| {
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
                OverlayKind::Delete => Style::default().fg(DEL_FG),
                OverlayKind::Context => Style::default(),
            },
        )],
    };
    apply_intra_spans(base, &line.intra, word_bg)
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

fn paint_expand_line(lines: &[OverlayLine], edge: FoldEdge, gutter_width: usize) -> Line<'static> {
    let first = lines
        .get(edge.start)
        .and_then(|line| line.new_no.or(line.old_no));
    let last = lines
        .get(edge.end.saturating_sub(1))
        .and_then(|line| line.new_no.or(line.old_no));
    let range = match (first, last) {
        (Some(start), Some(end)) => format!(" {start}–{end}"),
        _ => String::new(),
    };
    let show = EXPAND_CHUNK.min(edge.remaining);
    let (arrow, side) = match edge.direction {
        ExpandDirection::Up => ('▲', "above"),
        ExpandDirection::Down => ('▼', "below"),
    };
    Line::from(vec![
        Span::styled(" ".repeat(gutter_width), Color::DarkGray),
        Span::styled(
            format!(
                "── {arrow} {show} more {side} · {remaining} hidden{range} ──",
                remaining = edge.remaining
            ),
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
        assert!(body.iter().any(|line| {
            let text = texts(line);
            text.contains("hidden") && (text.contains("▲") || text.contains("▼"))
        }));
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

    #[test]
    fn deleted_text_stays_bright_without_dim() {
        let model = build_file_diff(
            &["new".into()],
            &["@@ -1 +1 @@".into(), "-old".into(), "+new".into()],
            Some(FileDiffKind::Edit),
            5,
        );
        let view = DiffViewState::new("a.rs".into(), model, None);
        let deleted = &view.body_lines()[0];
        assert_eq!(deleted.style.bg, Some(DEL_LINE_BG));
        assert!(deleted.spans.iter().any(|span| span.content.contains("old")
            && span.style.fg == Some(DEL_FG)
            && !span.style.add_modifier.contains(Modifier::DIM)));
    }

    #[test]
    fn hunk_splitters_expand_twenty_lines_from_each_edge() {
        let mut file: Vec<String> = (1..=80).map(|index| format!("line-{index}")).collect();
        file[49] = "changed".into();
        let diff = vec![
            "@@ -48,5 +48,5 @@".into(),
            " line-48".into(),
            " line-49".into(),
            "-line-50".into(),
            "+changed".into(),
            " line-51".into(),
            " line-52".into(),
        ];
        let model = build_file_diff(&file, &diff, Some(FileDiffKind::Edit), 5);
        let mut view = DiffViewState::new("a.cpp".into(), model, None);
        let leading = view
            .model
            .folds
            .iter()
            .find(|fold| fold.start == 0)
            .copied()
            .expect("leading fold");
        let trailing = view
            .model
            .folds
            .iter()
            .find(|fold| fold.end == view.model.lines.len())
            .copied()
            .expect("trailing fold");
        let before = view
            .visible_items()
            .iter()
            .filter(|item| matches!(item, VisibleItem::Line(_)))
            .count();
        assert!(view.expand_edge(leading.id, ExpandDirection::Up));
        let after_up = view
            .visible_items()
            .iter()
            .filter(|item| matches!(item, VisibleItem::Line(_)))
            .count();
        assert_eq!(after_up, before + EXPAND_CHUNK);
        assert!(view.expand_edge(trailing.id, ExpandDirection::Down));
        let after_down = view
            .visible_items()
            .iter()
            .filter(|item| matches!(item, VisibleItem::Line(_)))
            .count();
        assert_eq!(after_down, after_up + EXPAND_CHUNK);
        let body = view.body_lines();
        assert!(body.iter().any(|line| texts(line).contains("▲")));
        assert!(body.iter().any(|line| texts(line).contains("▼")));
    }
}
