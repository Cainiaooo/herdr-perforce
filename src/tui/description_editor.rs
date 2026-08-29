use crate::p4::MAX_DESCRIPTION_BYTES;

use super::display::char_width;

#[derive(Debug, Default)]
pub(super) enum DescriptionEditor {
    #[default]
    Idle,
    Loading {
        change: u64,
        request_id: u64,
    },
    Editing {
        change: u64,
        input: String,
        cursor: usize,
    },
    Applying {
        change: u64,
        input: String,
        cursor: usize,
        request_id: u64,
    },
}

impl DescriptionEditor {
    pub(super) fn is_active(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    pub(super) fn begin(&mut self, change: u64, description: &str) {
        let input = description.trim_end_matches(['\r', '\n']).to_owned();
        let cursor = input.len();
        *self = Self::Editing {
            change,
            input,
            cursor,
        };
    }

    pub(super) fn cancel(&mut self) -> bool {
        if matches!(self, Self::Applying { .. }) {
            return false;
        }
        *self = Self::Idle;
        true
    }

    pub(super) fn insert(&mut self, value: &str) -> Result<(), ()> {
        let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.contains('\0') {
            return Err(());
        }
        let Self::Editing { input, cursor, .. } = self else {
            return Err(());
        };
        if input.len().saturating_add(normalized.len()) > MAX_DESCRIPTION_BYTES {
            return Err(());
        }
        input.insert_str(*cursor, &normalized);
        *cursor += normalized.len();
        Ok(())
    }

    pub(super) fn backspace(&mut self) {
        let Self::Editing { input, cursor, .. } = self else {
            return;
        };
        let previous = previous_char_boundary(input, *cursor);
        if previous < *cursor {
            input.drain(previous..*cursor);
            *cursor = previous;
        }
    }

    pub(super) fn delete(&mut self) {
        let Self::Editing { input, cursor, .. } = self else {
            return;
        };
        let next = next_char_boundary(input, *cursor);
        if next > *cursor {
            input.drain(*cursor..next);
        }
    }

    pub(super) fn move_left(&mut self) {
        let Self::Editing { input, cursor, .. } = self else {
            return;
        };
        *cursor = previous_char_boundary(input, *cursor);
    }

    pub(super) fn move_right(&mut self) {
        let Self::Editing { input, cursor, .. } = self else {
            return;
        };
        *cursor = next_char_boundary(input, *cursor);
    }

    pub(super) fn move_home(&mut self) {
        let Self::Editing { input, cursor, .. } = self else {
            return;
        };
        *cursor = line_start(input, *cursor);
    }

    pub(super) fn move_end(&mut self) {
        let Self::Editing { input, cursor, .. } = self else {
            return;
        };
        *cursor = line_end(input, *cursor);
    }

    pub(super) fn move_vertical(&mut self, delta: isize, width: usize) {
        let Self::Editing { input, cursor, .. } = self else {
            return;
        };
        let layout = layout_description_text(input, *cursor, width);
        let target_line = if delta < 0 {
            let target = layout.cursor_line.saturating_sub(delta.unsigned_abs());
            if target == layout.cursor_line {
                return;
            }
            target
        } else {
            let target = layout.cursor_line.saturating_add(delta as usize);
            if target >= layout.lines.len() {
                return;
            }
            target
        };
        *cursor = cursor_at_visual_position(input, width.max(1), target_line, layout.cursor_column);
    }

    pub(super) fn text_for_change<'a>(
        &'a self,
        change: u64,
        fallback: &'a str,
    ) -> (&'a str, Option<usize>, Option<usize>) {
        match self {
            Self::Editing {
                change: editor_change,
                input,
                cursor,
            } if *editor_change == change => (input, Some(*cursor), Some(*cursor)),
            Self::Applying {
                change: editor_change,
                input,
                cursor,
                ..
            } if *editor_change == change => (input, None, Some(*cursor)),
            Self::Loading {
                change: editor_change,
                ..
            } if *editor_change == change => ("Loading full description...", None, None),
            _ => (fallback, None, None),
        }
    }

    pub(super) fn first_visible_line(&self, width: usize, visible_rows: usize) -> usize {
        let Self::Editing { input, cursor, .. } = self else {
            return 0;
        };
        layout_description_text(input, *cursor, width)
            .cursor_line
            .saturating_sub(visible_rows.saturating_sub(1))
    }

