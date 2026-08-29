//! Shared visual vocabulary for the Navigation and Content panes.
//!
//! The palette deliberately stays close to VS Code's dark UI: status colors
//! are readable without becoming neon, selections are neutral, and accent
//! colors are reserved for navigation and primary actions.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub(crate) const fn terminal(self) -> crossterm::style::Color {
        crossterm::style::Color::Rgb {
            r: self.0,
            g: self.1,
            b: self.2,
        }
    }

    pub(crate) const fn tui(self) -> ratatui::style::Color {
        ratatui::style::Color::Rgb(self.0, self.1, self.2)
    }
}

pub(crate) const KEYCAP_BG: Rgb = Rgb(0x32, 0x36, 0x3d);
pub(crate) const KEYCAP_FG: Rgb = Rgb(0xc9, 0xce, 0xd6);
pub(crate) const SELECTION_BG: Rgb = Rgb(0x37, 0x3b, 0x42);
pub(crate) const ACCENT: Rgb = Rgb(0x00, 0x78, 0xd4);
pub(crate) const ACCENT_FOCUS: Rgb = Rgb(0x02, 0x8a, 0xf0);
pub(crate) const HEADER: Rgb = Rgb(0x75, 0xbe, 0xff);
pub(crate) const MUTED: Rgb = Rgb(0x80, 0x86, 0x91);
pub(crate) const BORDER: Rgb = Rgb(0x48, 0x4e, 0x58);

pub(crate) const MODIFIED: Rgb = Rgb(0xe2, 0xc0, 0x8d);
pub(crate) const UNTRACKED: Rgb = Rgb(0x73, 0xc9, 0x91);
pub(crate) const ADDED: Rgb = Rgb(0x81, 0xb8, 0x8b);
pub(crate) const RENAMED: Rgb = Rgb(0x73, 0xc9, 0x91);
pub(crate) const DELETED: Rgb = Rgb(0xc7, 0x4e, 0x39);
pub(crate) const CONFLICT: Rgb = Rgb(0xe4, 0x67, 0x6b);
pub(crate) const INFO: Rgb = Rgb(0x7d, 0xae, 0xc7);

pub(crate) const ADD_LINE_BG: Rgb = Rgb(0x20, 0x39, 0x28);
pub(crate) const DEL_LINE_BG: Rgb = Rgb(0x42, 0x22, 0x26);
pub(crate) const ADD_WORD_BG: Rgb = Rgb(0x35, 0x59, 0x3d);
pub(crate) const DEL_WORD_BG: Rgb = Rgb(0x6f, 0x30, 0x36);
pub(crate) const ADD_MARK: Rgb = Rgb(0x8c, 0xc9, 0x8f);
pub(crate) const DEL_MARK: Rgb = Rgb(0xd1, 0x6d, 0x76);
pub(crate) const FOLD_ROW_BG: Rgb = Rgb(0x1c, 0x24, 0x30);

/// Pack contextual shortcuts into compact keycap rows. The Content pane can
/// wrap these rows, while the narrower Navigation pane uses the same palette
/// with a single-line renderer.
pub(crate) fn key_hint_lines(
    hints: &[(&'static str, &'static str)],
    width: u16,
) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::{
        style::{Modifier, Style},
        text::{Line, Span},
    };

    let width = usize::from(width.max(8));
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 1usize;
    for (key, label) in hints {
        let keycap = format!(" {key} ");
        let caption = format!(" {label}");
        let pair_width = unicode_width::UnicodeWidthStr::width(keycap.as_str())
            + unicode_width::UnicodeWidthStr::width(caption.as_str());
        if !lines.last().is_some_and(Vec::is_empty) && used + 2 + pair_width > width {
            lines.push(Vec::new());
            used = 1;
        }
        let line = lines.last_mut().expect("footer starts with one row");
        if !line.is_empty() {
            line.push(Span::raw("  "));
            used += 2;
        } else {
            line.push(Span::raw(" "));
        }
        line.push(Span::styled(
            keycap,
            Style::default().bg(KEYCAP_BG.tui()).fg(KEYCAP_FG.tui()),
        ));
        line.push(Span::styled(
            caption,
            Style::default().fg(MUTED.tui()).add_modifier(Modifier::DIM),
        ));
        used += pair_width;
    }
    lines.into_iter().map(Line::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_hints_wrap_into_readable_rows_in_a_narrow_pane() {
        let hints = &[("↑↓", "scroll"), ("PgUp/Dn", "page"), ("q", "close")];
        let rows = key_hint_lines(hints, 20);
        assert!(rows.len() >= 2);
        assert!(rows.iter().all(|row| row.width() <= 20));
    }

    #[test]
    fn keycaps_and_captions_keep_distinct_visual_weight() {
        let rows = key_hint_lines(&[("r", "refresh")], 40);
        let spans = &rows[0].spans;
        assert_eq!(spans[1].style.bg, Some(KEYCAP_BG.tui()));
        assert_eq!(spans[1].style.fg, Some(KEYCAP_FG.tui()));
        assert_eq!(spans[2].style.fg, Some(MUTED.tui()));
    }
}
