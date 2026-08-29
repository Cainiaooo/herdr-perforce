//! Workspace File Explorer models.
//!
//! Decorations, preview, and tree flattening are pure: they do not read the
//! filesystem or run `p4`. That keeps ACC-EXPLORER fixtures deterministic.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use super::{ChangelistId, FileAction, FileType};

/// Byte budget for workspace text preview. Larger files are truncated with a reason.
pub const MAX_PREVIEW_BYTES: usize = 512 * 1024;
/// Line budget for workspace text preview.
pub const MAX_PREVIEW_LINES: usize = 4_000;
/// Local directory listing cap. Extra entries are omitted with a truncation marker.
pub const MAX_DIRECTORY_ENTRIES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplorerEntryKind {
    Directory,
    File,
}

/// Read-only P4 decoration for a local tree entry.
///
/// `None` on an [`ExplorerEntry`] means the decoration query failed; the UI
/// must leave the badge empty and must not invent Git status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplorerDecoration {
    Unopened,
    Untracked,
    Opened {
        action: FileAction,
        change: Option<ChangelistId>,
    },
    OutOfDate,
    NotInView,
    Unmapped,
}

impl ExplorerDecoration {
    #[must_use]
    pub fn badge(&self) -> &'static str {
        match self {
            Self::Unopened => "",
            Self::Untracked => "U",
            Self::Opened { action, .. } => action.short_badge(),
            Self::OutOfDate => "↓",
            Self::NotInView => "⊘",
            Self::Unmapped => "?",
        }
    }

    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Unopened => "tracked and current".to_owned(),
            Self::Untracked => "not under Perforce control".to_owned(),
            Self::Opened { action, change } => match change {
                Some(change) => format!("{} in CL {change}", action.canonical_name()),
                None => action.canonical_name().to_owned(),
            },
            Self::OutOfDate => "behind depot revision".to_owned(),
            Self::NotInView => "not in view".to_owned(),
            Self::Unmapped => "unmapped".to_owned(),
        }
    }

    #[must_use]
    pub fn opened_change(&self) -> Option<ChangelistId> {
        match self {
            Self::Opened { change, .. } => *change,
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplorerEntry {
    pub name: String,
    pub path: PathBuf,
    pub kind: ExplorerEntryKind,
    pub decoration: Option<ExplorerDecoration>,
    pub file_type: Option<FileType>,
    pub have_rev: Option<u64>,
    pub head_rev: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileP4Facts {
    pub not_in_view: bool,
    pub mapped: bool,
    pub untracked: bool,
    pub opened_action: Option<FileAction>,
    pub opened_change: Option<ChangelistId>,
    pub have_rev: Option<u64>,
    pub head_rev: Option<u64>,
    pub file_type: Option<FileType>,
    pub query_failed: bool,
}

impl FileP4Facts {
    #[must_use]
    pub fn query_failed() -> Self {
        Self {
            not_in_view: false,
            mapped: false,
            untracked: false,
            opened_action: None,
            opened_change: None,
            have_rev: None,
            head_rev: None,
            file_type: None,
            query_failed: true,
        }
    }
}

/// Maps P4 where/fstat/opened facts to a decoration.
///
/// Precedence: query failure → no badge; not in view; opened; untracked;
/// unmapped; out-of-date; clean tracked file.
#[must_use]
pub fn decoration_from_facts(facts: &FileP4Facts) -> Option<ExplorerDecoration> {
    if facts.query_failed {
        return None;
    }
    if facts.not_in_view {
        return Some(ExplorerDecoration::NotInView);
    }
    if let Some(action) = facts.opened_action.clone() {
        return Some(ExplorerDecoration::Opened {
            action,
            change: facts.opened_change,
        });
    }
    if facts.untracked {
        return Some(ExplorerDecoration::Untracked);
    }
    if !facts.mapped {
        return Some(ExplorerDecoration::Unmapped);
    }
    if is_out_of_date(facts.have_rev, facts.head_rev) {
        return Some(ExplorerDecoration::OutOfDate);
    }
    Some(ExplorerDecoration::Unopened)
}

fn is_out_of_date(have: Option<u64>, head: Option<u64>) -> bool {
    match (have, head) {
        (Some(have), Some(head)) => have < head,
        (None, Some(_)) => true,
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewTruncation {
    ByteBudget { limit: usize },
    LineBudget { limit: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewContent {
    None,
    Directory {
        name: String,
        child_count: Option<usize>,
    },
    Text {
        lines: Vec<String>,
        truncated: Option<PreviewTruncation>,
    },
    Binary {
        size: Option<u64>,
        file_type: Option<String>,
        have_rev: Option<u64>,
        head_rev: Option<u64>,
    },
    Failed {
        message: String,
    },
}

impl PreviewContent {
    #[must_use]
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// Builds a preview from bytes already read from the workspace file.
///
/// An empty buffer is a valid empty text file, never a read failure. NUL bytes
/// or invalid UTF-8 are binary, not an empty document. `file_type` from P4
/// wins when it reports a binary type.
#[must_use]
pub fn preview_from_bytes(
    bytes: &[u8],
    byte_truncated: bool,
    file_type: Option<&FileType>,
    local_size: Option<u64>,
    have_rev: Option<u64>,
    head_rev: Option<u64>,
) -> PreviewContent {
    if file_type.is_some_and(FileType::is_binary) || is_binary_content(bytes) {
        return PreviewContent::Binary {
            size: local_size,
            file_type: file_type.map(|value| value.as_str().to_owned()),
            have_rev,
            head_rev,
        };
    }

    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text.strip_prefix('\u{feff}').unwrap_or(text),
        Err(_) => {
            return PreviewContent::Binary {
                size: local_size,
                file_type: file_type.map(|value| value.as_str().to_owned()),
                have_rev,
                head_rev,
            };
        }
    };

    let mut lines: Vec<String> = text
        .split('\n')
        .map(|line| line.trim_end_matches('\r').to_owned())
        .collect();
    if text.is_empty() {
        lines.clear();
    } else if text.ends_with('\n') {
        lines.pop();
    }

    let mut truncated = if byte_truncated {
        Some(PreviewTruncation::ByteBudget {
            limit: MAX_PREVIEW_BYTES,
        })
    } else {
        None
    };
    if lines.len() > MAX_PREVIEW_LINES {
        lines.truncate(MAX_PREVIEW_LINES);
        if truncated.is_none() {
            truncated = Some(PreviewTruncation::LineBudget {
                limit: MAX_PREVIEW_LINES,
            });
        }
    }

    PreviewContent::Text { lines, truncated }
}

fn is_binary_content(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleExplorerRow {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub kind: ExplorerEntryKind,
    pub expanded: bool,
    pub decoration: Option<ExplorerDecoration>,
    pub file_type: Option<FileType>,
    pub have_rev: Option<u64>,
    pub head_rev: Option<u64>,
}

/// Flattens the lazy directory map into the rows the tree widget paints.
///
/// `listings` is keyed by the directory path as stored when that directory was
/// loaded. Missing keys are treated as not-yet-loaded (no children rendered).
#[must_use]
pub fn flatten_explorer_tree(
    root: &Path,
    root_name: &str,
    listings: &BTreeMap<PathBuf, Vec<ExplorerEntry>>,
    expanded: &BTreeSet<PathBuf>,
) -> Vec<VisibleExplorerRow> {
    let mut rows = Vec::new();
    let root_expanded = expanded.contains(root);
    rows.push(VisibleExplorerRow {
        path: root.to_path_buf(),
        name: root_name.to_owned(),
        depth: 0,
        kind: ExplorerEntryKind::Directory,
        expanded: root_expanded,
        decoration: None,
        file_type: None,
        have_rev: None,
        head_rev: None,
    });
    if root_expanded {
        push_children(&mut rows, root, listings, expanded, 1);
    }
    rows
}

fn push_children(
    rows: &mut Vec<VisibleExplorerRow>,
    directory: &Path,
    listings: &BTreeMap<PathBuf, Vec<ExplorerEntry>>,
    expanded: &BTreeSet<PathBuf>,
    depth: usize,
) {
    let Some(entries) = listings.get(directory) else {
        return;
    };
    for entry in entries {
        let is_expanded =
            entry.kind == ExplorerEntryKind::Directory && expanded.contains(&entry.path);
        rows.push(VisibleExplorerRow {
            path: entry.path.clone(),
            name: entry.name.clone(),
            depth,
            kind: entry.kind,
            expanded: is_expanded,
            decoration: entry.decoration.clone(),
            file_type: entry.file_type.clone(),
            have_rev: entry.have_rev,
            head_rev: entry.head_rev,
        });
        if is_expanded {
            push_children(rows, &entry.path, listings, expanded, depth + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts_mapped() -> FileP4Facts {
        FileP4Facts {
            not_in_view: false,
            mapped: true,
            untracked: false,
            opened_action: None,
            opened_change: None,
            have_rev: Some(3),
            head_rev: Some(3),
            file_type: Some(FileType::new("text")),
            query_failed: false,
        }
    }

    #[test]
    fn decorations_follow_acc_explorer_precedence() {
        assert_eq!(decoration_from_facts(&FileP4Facts::query_failed()), None);

        let mut facts = facts_mapped();
        facts.not_in_view = true;
        assert_eq!(
            decoration_from_facts(&facts),
            Some(ExplorerDecoration::NotInView)
        );

        facts = facts_mapped();
        facts.untracked = true;
        assert_eq!(
            decoration_from_facts(&facts),
            Some(ExplorerDecoration::Untracked)
        );

        facts = facts_mapped();
        facts.mapped = false;
        assert_eq!(
            decoration_from_facts(&facts),
            Some(ExplorerDecoration::Unmapped)
        );

        facts = facts_mapped();
        facts.opened_action = Some(FileAction::Edit);
        facts.opened_change = Some(ChangelistId::Numbered(42));
        facts.have_rev = Some(3);
        facts.head_rev = Some(4);
        assert_eq!(
            decoration_from_facts(&facts),
            Some(ExplorerDecoration::Opened {
                action: FileAction::Edit,
                change: Some(ChangelistId::Numbered(42)),
            })
        );

        facts = facts_mapped();
        facts.have_rev = Some(2);
        facts.head_rev = Some(4);
        assert_eq!(
            decoration_from_facts(&facts),
            Some(ExplorerDecoration::OutOfDate)
        );

        facts = facts_mapped();
        facts.opened_action = Some(FileAction::Add);
        facts.opened_change = Some(ChangelistId::Default);
        assert_eq!(
            decoration_from_facts(&facts),
            Some(ExplorerDecoration::Opened {
                action: FileAction::Add,
                change: Some(ChangelistId::Default),
            })
        );

        assert_eq!(
            decoration_from_facts(&facts_mapped()),
            Some(ExplorerDecoration::Unopened)
        );
    }

    #[test]
    fn badges_distinguish_move_untracked_and_behind_states() {
        assert_eq!(ExplorerDecoration::Untracked.badge(), "U");
        assert_eq!(ExplorerDecoration::OutOfDate.badge(), "↓");
        for action in [FileAction::MoveAdd, FileAction::MoveDelete] {
            assert_eq!(
                ExplorerDecoration::Opened {
                    action,
                    change: Some(ChangelistId::Numbered(42)),
                }
                .badge(),
                "R"
            );
        }
    }

    #[test]
    fn empty_bytes_are_an_empty_text_file_not_a_read_failure() {
        let preview = preview_from_bytes(
            &[],
            false,
            Some(&FileType::new("text")),
            Some(0),
            None,
            None,
        );
        assert_eq!(
            preview,
            PreviewContent::Text {
                lines: Vec::new(),
                truncated: None,
            }
        );
        assert!(!preview.is_failure());
    }

    #[test]
    fn nul_and_p4_binary_types_use_the_metadata_card() {
        let from_nul = preview_from_bytes(b"abc\0def", false, None, Some(7), Some(1), Some(2));
        assert_eq!(
            from_nul,
            PreviewContent::Binary {
                size: Some(7),
                file_type: None,
                have_rev: Some(1),
                head_rev: Some(2),
            }
        );

        let from_type = preview_from_bytes(
            b"not really binary",
            false,
            Some(&FileType::new("binary+l")),
            Some(17),
            Some(3),
            Some(3),
        );
        assert_eq!(
            from_type,
            PreviewContent::Binary {
                size: Some(17),
                file_type: Some("binary+l".into()),
                have_rev: Some(3),
                head_rev: Some(3),
            }
        );
    }

    #[test]
    fn invalid_utf8_is_binary_not_an_empty_document() {
        let preview = preview_from_bytes(&[0xff, 0xfe, 0xfd], false, None, Some(3), None, None);
        assert!(matches!(
            preview,
            PreviewContent::Binary { size: Some(3), .. }
        ));
    }

    #[test]
    fn crlf_and_missing_final_newline_keep_stable_lines() {
        let with_crlf = preview_from_bytes(b"a\r\nb\r\n", false, None, Some(6), None, None);
        let PreviewContent::Text { lines, truncated } = with_crlf else {
            panic!("expected text");
        };
        assert_eq!(lines, ["a", "b"]);
        assert_eq!(truncated, None);

        let no_nl = preview_from_bytes(b"only", false, None, Some(4), None, None);
        let PreviewContent::Text { lines, .. } = no_nl else {
            panic!("expected text");
        };
        assert_eq!(lines, ["only"]);
    }

    #[test]
    fn byte_and_line_budgets_record_the_truncation_reason() {
        let byte_cut = preview_from_bytes(b"hello\nworld", true, None, Some(9_000), None, None);
        assert!(matches!(
            byte_cut,
            PreviewContent::Text {
                truncated: Some(PreviewTruncation::ByteBudget {
                    limit: MAX_PREVIEW_BYTES
                }),
                ..
            }
        ));

        let many = (0..MAX_PREVIEW_LINES + 8)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let line_cut = preview_from_bytes(many.as_bytes(), false, None, None, None, None);
        let PreviewContent::Text { lines, truncated } = line_cut else {
            panic!("expected text");
        };
        assert_eq!(lines.len(), MAX_PREVIEW_LINES);
        assert_eq!(
            truncated,
            Some(PreviewTruncation::LineBudget {
                limit: MAX_PREVIEW_LINES,
            })
        );
    }

    #[test]
    fn flatten_keeps_collapsed_children_hidden_and_preserves_expansion() {
        let root = PathBuf::from("C:/ws");
        let src = root.join("src");
        let mut listings = BTreeMap::new();
        listings.insert(
            root.clone(),
            vec![
                ExplorerEntry {
                    name: "src".into(),
                    path: src.clone(),
                    kind: ExplorerEntryKind::Directory,
                    decoration: None,
                    file_type: None,
                    have_rev: None,
                    head_rev: None,
                },
                ExplorerEntry {
                    name: "README.md".into(),
                    path: root.join("README.md"),
                    kind: ExplorerEntryKind::File,
                    decoration: Some(ExplorerDecoration::Unopened),
                    file_type: Some(FileType::new("text")),
                    have_rev: Some(1),
                    head_rev: Some(1),
                },
            ],
        );
        listings.insert(
            src.clone(),
            vec![ExplorerEntry {
                name: "main.rs".into(),
                path: src.join("main.rs"),
                kind: ExplorerEntryKind::File,
                decoration: Some(ExplorerDecoration::Opened {
                    action: FileAction::Edit,
                    change: Some(ChangelistId::Numbered(42)),
                }),
                file_type: Some(FileType::new("text")),
                have_rev: Some(2),
                head_rev: Some(2),
            }],
        );

        let mut expanded = BTreeSet::from([root.clone()]);
        let collapsed = flatten_explorer_tree(&root, "ws", &listings, &expanded);
        assert_eq!(
            collapsed
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            ["ws", "src", "README.md"]
        );

        expanded.insert(src);
        let opened = flatten_explorer_tree(&root, "ws", &listings, &expanded);
        assert_eq!(
            opened
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            ["ws", "src", "main.rs", "README.md"]
        );
        assert_eq!(
            opened[2]
                .decoration
                .as_ref()
                .and_then(ExplorerDecoration::opened_change),
            Some(ChangelistId::Numbered(42))
        );
        assert_eq!(opened[2].depth, 2);
    }
}
