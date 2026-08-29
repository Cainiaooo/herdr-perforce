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
use unicode_width::UnicodeWidthStr;

use crate::domain::{
    EXPAND_CHUNK, FileDiff, FoldRange, IntraSpan, MIN_FOLD_HIDDEN, OverlayKind, OverlayLine,
};

use super::{syntax, theme, wrap};

const ADD_LINE_BG: Color = theme::ADD_LINE_BG.tui();
const DEL_LINE_BG: Color = theme::DEL_LINE_BG.tui();
const ADD_WORD_BG: Color = theme::ADD_WORD_BG.tui();
const DEL_WORD_BG: Color = theme::DEL_WORD_BG.tui();
const ADD_FG: Color = theme::ADD_MARK.tui();
const DEL_FG: Color = theme::DEL_MARK.tui();
const FOLD_FG: Color = theme::INFO.tui();
const FOLD_ROW_BG: Color = theme::FOLD_ROW_BG.tui();
const ELLIPSIS: &str = "⋯";
const BTN_DOWN: &str = "▼ 20";
const BTN_UP: &str = "▲ 20";

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
    /// Reveal more hidden lines above the following hunk.
    Up,
    /// Reveal more hidden lines below the preceding hunk.
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FoldEdge {
    pub id: usize,
    pub remaining: usize,
    pub start: usize,
    pub end: usize,
    pub expand_down: bool,
    pub expand_up: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FoldClick {
    Direction {
        id: usize,
        direction: ExpandDirection,
    },
    Both {
        id: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FoldButton {
    x: u16,
    width: u16,
    direction: ExpandDirection,
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
    number_width: usize,
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
        let number_width = (max_number.max(1).ilog10() as usize + 1).max(2);
        let highlighted = highlight_new_file(&filename, &model.lines);
        Self {
            model,
            expanded: BTreeSet::new(),
            current_hunk: 0,
            gutter_width: number_width * 2 + 3,
            truncated,
            number_width,
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
            if self.model.lines[index].kind == OverlayKind::Skipped {
                let remaining = self.model.lines[index]
                    .text
                    .parse::<usize>()
                    .unwrap_or(1)
                    .max(1);
                items.push(VisibleItem::FoldEdge(FoldEdge {
                    id: self
                        .model
                        .folds
                        .len()
                        .saturating_add(index)
                        .saturating_add(1),
                    remaining,
                    start: index,
                    end: index,
                    expand_down: false,
                    expand_up: false,
                }));
                index += 1;
                continue;
            }
            if let Some(fold) = self.model.folds.iter().find(|fold| fold.start == index) {
                let (from_start, from_end, remaining) = self.fold_reveal(fold);
                let remaining_start = fold.start + from_start;
                let remaining_end = fold.end - from_end;
                for line_index in fold.start..remaining_start {
                    items.push(VisibleItem::Line(line_index));
                }
                if remaining > 0 {
                    let expand_down = remaining_start > 0;
                    let expand_up = remaining_end < line_count;
                    items.push(VisibleItem::FoldEdge(FoldEdge {
                        id: fold.id,
                        remaining,
                        start: remaining_start,
                        end: remaining_end,
                        expand_down: expand_down || !expand_up,
                        expand_up,
                    }));
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
                    self.number_width,
                    self.highlighted
                        .as_ref()
                        .and_then(|lines| {
                            self.model.lines[index]
                                .new_no
                                .and_then(|number| lines.get(number - 1))
                        })
                        .cloned(),
                ),
                VisibleItem::FoldEdge(edge) => expand_row(edge, self.number_width).0,
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

    pub(crate) fn expand_both(&mut self, id: usize) -> bool {
        let down = self.expand_edge(id, ExpandDirection::Down);
        let up = self.expand_edge(id, ExpandDirection::Up);
        down || up
    }

    pub(crate) fn expand_visible(
        &mut self,
        scroll_y: usize,
        body_height: usize,
        width: usize,
    ) -> bool {
        let view_end = scroll_y.saturating_add(body_height.max(1));
        let mut visual = 0usize;
        let mut target = None;
        for item in self.visible_items() {
            let line = match &item {
                VisibleItem::Line(index) => {
                    paint_overlay_line(&self.model.lines[*index], self.number_width, None)
                }
                VisibleItem::FoldEdge(edge) => expand_row(*edge, self.number_width).0,
            };
            let height = wrap::wrap_line_with_gutter(&line, width.max(1), self.gutter_width)
                .len()
                .max(1);
            if let VisibleItem::FoldEdge(edge) = item
                && (edge.expand_down || edge.expand_up)
                && visual < view_end
                && visual + height > scroll_y
            {
                target = fold_click(edge);
                break;
            }
            visual += height;
        }
        let Some(click) = target else {
            return false;
        };
        self.apply_fold_click(click)
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
                    paint_overlay_line(&self.model.lines[index], self.number_width, None)
                }
                VisibleItem::FoldEdge(edge) => {
                    if overlay_index >= edge.start && overlay_index < edge.end {
                        return visual;
                    }
                    expand_row(edge, self.number_width).0
                }
            };
            visual += wrap::wrap_line_with_gutter(&line, width.max(1), self.gutter_width).len();
        }
        visual
    }

    pub(crate) fn fold_hit_at(
        &self,
        visual_row: usize,
        column: u16,
        width: usize,
    ) -> Option<FoldClick> {
        let mut visual = 0;
        for item in self.visible_items() {
            let (line, buttons) = match item {
                VisibleItem::Line(index) => (
                    paint_overlay_line(&self.model.lines[index], self.number_width, None),
                    Vec::new(),
                ),
                VisibleItem::FoldEdge(edge) => expand_row(edge, self.number_width),
            };
            let height = wrap::wrap_line_with_gutter(&line, width.max(1), self.gutter_width)
                .len()
                .max(1);
            if visual_row >= visual && visual_row < visual + height {
                let VisibleItem::FoldEdge(edge) = item else {
                    return None;
                };
                if visual_row == visual {
                    if let Some(button) = buttons.iter().find(|button| {
                        column >= button.x && column < button.x.saturating_add(button.width)
                    }) {
                        return Some(FoldClick::Direction {
                            id: edge.id,
                            direction: button.direction,
                        });
                    }
                }
                return fold_click(edge);
            }
            visual += height;
        }
        None
    }

    pub(crate) fn apply_fold_click(&mut self, click: FoldClick) -> bool {
        match click {
            FoldClick::Direction { id, direction } => self.expand_edge(id, direction),
            FoldClick::Both { id } => self.expand_both(id),
        }
    }
}

fn fold_click(edge: FoldEdge) -> Option<FoldClick> {
    match (edge.expand_down, edge.expand_up) {
        (true, true) => Some(FoldClick::Both { id: edge.id }),
        (true, false) => Some(FoldClick::Direction {
            id: edge.id,
            direction: ExpandDirection::Down,
        }),
        (false, true) => Some(FoldClick::Direction {
            id: edge.id,
            direction: ExpandDirection::Up,
        }),
        (false, false) => None,
    }
}

pub(crate) fn stats_spans(diff: &FileDiff) -> Vec<Span<'static>> {
    vec![
        Span::styled(format!("+{}", diff.added), Style::default().fg(ADD_FG)),
        Span::raw(" "),
        Span::styled(format!("-{}", diff.removed), Style::default().fg(DEL_FG)),
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
                .fg(theme::KEYCAP_FG.tui())
                .bg(theme::KEYCAP_BG.tui())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(theme::MUTED.tui())
                .bg(theme::FOLD_ROW_BG.tui())
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
        if matches!(line.kind, OverlayKind::Skipped) {
            continue;
        }
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

fn number_cell(number: Option<usize>, width: usize) -> String {
    match number {
        Some(number) => format!("{number:>width$}"),
        None => " ".repeat(width),
    }
}

fn paint_overlay_line(
    line: &OverlayLine,
    number_width: usize,
    syntax_line: Option<Line<'static>>,
) -> Line<'static> {
    let (old_no, new_no, mark, line_bg, mark_fg) = match line.kind {
        OverlayKind::Insert => (None, line.new_no, '+', Some(ADD_LINE_BG), ADD_FG),
        OverlayKind::Delete => (line.old_no, None, '-', Some(DEL_LINE_BG), DEL_FG),
        OverlayKind::Context | OverlayKind::Skipped => {
            (line.old_no, line.new_no, ' ', None, Color::DarkGray)
        }
    };
    let gutter = format!(
        "{} {} {mark}",
        number_cell(old_no, number_width),
        number_cell(new_no, number_width)
    );
    let gutter_style = match line.kind {
        OverlayKind::Insert => Style::default().fg(mark_fg).bg(ADD_LINE_BG),
        OverlayKind::Delete => Style::default().fg(mark_fg).bg(DEL_LINE_BG),
        OverlayKind::Context | OverlayKind::Skipped => Style::default().fg(Color::DarkGray),
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
        OverlayKind::Context | OverlayKind::Skipped => None,
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
                OverlayKind::Insert => Style::default().fg(ADD_FG),
                OverlayKind::Delete => Style::default().fg(DEL_FG),
                OverlayKind::Context | OverlayKind::Skipped => Style::default(),
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

fn expand_row(edge: FoldEdge, number_width: usize) -> (Line<'static>, Vec<FoldButton>) {
    let blank = " ".repeat(number_width);
    let gutter = format!("{blank} {blank} {ELLIPSIS}");
    let mut x = gutter.width() as u16;
    let mut spans = vec![Span::styled(
        gutter,
        Style::default().fg(FOLD_FG).add_modifier(Modifier::DIM),
    )];
    let mut buttons = Vec::new();

    let count = if edge.expand_down || edge.expand_up {
        format!(" {} hidden  click or Enter ", edge.remaining)
    } else {
        format!(" {} unchanged lines omitted ", edge.remaining)
    };
    x += count.width() as u16;
    spans.push(Span::styled(count, Style::default().fg(FOLD_FG)));

    let mut push_button = |label: &str, direction: ExpandDirection, x: &mut u16| {
        let text = format!(" {label} ");
        let width = text.width() as u16;
        buttons.push(FoldButton {
            x: *x,
            width,
            direction,
        });
        spans.push(Span::styled(
            text,
            Style::default()
                .fg(Color::Black)
                .bg(FOLD_FG)
                .add_modifier(Modifier::BOLD),
        ));
        *x += width;
        spans.push(Span::raw(" "));
        *x += 1;
    };

    if edge.expand_down {
        push_button(BTN_DOWN, ExpandDirection::Down, &mut x);
    }
    if edge.expand_up {
        push_button(BTN_UP, ExpandDirection::Up, &mut x);
    }

    let mut line = Line::from(spans);
    line.style = Style::default().bg(FOLD_ROW_BG).fg(FOLD_FG);
    (line, buttons)
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
            text.contains(ELLIPSIS) && (text.contains("▲") || text.contains("▼"))
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
        let mut click_view = DiffViewState::new("a.cpp".into(), view.model.clone(), None);
        let body = click_view.body_lines();
        assert!(body.iter().any(|line| texts(line).contains(ELLIPSIS)));
        assert!(body.iter().any(|line| texts(line).contains("▲")));
        assert!(body.iter().any(|line| texts(line).contains("▼")));
        let leading_row = body
            .iter()
            .position(|line| texts(line).contains("▲ 20"))
            .expect("leading fold row");
        let click = click_view
            .fold_hit_at(leading_row, 0, 80)
            .expect("clickable fold");
        assert!(matches!(
            click,
            FoldClick::Direction {
                id,
                direction: ExpandDirection::Up
            } if id == leading.id
        ));

        let before_click = click_view
            .visible_items()
            .iter()
            .filter(|item| matches!(item, VisibleItem::Line(_)))
            .count();
        assert!(click_view.apply_fold_click(click));
        let after_click = click_view
            .visible_items()
            .iter()
            .filter(|item| matches!(item, VisibleItem::Line(_)))
            .count();
        assert_eq!(after_click, before_click + EXPAND_CHUNK);
    }

    #[test]
    fn dual_gutters_keep_old_and_new_numbers_across_edits() {
        let model = build_file_diff(
            &["new".into(), "kept".into()],
            &[
                "@@ -1,2 +1,2 @@".into(),
                "-old".into(),
                "+new".into(),
                " kept".into(),
            ],
            Some(FileDiffKind::Edit),
            5,
        );
        let view = DiffViewState::new("a.rs".into(), model, None);
        let lines = view.body_lines();
        assert!(
            texts(&lines[0]).starts_with(" 1    -"),
            "delete keeps old number only: {}",
            texts(&lines[0])
        );
        assert!(
            texts(&lines[1]).starts_with("    1 +"),
            "insert keeps new number only: {}",
            texts(&lines[1])
        );
        assert!(
            texts(&lines[2]).starts_with(" 2  2  "),
            "context shows both numbers: {}",
            texts(&lines[2])
        );
    }

    #[test]
    fn folded_gap_between_hunks_renders_an_ellipsis_separator() {
        let mut file: Vec<String> = (1..=40).map(|index| format!("line-{index}")).collect();
        file[4] = "first".into();
        file[34] = "second".into();
        let diff = vec![
            "@@ -4,3 +4,3 @@".into(),
            " line-4".into(),
            "-line-5".into(),
            "+first".into(),
            " line-6".into(),
            "@@ -34,3 +34,3 @@".into(),
            " line-34".into(),
            "-line-35".into(),
            "+second".into(),
            " line-36".into(),
        ];
        let model = build_file_diff(&file, &diff, Some(FileDiffKind::Edit), 5);
        let mut view = DiffViewState::new("a.cpp".into(), model, None);
        let body: Vec<String> = view.body_lines().iter().map(texts).collect();
        assert!(
            body.iter().any(|line| line.contains(ELLIPSIS)),
            "expected ⋯ between hunks, got {body:?}"
        );
        let fold_row = body
            .iter()
            .find(|line| line.contains(ELLIPSIS))
            .expect("fold");
        assert!(fold_row.contains("▼ 20") && fold_row.contains("▲ 20"));
        assert!(view.expand_visible(0, 80, 80));
        let expanded: Vec<String> = view.body_lines().iter().map(texts).collect();
        assert!(
            expanded.iter().any(|line| line.contains("line-20")),
            "Enter/click should reveal more context, got {expanded:?}"
        );
    }

    #[test]
    fn concatenated_unified_hunks_without_file_still_show_ellipsis() {
        let diff = vec![
            "@@ -2,3 +2,3 @@".into(),
            " a".into(),
            "-b".into(),
            "+B".into(),
            " c".into(),
            "@@ -30,3 +30,3 @@".into(),
            " x".into(),
            "-y".into(),
            "+Y".into(),
            " z".into(),
        ];
        let model = build_file_diff(&[], &diff, Some(FileDiffKind::Edit), 5);
        let mut view = DiffViewState::new("a.rs".into(), model, None);
        let body: Vec<String> = view.body_lines().iter().map(texts).collect();
        assert!(
            body.iter().any(|line| line.contains(ELLIPSIS)),
            "expected ⋯ between concatenated hunks, got {body:?}"
        );
        let separator = body
            .iter()
            .find(|line| line.contains(ELLIPSIS))
            .expect("separator");
        assert!(separator.contains("unchanged lines omitted"));
        assert!(!separator.contains("click or Enter"));
        assert!(!separator.contains(BTN_DOWN));
        assert!(!separator.contains(BTN_UP));
        assert!(!view.expand_visible(0, 80, 80));
    }
}