    pub(super) fn set_cursor_from_visual_position(
        &mut self,
        first_line: usize,
        visible_line: usize,
        column: usize,
        width: usize,
    ) {
        let Self::Editing { input, cursor, .. } = self else {
            return;
        };
        *cursor = cursor_at_visual_position(
            input,
            width.max(1),
            first_line.saturating_add(visible_line),
            column,
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct DescriptionTextLayout {
    pub(super) lines: Vec<String>,
    pub(super) cursor_line: usize,
    pub(super) cursor_column: usize,
}

pub(super) fn layout_description_text(
    text: &str,
    cursor: usize,
    width: usize,
) -> DescriptionTextLayout {
    let width = width.max(1);
    let cursor = cursor.min(text.len());
    let mut lines = vec![String::new()];
    let mut line = 0usize;
    let mut column = 0usize;
    let mut cursor_line = 0usize;
    let mut cursor_column = 0usize;
    let mut cursor_recorded = false;

    for (index, character) in text.char_indices() {
        if character != '\n' {
            let character_width = char_width(character).max(1);
            if column > 0 && column.saturating_add(character_width) > width {
                lines.push(String::new());
                line += 1;
                column = 0;
            }
        }
        if index == cursor {
            cursor_line = line;
            cursor_column = column;
            cursor_recorded = true;
        }
        if character == '\n' {
            lines.push(String::new());
            line += 1;
            column = 0;
            continue;
        }
        let character_width = char_width(character).max(1);
        if character == '\t' {
            lines[line].push_str("    ");
        } else {
            lines[line].push(character);
        }
        column = column.saturating_add(character_width);
    }
    if !cursor_recorded {
        cursor_line = line;
        cursor_column = column;
    }

    DescriptionTextLayout {
        lines,
        cursor_line,
        cursor_column,
    }
}

fn previous_char_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor.min(text.len())]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_char_boundary(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    text[cursor..]
        .chars()
        .next()
        .map_or(text.len(), |character| cursor + character.len_utf8())
}

fn line_start(text: &str, cursor: usize) -> usize {
    text[..cursor.min(text.len())]
        .rfind('\n')
        .map_or(0, |index| index + 1)
}

fn line_end(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    text[cursor..]
        .find('\n')
        .map_or(text.len(), |index| cursor + index)
}

fn cursor_at_visual_position(
    text: &str,
    width: usize,
    target_line: usize,
    target_column: usize,
) -> usize {
    let mut line = 0usize;
    let mut column = 0usize;
    let mut best: Option<(usize, usize)> = None;
    for (index, character) in text.char_indices() {
        if character != '\n' {
            let character_width = char_width(character).max(1);
            if column > 0 && column.saturating_add(character_width) > width {
                if line > target_line {
                    break;
                }
                line += 1;
                column = 0;
            }
        }
        if line == target_line {
            let distance = column.abs_diff(target_column);
            if best.is_none_or(|(best_distance, _)| distance <= best_distance) {
                best = Some((distance, index));
            }
        } else if line > target_line {
            break;
        }
        if character == '\n' {
            line += 1;
            column = 0;
        } else {
            column = column.saturating_add(char_width(character).max(1));
        }
    }
    if line == target_line {
        let distance = column.abs_diff(target_column);
        if best.is_none_or(|(best_distance, _)| distance <= best_distance) {
            best = Some((distance, text.len()));
        }
    }
    best.map_or(text.len(), |(_, index)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_mutations_keep_utf8_cursor_on_boundaries() {
        let mut editor = DescriptionEditor::Idle;
        editor.begin(42, "修复 pooled actor");
        editor.backspace();
        editor.insert(" 生命周期").expect("insert");
        let DescriptionEditor::Editing { input, cursor, .. } = editor else {
            panic!("editing");
        };
        assert_eq!(input, "修复 pooled acto 生命周期");
        assert!(input.is_char_boundary(cursor));
    }

    #[test]
    fn visual_layout_preserves_newlines_and_tracks_wrapped_cursor() {
        let text = "first line\nwide 文件 name";
        let layout = layout_description_text(text, text.len(), 8);
        assert!(layout.lines.len() >= 3);
        assert_eq!(layout.lines[0], "first li");
        assert_eq!(layout.cursor_line, layout.lines.len() - 1);
    }

    #[test]
    fn apply_cannot_be_cancelled_after_write_dispatch() {
        let mut editor = DescriptionEditor::Applying {
            change: 42,
            input: "new".into(),
            cursor: 3,
            request_id: 7,
        };
        assert!(!editor.cancel());
        assert!(matches!(editor, DescriptionEditor::Applying { .. }));
    }

    #[test]
    fn pointer_position_maps_wrapped_and_wide_text_to_a_utf8_boundary() {
        let mut editor = DescriptionEditor::Idle;
        editor.begin(42, "ab文件cd");
        editor.set_cursor_from_visual_position(0, 1, 0, 5);
        editor.insert("!").expect("insert at clicked position");
        let DescriptionEditor::Editing { input, cursor, .. } = editor else {
            panic!("editing");
        };
        assert_eq!(input, "ab文!件cd");
        assert!(input.is_char_boundary(cursor));
    }

    #[test]
    fn cursor_before_a_wrapped_character_is_on_the_next_visual_line() {
        let layout = layout_description_text("abcdeX", 5, 5);
        assert_eq!((layout.cursor_line, layout.cursor_column), (1, 0));
        assert_eq!(layout.lines, ["abcde", "X"]);
    }

    #[test]
    fn vertical_movement_uses_wrapped_visual_lines() {
        let mut editor = DescriptionEditor::Idle;
        editor.begin(42, "abcdefghi");
        editor.move_vertical(-1, 4);
        let DescriptionEditor::Editing { cursor, .. } = editor else {
            panic!("editing");
        };
        assert_eq!(cursor, 5, "column one of the middle visual row");
    }
}
