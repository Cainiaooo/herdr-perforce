//! Display-column helpers for the navigation pane.
//!
//! Tree rows stay on a single visual line. When a name is wider than the pane,
//! the UI pans horizontally instead of wrapping.

use unicode_width::UnicodeWidthChar;

pub fn char_width(character: char) -> usize {
    match character {
        '\t' => 4,
        other => UnicodeWidthChar::width(other).unwrap_or(0),
    }
}

#[must_use]
pub fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// Skip `skip` display columns, then take up to `take` display columns.
#[must_use]
pub fn slice_display(text: &str, skip: usize, take: usize) -> String {
    let mut skipped = 0usize;
    let mut taken = 0usize;
    let mut output = String::new();
    for character in text.chars() {
        let width = char_width(character);
        if skipped < skip {
            skipped = skipped.saturating_add(width);
            continue;
        }
        if take != usize::MAX && taken.saturating_add(width) > take {
            break;
        }
        output.push(character);
        taken = taken.saturating_add(width);
    }
    output
}

#[must_use]
pub fn pad_display(text: &str, width: usize) -> String {
    let sliced = slice_display(text, 0, width);
    let used = display_width(&sliced);
    let mut output = sliced;
    if used < width {
        output.push_str(&" ".repeat(width - used));
    }
    output
}

pub fn splice_display(line: &str, column: usize, value: &str) -> String {
    let left = pad_display(&slice_display(line, 0, column), column);
    let end = column.saturating_add(display_width(value));
    let right = slice_display(line, end, usize::MAX);
    format!("{left}{value}{right}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_display_pans_across_wide_glyphs() {
        let text = ">📁 very-long-file-name.cpp";
        assert!(display_width(text) > 10);
        let panned = slice_display(text, 4, 8);
        assert!(panned.contains("very") || panned.contains("ery-"));
        assert!(!panned.contains('\n'));
        assert_eq!(display_width(&pad_display(text, 12)), 12);
    }
}
