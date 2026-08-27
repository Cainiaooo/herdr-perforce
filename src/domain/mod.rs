mod explorer;
mod freshness;

use std::{collections::BTreeMap, fmt, path::PathBuf};

pub use explorer::{
    ExplorerDecoration, ExplorerEntry, ExplorerEntryKind, FileP4Facts, MAX_DIRECTORY_ENTRIES,
    MAX_PREVIEW_BYTES, MAX_PREVIEW_LINES, PreviewContent, PreviewTruncation, VisibleExplorerRow,
    decoration_from_facts, flatten_explorer_tree, preview_from_bytes,
};
pub use freshness::{ContentToken, SpecToken, compute_spec_token};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceIdentity {
    pub server_id: String,
    pub user: String,
    pub client: String,
    pub root: PathBuf,
    pub stream: Option<String>,
    pub case_handling: CaseHandling,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaseHandling {
    Sensitive,
    Insensitive,
    Hybrid,
    Unknown(String),
}

impl CaseHandling {
    #[must_use]
    pub fn from_p4(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "sensitive" => Self::Sensitive,
            "insensitive" => Self::Insensitive,
            "hybrid" => Self::Hybrid,
            _ => Self::Unknown(value.to_owned()),
        }
    }

    #[must_use]
    pub fn canonical_name(&self) -> &str {
        match self {
            Self::Sensitive => "sensitive",
            Self::Insensitive => "insensitive",
            Self::Hybrid => "hybrid",
            Self::Unknown(value) => value,
        }
    }

    #[must_use]
    pub fn canonical_path_key(&self, path: &str) -> String {
        match self {
            Self::Sensitive => path.to_owned(),
            Self::Insensitive | Self::Hybrid | Self::Unknown(_) => path.to_lowercase(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChangelistId {
    Default,
    Numbered(u64),
}

impl ChangelistId {
    #[must_use]
    pub fn as_p4_arg(self) -> String {
        match self {
            Self::Default => "default".to_owned(),
            Self::Numbered(number) => number.to_string(),
        }
    }
}

impl fmt::Display for ChangelistId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => formatter.write_str("default"),
            Self::Numbered(number) => number.fmt(formatter),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangelistStatus {
    Pending,
    Shelved,
    Submitted,
    Unknown(String),
}

impl ChangelistStatus {
    #[must_use]
    pub fn from_p4(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "pending" => Self::Pending,
            "shelved" => Self::Shelved,
            "submitted" => Self::Submitted,
            _ => Self::Unknown(value.to_owned()),
        }
    }

    #[must_use]
    pub fn canonical_name(&self) -> &str {
        match self {
            Self::Pending => "pending",
            Self::Shelved => "shelved",
            Self::Submitted => "submitted",
            Self::Unknown(value) => value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changelist {
    pub id: ChangelistId,
    pub status: ChangelistStatus,
    pub owner: String,
    pub client: String,
    pub description: String,
    pub files: Vec<ChangedFile>,
    pub preserved_spec_fields: BTreeMap<String, String>,
    pub spec_token: Option<SpecToken>,
    pub content_token: Option<ContentToken>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub depot_path: String,
    pub client_path: Option<PathBuf>,
    pub action: FileAction,
    pub file_type: FileType,
    pub base_revision: Option<u64>,
    pub moved_from: Option<String>,
    pub moved_to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileAction {
    Add,
    Edit,
    Delete,
    Branch,
    MoveAdd,
    MoveDelete,
    Integrate,
    Import,
    Purge,
    Archive,
    Unknown(String),
}

impl FileAction {
    #[must_use]
    pub fn from_p4(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "add" => Self::Add,
            "edit" => Self::Edit,
            "delete" => Self::Delete,
            "branch" => Self::Branch,
            "move/add" => Self::MoveAdd,
            "move/delete" => Self::MoveDelete,
            "integrate" => Self::Integrate,
            "import" => Self::Import,
            "purge" => Self::Purge,
            "archive" => Self::Archive,
            _ => Self::Unknown(value.to_owned()),
        }
    }

    #[must_use]
    pub fn canonical_name(&self) -> &str {
        match self {
            Self::Add => "add",
            Self::Edit => "edit",
            Self::Delete => "delete",
            Self::Branch => "branch",
            Self::MoveAdd => "move/add",
            Self::MoveDelete => "move/delete",
            Self::Integrate => "integrate",
            Self::Import => "import",
            Self::Purge => "purge",
            Self::Archive => "archive",
            Self::Unknown(value) => value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileType {
    raw: String,
}

impl FileType {
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self { raw: raw.into() }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    #[must_use]
    pub fn is_binary(&self) -> bool {
        matches!(
            self.raw.split('+').next().unwrap_or_default(),
            "binary" | "ubinary" | "apple" | "resource"
        )
    }

    #[must_use]
    pub fn is_exclusive_open(&self) -> bool {
        self.raw
            .split_once('+')
            .is_some_and(|(_, modifiers)| modifiers.contains('l'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_type_preserves_unknown_modifiers() {
        let file_type = FileType::new("binary+FlS3");
        assert!(file_type.is_binary());
        assert!(file_type.is_exclusive_open());
        assert_eq!(file_type.as_str(), "binary+FlS3");
    }

    #[test]
    fn unknown_file_action_is_forward_compatible() {
        let action = FileAction::from_p4("future-action");
        assert_eq!(action.canonical_name(), "future-action");
        assert_ne!(action, FileAction::Edit);
        assert_eq!(FileAction::from_p4("import"), FileAction::Import);
        assert_eq!(FileAction::from_p4("purge"), FileAction::Purge);
        assert_eq!(FileAction::from_p4("archive"), FileAction::Archive);
    }
}
