//! File-as-canvas diff model: overlay lines, fold ranges, and intra-line spans.
//!
//! The Content pane renders this model. Parsing is independent of Ratatui and
//! of `p4`, so fixtures can cover add/delete/edit, folding, and word diffs.

use super::FileAction;

/// Unchanged lines kept visible on each side of a fold. `0` disables folding.
pub const DEFAULT_FOLD_CONTEXT: usize = 5;
/// Rejected above this in panel.json so a typo cannot disable folding forever.
pub const MAX_FOLD_CONTEXT: usize = 200;
/// Do not fold a run unless at least this many lines would actually be hidden.
pub const MIN_FOLD_HIDDEN: usize = 4;
/// Extra unchanged lines revealed by one click on a hunk-adjacent expand row.
pub const EXPAND_CHUNK: usize = 20;
/// Bound the intra-line DP so a 2k-character minified line cannot explode memory.
const MAX_INTRA_TOKENS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileDiffKind {
    Edit,
    Add,
    Delete,
}

impl FileDiffKind {
    #[must_use]
    pub fn from_action(action: &FileAction) -> Self {
        match action {
            FileAction::Add | FileAction::MoveAdd | FileAction::Branch => Self::Add,
            FileAction::Delete
            | FileAction::MoveDelete
            | FileAction::Purge
            | FileAction::Archive => Self::Delete,
            _ => Self::Edit,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    Context,
    Insert,
    Delete,
    /// A skipped unchanged run between unified hunks when the workspace file
    /// is not available to fill those lines. Rendered as an ellipsis separator.
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntraSpan {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayLine {
    pub kind: OverlayKind,
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    pub text: String,
    pub intra: Vec<IntraSpan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldRange {
    pub id: usize,
    pub start: usize,
    pub end: usize,
    pub expandable: bool,
}

impl FoldRange {
    #[must_use]
    pub fn hidden(&self) -> usize {
        self.end.saturating_sub(self.start)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub lines: Vec<OverlayLine>,
    pub folds: Vec<FoldRange>,
    pub hunks: Vec<usize>,
    pub added: usize,
    pub removed: usize,
    pub truncated: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UnifiedHunk {
    old_start: usize,
    new_start: usize,
    lines: Vec<HunkLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HunkLine {
    Context(String),
    Delete(String),
    Insert(String),
}

/// Build a file-as-canvas overlay from the workspace text and `p4 diff -du`.
#[must_use]
pub fn build_file_diff(
    new_lines: &[String],
    unified: &[String],
    kind: Option<FileDiffKind>,
    fold_context: usize,
) -> FileDiff {
    let (hunks, truncated) = parse_unified_hunks(unified);
    let mut lines = if hunks.is_empty() {
        if truncated.is_some()
            && !matches!(kind, Some(FileDiffKind::Add) | Some(FileDiffKind::Delete))
        {
            Vec::new()
        } else {
            overlay_without_hunks(new_lines, kind)
        }
    } else if new_lines.is_empty() {
        overlay_from_hunks(&hunks)
    } else {
        overlay_onto_file(new_lines, &hunks, truncated.is_none())
    };
    lines = fill_line_number_gaps(lines, new_lines);
    apply_intra_line(&mut lines);
    let added = count_kind(&lines, OverlayKind::Insert);
    let removed = count_kind(&lines, OverlayKind::Delete);
    let hunk_starts = change_hunk_starts(&lines);
    let folds = fold_ranges(&lines, fold_context);
    FileDiff {
        lines,
        folds,
        hunks: hunk_starts,
        added,
        removed,
        truncated,
    }
}

fn overlay_without_hunks(new_lines: &[String], kind: Option<FileDiffKind>) -> Vec<OverlayLine> {
    match kind {
        Some(FileDiffKind::Add) => new_lines
            .iter()
            .enumerate()
            .map(|(index, text)| overlay(OverlayKind::Insert, None, Some(index + 1), text))
            .collect(),
        Some(FileDiffKind::Delete) => new_lines
            .iter()
            .enumerate()
            .map(|(index, text)| overlay(OverlayKind::Delete, Some(index + 1), None, text))
            .collect(),
        Some(FileDiffKind::Edit) | None => new_lines
            .iter()
            .enumerate()
            .map(|(index, text)| {
                overlay(OverlayKind::Context, Some(index + 1), Some(index + 1), text)
            })
            .collect(),
    }
}

fn overlay_onto_file(
    new_lines: &[String],
    hunks: &[UnifiedHunk],
    fill_trailing: bool,
) -> Vec<OverlayLine> {
    let mut output = Vec::new();
    let mut new_i = 0usize;
    let mut old_i = 0usize;
    for hunk in hunks {
        let new_target = hunk.new_start.saturating_sub(1);
        while new_i < new_target && new_i < new_lines.len() {
            old_i += 1;
            new_i += 1;
            output.push(overlay(
                OverlayKind::Context,
                Some(old_i),
                Some(new_i),
                &new_lines[new_i - 1],
            ));
        }
        // The workspace preview is byte/line bounded. If this hunk starts
        // beyond the captured prefix, advance to the line number declared by
        // the unified header instead of numbering the hunk from the preview
        // boundary.
        new_i = new_i.max(new_target);
        old_i = hunk.old_start.saturating_sub(1);
        for line in &hunk.lines {
            match line {
                HunkLine::Context(text) => {
                    old_i += 1;
                    new_i += 1;
                    let text = new_lines
                        .get(new_i - 1)
                        .map_or(text.as_str(), String::as_str);
                    output.push(overlay(
                        OverlayKind::Context,
                        Some(old_i),
                        Some(new_i),
                        text,
                    ));
                }
                HunkLine::Insert(text) => {
                    new_i += 1;
                    let text = new_lines
                        .get(new_i - 1)
                        .map_or(text.as_str(), String::as_str);
                    output.push(overlay(OverlayKind::Insert, None, Some(new_i), text));
                }
                HunkLine::Delete(text) => {
                    old_i += 1;
                    output.push(overlay(OverlayKind::Delete, Some(old_i), None, text));
                }
            }
        }
    }
    if fill_trailing {
        while new_i < new_lines.len() {
            old_i += 1;
            new_i += 1;
            output.push(overlay(
                OverlayKind::Context,
                Some(old_i),
                Some(new_i),
                &new_lines[new_i - 1],
            ));
        }
    }
    output
}

fn overlay_from_hunks(hunks: &[UnifiedHunk]) -> Vec<OverlayLine> {
    let mut output = Vec::new();
    let mut old_i;
    let mut new_i;
    for hunk in hunks {
        old_i = hunk.old_start.saturating_sub(1);
        new_i = hunk.new_start.saturating_sub(1);
        for line in &hunk.lines {
            match line {
                HunkLine::Context(text) => {
                    old_i += 1;
                    new_i += 1;
                    output.push(overlay(
                        OverlayKind::Context,
                        Some(old_i),
                        Some(new_i),
                        text,
                    ));
                }
                HunkLine::Insert(text) => {
                    new_i += 1;
                    output.push(overlay(OverlayKind::Insert, None, Some(new_i), text));
                }
                HunkLine::Delete(text) => {
                    old_i += 1;
                    output.push(overlay(OverlayKind::Delete, Some(old_i), None, text));
                }
            }
        }
    }
    output
}

fn overlay(
    kind: OverlayKind,
    old_no: Option<usize>,
    new_no: Option<usize>,
    text: &str,
) -> OverlayLine {
    OverlayLine {
        kind,
        old_no,
        new_no,
        text: text.to_owned(),
        intra: Vec::new(),
    }
}

fn count_kind(lines: &[OverlayLine], kind: OverlayKind) -> usize {
    lines.iter().filter(|line| line.kind == kind).count()
}

fn change_hunk_starts(lines: &[OverlayLine]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut previous_change = false;
    for (index, line) in lines.iter().enumerate() {
        let is_change = matches!(line.kind, OverlayKind::Insert | OverlayKind::Delete);
        if is_change && !previous_change {
            starts.push(index);
        }
        previous_change = is_change;
    }
    starts
}

fn fill_line_number_gaps(lines: Vec<OverlayLine>, new_lines: &[String]) -> Vec<OverlayLine> {
    if lines.len() < 2 {
        return lines;
    }
    let mut output = Vec::with_capacity(lines.len().saturating_add(new_lines.len()));
    for line in lines {
        if let Some(previous) = output.last().cloned() {
            let gap = numbered_gap(&previous, &line);
            if gap >= MIN_FOLD_HIDDEN {
                if new_lines.is_empty() {
                    output.push(skipped_overlay(&previous, gap));
                } else if let Some(after) = previous.new_no {
                    for number in (after + 1)..after.saturating_add(gap + 1) {
                        if number == 0 || number > new_lines.len() {
                            break;
                        }
                        if line.new_no == Some(number) {
                            break;
                        }
                        let old_no = previous.old_no.map(|old| old + (number - after));
                        output.push(overlay(
                            OverlayKind::Context,
                            old_no,
                            Some(number),
                            &new_lines[number - 1],
                        ));
                    }
                } else {
                    output.push(skipped_overlay(&previous, gap));
                }
            }
        }
        output.push(line);
    }
    output
}

fn numbered_gap(previous: &OverlayLine, next: &OverlayLine) -> usize {
    match (previous.new_no, next.new_no) {
        (Some(from), Some(to)) if to > from + 1 => to - from - 1,
        _ => match (previous.old_no, next.old_no) {
            (Some(from), Some(to)) if to > from + 1 => to - from - 1,
            _ => 0,
        },
    }
}

fn skipped_overlay(previous: &OverlayLine, hidden: usize) -> OverlayLine {
    OverlayLine {
        kind: OverlayKind::Skipped,
        old_no: previous.old_no.map(|old| old + 1),
        new_no: previous.new_no.map(|new| new + 1),
        text: hidden.to_string(),
        intra: Vec::new(),
    }
}

fn fold_ranges(lines: &[OverlayLine], fold_context: usize) -> Vec<FoldRange> {
    if fold_context == 0 || lines.is_empty() {
        return Vec::new();
    }
    let mut runs = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if lines[index].kind != OverlayKind::Context {
            index += 1;
            continue;
        }
        let start = index;
        while index < lines.len() && lines[index].kind == OverlayKind::Context {
            index += 1;
        }
        runs.push((start, index));
    }

    let mut folds = Vec::new();
    for (start, end) in runs {
        let at_start = start == 0;
        let at_end = end == lines.len();
        let keep_before = if at_start { 0 } else { fold_context };
        let keep_after = if at_end { 0 } else { fold_context };
        let hidden_start = start + keep_before;
        let hidden_end = end.saturating_sub(keep_after);
        if hidden_end.saturating_sub(hidden_start) < MIN_FOLD_HIDDEN {
            continue;
        }
        if hidden_start >= hidden_end {
            continue;
        }
        let id = folds.len();
        folds.push(FoldRange {
            id,
            start: hidden_start,
            end: hidden_end,
            expandable: true,
        });
    }
    folds
}

fn apply_intra_line(lines: &mut [OverlayLine]) {
    let mut index = 0;
    while index < lines.len() {
        if lines[index].kind != OverlayKind::Delete {
            index += 1;
            continue;
        }
        let delete_start = index;
        while index < lines.len() && lines[index].kind == OverlayKind::Delete {
            index += 1;
        }
        let delete_end = index;
        let insert_start = index;
        while index < lines.len() && lines[index].kind == OverlayKind::Insert {
            index += 1;
        }
        let insert_end = index;
        let paired = (delete_end - delete_start).min(insert_end - insert_start);
        for offset in 0..paired {
            let old_text = lines[delete_start + offset].text.clone();
            let new_text = lines[insert_start + offset].text.clone();
            let (old_spans, new_spans) = intra_spans(&old_text, &new_text);
            lines[delete_start + offset].intra = old_spans;
            lines[insert_start + offset].intra = new_spans;
        }
    }
}

fn intra_spans(old: &str, new: &str) -> (Vec<IntraSpan>, Vec<IntraSpan>) {
    if old == new || old.is_empty() || new.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let old_tokens = tokenize(old);
    let new_tokens = tokenize(new);
    if old_tokens.len() > MAX_INTRA_TOKENS || new_tokens.len() > MAX_INTRA_TOKENS {
        return (Vec::new(), Vec::new());
    }
    let old_values: Vec<&str> = old_tokens
        .iter()
        .map(|&(start, end)| &old[start..end])
        .collect();
    let new_values: Vec<&str> = new_tokens
        .iter()
        .map(|&(start, end)| &new[start..end])
        .collect();
    let (old_keep, new_keep) = token_lcs_keep(&old_values, &new_values);
    let meaningful = |token: &str| {
        token
            .chars()
            .any(|character| character.is_ascii_alphanumeric())
    };
    let kept = old_values
        .iter()
        .zip(old_keep.iter())
        .filter(|(token, keep)| **keep && meaningful(token))
        .count();
    let shorter = old_values
        .iter()
        .filter(|token| meaningful(token))
        .count()
        .min(new_values.iter().filter(|token| meaningful(token)).count())
        .max(1);
    if kept * 2 < shorter {
        return (Vec::new(), Vec::new());
    }
    (
        spans_from_changed(&old_tokens, &old_keep),
        spans_from_changed(&new_tokens, &new_keep),
    )
}

fn spans_from_changed(tokens: &[(usize, usize)], keep: &[bool]) -> Vec<IntraSpan> {
    let mut spans: Vec<IntraSpan> = Vec::new();
    for (index, &(start, end)) in tokens.iter().enumerate() {
        if keep[index] {
            continue;
        }
        if let Some(last) = spans.last_mut()
            && last.end == start
        {
            last.end = end;
        } else {
            spans.push(IntraSpan { start, end });
        }
    }
    spans
}

fn tokenize(text: &str) -> Vec<(usize, usize)> {
    let mut tokens = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, character)) = chars.next() {
        let mut end = start + character.len_utf8();
        if is_word_char(character) {
            while let Some((_, next)) = chars.peek() {
                if !is_word_char(*next) {
                    break;
                }
                let (next_start, next_char) = chars.next().expect("peeked");
                end = next_start + next_char.len_utf8();
            }
        } else if character.is_whitespace() {
            while let Some((_, next)) = chars.peek() {
                if !next.is_whitespace() {
                    break;
                }
                let (next_start, next_char) = chars.next().expect("peeked");
                end = next_start + next_char.len_utf8();
            }
        }
        tokens.push((start, end));
    }
    tokens
}

fn is_word_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

fn token_lcs_keep(old: &[&str], new: &[&str]) -> (Vec<bool>, Vec<bool>) {
    let rows = old.len();
    let cols = new.len();
    let mut table = vec![0u16; (rows + 1) * (cols + 1)];
    let at = |row: usize, col: usize| row * (cols + 1) + col;
    for row in 0..rows {
        for col in 0..cols {
            table[at(row + 1, col + 1)] = if old[row] == new[col] {
                table[at(row, col)] + 1
            } else {
                table[at(row + 1, col)].max(table[at(row, col + 1)])
            };
        }
    }
    let mut old_keep = vec![false; rows];
    let mut new_keep = vec![false; cols];
    let mut row = rows;
    let mut col = cols;
    while row > 0 && col > 0 {
        if old[row - 1] == new[col - 1] {
            old_keep[row - 1] = true;
            new_keep[col - 1] = true;
            row -= 1;
            col -= 1;
        } else if table[at(row - 1, col)] >= table[at(row, col - 1)] {
            row -= 1;
        } else {
            col -= 1;
        }
    }
    (old_keep, new_keep)
}

fn parse_unified_hunks(lines: &[String]) -> (Vec<UnifiedHunk>, Option<String>) {
    let mut hunks = Vec::new();
    let mut truncated = None;
    let mut index = 0;
    while index < lines.len() {
        if is_truncation_marker(&lines[index]) {
            truncated = Some(lines[index].clone());
            break;
        }
        let Some((old_start, old_count, new_start, new_count)) = parse_hunk_header(&lines[index])
        else {
            index += 1;
            continue;
        };
        index += 1;
        let mut hunk_lines = Vec::new();
        let mut seen_old = 0usize;
        let mut seen_new = 0usize;
        while index < lines.len() && (seen_old < old_count || seen_new < new_count) {
            let raw = &lines[index];
            if is_truncation_marker(raw) {
                truncated = Some(raw.clone());
                break;
            }
            if raw.starts_with("@@") {
                break;
            }
            if let Some(line) = parse_hunk_line(raw) {
                match line {
                    HunkLine::Context(_) => {
                        seen_old += 1;
                        seen_new += 1;
                    }
                    HunkLine::Delete(_) => seen_old += 1,
                    HunkLine::Insert(_) => seen_new += 1,
                }
                hunk_lines.push(line);
            }
            index += 1;
        }
        hunks.push(UnifiedHunk {
            old_start,
            new_start,
            lines: hunk_lines,
        });
        if truncated.is_some() {
            break;
        }
    }
    (hunks, truncated)
}

fn parse_hunk_header(line: &str) -> Option<(usize, usize, usize, usize)> {
    let rest = line.strip_prefix("@@")?;
    let (body, _) = rest.split_once("@@")?;
    let mut parts = body.split_whitespace();
    let old = parse_range(parts.next()?.strip_prefix('-')?)?;
    let new = parse_range(parts.next()?.strip_prefix('+')?)?;
    Some((old.0, old.1, new.0, new.1))
}

fn parse_range(value: &str) -> Option<(usize, usize)> {
    match value.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((value.parse().ok()?, 1)),
    }
}

fn parse_hunk_line(line: &str) -> Option<HunkLine> {
    if line.starts_with('\\') {
        return None;
    }
    match line.as_bytes().first() {
        Some(b'+') => Some(HunkLine::Insert(line[1..].to_owned())),
        Some(b'-') => Some(HunkLine::Delete(line[1..].to_owned())),
        Some(b' ') => Some(HunkLine::Context(line[1..].to_owned())),
        _ => None,
    }
}

fn is_truncation_marker(line: &str) -> bool {
    line.starts_with("truncated:")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn unified(body: &str) -> Vec<String> {
        body.lines().map(ToOwned::to_owned).collect()
    }

    #[test]
    fn file_canvas_keeps_unchanged_lines_and_inserts_deletes_in_place() {
        let file = lines(&[
            "alpha", "bravo", "changed", "inserted", "delta", "echo", "foxtrot", "golf",
        ]);
        let diff = unified(
            "--- a/file\n+++ b/file\n@@ -2,4 +2,5 @@\n bravo\n-charlie\n+changed\n+inserted\n delta\n echo\n",
        );
        let model = build_file_diff(&file, &diff, Some(FileDiffKind::Edit), 5);
        let kinds: Vec<_> = model
            .lines
            .iter()
            .map(|line| (line.kind, line.text.as_str(), line.new_no, line.old_no))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (OverlayKind::Context, "alpha", Some(1), Some(1)),
                (OverlayKind::Context, "bravo", Some(2), Some(2)),
                (OverlayKind::Delete, "charlie", None, Some(3)),
                (OverlayKind::Insert, "changed", Some(3), None),
                (OverlayKind::Insert, "inserted", Some(4), None),
                (OverlayKind::Context, "delta", Some(5), Some(4)),
                (OverlayKind::Context, "echo", Some(6), Some(5)),
                (OverlayKind::Context, "foxtrot", Some(7), Some(6)),
                (OverlayKind::Context, "golf", Some(8), Some(7)),
            ]
        );
        assert_eq!(model.added, 2);
        assert_eq!(model.removed, 1);
        assert_eq!(model.hunks, vec![2]);
    }

    #[test]
    fn hunk_after_bounded_workspace_prefix_keeps_declared_new_line_numbers() {
        let file = lines(&["preview-1", "preview-2"]);
        let diff = unified("@@ -10,2 +10,4 @@\n old-10\n+insert-a\n+insert-b\n old-11\n");
        let model = build_file_diff(&file, &diff, Some(FileDiffKind::Edit), 5);
        let inserts = model
            .lines
            .iter()
            .filter(|line| line.kind == OverlayKind::Insert)
            .map(|line| (line.new_no, line.text.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            inserts,
            vec![(Some(11), "insert-a"), (Some(12), "insert-b")]
        );
    }

    #[test]
    fn distant_hunks_without_file_bytes_still_insert_a_skip_separator() {
        let diff = unified("@@ -2,3 +2,3 @@\n a\n-b\n+B\n c\n@@ -30,3 +30,3 @@\n x\n-y\n+Y\n z\n");
        let model = build_file_diff(&[], &diff, Some(FileDiffKind::Edit), 5);
        assert!(
            model
                .lines
                .iter()
                .any(|line| line.kind == OverlayKind::Skipped),
            "hunks concatenated without a skip marker: {:?}",
            model
                .lines
                .iter()
                .map(|line| (line.kind, line.new_no, line.text.as_str()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn distant_unchanged_runs_fold_and_stay_expandable() {
        let mut file = Vec::new();
        for index in 1..=30 {
            file.push(format!("line-{index}"));
        }
        file[14] = "changed".into();
        let mut diff = lines(&["--- a/f", "+++ b/f", "@@ -13,5 +13,5 @@"]);
        diff.extend(lines(&[
            " line-13", " line-14", "-line-15", "+changed", " line-16", " line-17",
        ]));
        let model = build_file_diff(&file, &diff, Some(FileDiffKind::Edit), 5);
        assert_eq!(model.folds.len(), 2);
        assert!(model.folds.iter().all(|fold| fold.expandable));
        assert_eq!(model.folds[0].start, 0);
        assert!(model.folds[0].end >= MIN_FOLD_HIDDEN);
        assert!(model.folds[1].hidden() >= MIN_FOLD_HIDDEN);
    }

    #[test]
    fn fold_context_zero_disables_folding() {
        let file: Vec<String> = (1..=40).map(|index| format!("l{index}")).collect();
        let diff = unified("@@ -1,3 +1,3 @@\n l1\n-l2\n+xx\n l3\n");
        let model = build_file_diff(&file, &diff, None, 0);
        assert!(model.folds.is_empty());
    }

    #[test]
    fn short_unchanged_gaps_are_not_folded() {
        let file = lines(&["a", "b", "c", "d", "e", "f"]);
        let diff = unified("@@ -2,3 +2,3 @@\n b\n-c\n+C\n d\n");
        let model = build_file_diff(&file, &diff, None, 5);
        assert!(model.folds.is_empty());
    }

    #[test]
    fn intra_line_marks_replaced_tokens_on_paired_lines() {
        let file = lines(&["CurrentHealth -= Applied;"]);
        let diff = unified("@@ -1 +1 @@\n-CurrentHealth -= Amount;\n+CurrentHealth -= Applied;\n");
        let model = build_file_diff(&file, &diff, Some(FileDiffKind::Edit), 5);
        assert_eq!(model.lines.len(), 2);
        assert_eq!(model.lines[0].kind, OverlayKind::Delete);
        assert_eq!(model.lines[1].kind, OverlayKind::Insert);
        let deleted: Vec<&str> = model.lines[0]
            .intra
            .iter()
            .map(|span| &model.lines[0].text[span.start..span.end])
            .collect();
        let inserted: Vec<&str> = model.lines[1]
            .intra
            .iter()
            .map(|span| &model.lines[1].text[span.start..span.end])
            .collect();
        assert_eq!(deleted, vec!["Amount"]);
        assert_eq!(inserted, vec!["Applied"]);
    }

    #[test]
    fn unrelated_replacement_skips_intra_line_noise() {
        let file = lines(&["fn next() -> i32 { 1 }"]);
        let diff = unified("@@ -1 +1 @@\n-struct TotallyDifferent {}\n+fn next() -> i32 { 1 }\n");
        let model = build_file_diff(&file, &diff, None, 5);
        assert!(model.lines[0].intra.is_empty());
        assert!(model.lines[1].intra.is_empty());
    }

    #[test]
    fn add_without_hunks_is_all_inserts() {
        let file = lines(&["one", "two"]);
        let model = build_file_diff(&file, &[], Some(FileDiffKind::Add), 5);
        assert!(
            model
                .lines
                .iter()
                .all(|line| line.kind == OverlayKind::Insert)
        );
        assert_eq!(model.added, 2);
        assert_eq!(model.removed, 0);
    }

    #[test]
    fn delete_without_hunks_is_all_deletes() {
        let file = lines(&["gone"]);
        let model = build_file_diff(&file, &[], Some(FileDiffKind::Delete), 5);
        assert_eq!(model.lines[0].kind, OverlayKind::Delete);
        assert_eq!(model.removed, 1);
    }

    #[test]
    fn missing_workspace_file_still_renders_hunks() {
        let diff = unified("@@ -1,2 +1,2 @@\n context\n-old\n+new\n");
        let model = build_file_diff(&[], &diff, Some(FileDiffKind::Edit), 5);
        assert_eq!(model.lines.len(), 3);
        assert_eq!(model.lines[1].kind, OverlayKind::Delete);
        assert_eq!(model.lines[2].kind, OverlayKind::Insert);
    }

    #[test]
    fn p4_headers_and_truncation_markers_are_ignored() {
        let diff = unified(
            "==== //depot/a#1 - C:\\ws\\a ====\n--- //depot/a\t#1\n+++ C:\\ws\\a\n@@ -1 +1 @@\n-old\n+new\ntruncated: 4000 line diff budget exceeded\n",
        );
        let model = build_file_diff(&lines(&["new"]), &diff, None, 5);
        assert_eq!(model.added, 1);
        assert_eq!(model.removed, 1);
        assert_eq!(model.lines[1].text, "new");
        assert_eq!(
            model.truncated.as_deref(),
            Some("truncated: 4000 line diff budget exceeded")
        );
    }

    #[test]
    fn hunk_lines_that_look_like_file_headers_stay_in_the_hunk() {
        let file = lines(&["a", "++", "b"]);
        let diff = unified("@@ -1,4 +1,3 @@\n a\n---\n----\n+++\n b\n");
        let model = build_file_diff(&file, &diff, Some(FileDiffKind::Edit), 5);
        let kinds: Vec<_> = model
            .lines
            .iter()
            .map(|line| (line.kind, line.text.as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (OverlayKind::Context, "a"),
                (OverlayKind::Delete, "--"),
                (OverlayKind::Delete, "---"),
                (OverlayKind::Insert, "++"),
                (OverlayKind::Context, "b"),
            ]
        );
        assert!(model.truncated.is_none());
    }

    #[test]
    fn truncated_unified_diff_does_not_treat_unparsed_tail_as_unchanged() {
        let file = lines(&["a", "B", "c", "later-change", "tail"]);
        let diff =
            unified("@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\ntruncated: 4000 line diff budget exceeded\n");
        let model = build_file_diff(&file, &diff, Some(FileDiffKind::Edit), 5);
        assert!(model.truncated.is_some());
        assert!(
            !model
                .lines
                .iter()
                .any(|line| line.text == "later-change" || line.text == "tail"),
            "unparsed tail must not be painted as context: {:?}",
            model
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(model.added, 1);
        assert_eq!(model.removed, 1);
    }

    #[test]
    fn kind_maps_from_p4_actions() {
        assert_eq!(
            FileDiffKind::from_action(&FileAction::MoveAdd),
            FileDiffKind::Add
        );
        assert_eq!(
            FileDiffKind::from_action(&FileAction::MoveDelete),
            FileDiffKind::Delete
        );
        assert_eq!(
            FileDiffKind::from_action(&FileAction::Integrate),
            FileDiffKind::Edit
        );
    }
}
