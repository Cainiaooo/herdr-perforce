use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
};

use crate::{
    domain::{ChangedFile, Changelist, ChangelistId, WorkspaceIdentity},
    p4::strip_verbatim_prefix,
};

use super::icons::explorer_icon;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReviewRowKey {
    Changelist(ChangelistId),
    Directory {
        change: ChangelistId,
        path: String,
    },
    File {
        change: ChangelistId,
        depot_path: String,
    },
    Status {
        change: ChangelistId,
    },
}

#[derive(Debug, Clone)]
pub enum ReviewRowKind {
    Changelist { expanded: bool },
    Directory,
    File(ChangedFile),
    Status,
}

#[derive(Debug, Clone)]
pub struct ReviewRow {
    pub key: ReviewRowKey,
    pub change: ChangelistId,
    pub change_index: usize,
    pub depth: usize,
    pub label: String,
    pub kind: ReviewRowKind,
}

#[derive(Debug, Clone)]
enum FileState {
    Loading,
    Ready(Vec<ChangedFile>),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewActivation {
    None,
    LoadFiles {
        generation: u64,
        change: ChangelistId,
    },
    OpenFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckedFiles {
    pub source: ChangelistId,
    pub count: usize,
}

#[derive(Debug)]
pub struct ReviewTreeModel {
    generation: u64,
    expanded: BTreeSet<ChangelistId>,
    files: BTreeMap<ChangelistId, FileState>,
    selected: Option<ReviewRowKey>,
    restore_selected_file: Option<(ChangelistId, String)>,
    checked_source: Option<ChangelistId>,
    checked: BTreeSet<String>,
}

impl ReviewTreeModel {
    pub fn new() -> Self {
        Self {
            generation: 0,
            expanded: BTreeSet::new(),
            files: BTreeMap::new(),
            selected: None,
            restore_selected_file: None,
            checked_source: None,
            checked: BTreeSet::new(),
        }
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn refresh(&mut self, changes: &[Changelist], preferred: Option<ChangelistId>) {
        let ids = changes
            .iter()
            .map(|change| change.id)
            .collect::<BTreeSet<_>>();
        self.generation = self.generation.wrapping_add(1);
        self.expanded.retain(|change| ids.contains(change));
        self.files.clear();
        self.checked.clear();
        self.checked_source = None;
        let previous_selection = self.selected.clone();
        let selected_change = preferred
            .filter(|change| ids.contains(change))
            .or_else(|| previous_selection.as_ref().map(ReviewRowKey::change))
            .filter(|change| ids.contains(change))
            .or_else(|| changes.first().map(|change| change.id));
        self.restore_selected_file = match (selected_change, previous_selection) {
            (Some(selected_change), Some(ReviewRowKey::File { change, depot_path }))
                if change == selected_change =>
            {
                Some((change, depot_path))
            }
            _ => None,
        };
        self.selected = selected_change.map(ReviewRowKey::Changelist);
    }

    #[must_use]
    pub fn rows(&self, changes: &[Changelist], workspace: &WorkspaceIdentity) -> Vec<ReviewRow> {
        let mut rows = Vec::new();
        for (change_index, change) in changes.iter().enumerate() {
            let expanded = self.expanded.contains(&change.id);
            let file_count = match self.files.get(&change.id) {
                Some(FileState::Ready(files)) => Some(files.len()),
                _ => None,
            };
            rows.push(ReviewRow {
                key: ReviewRowKey::Changelist(change.id),
                change: change.id,
                change_index,
                depth: 0,
                label: format_changelist(change, expanded, file_count),
                kind: ReviewRowKind::Changelist { expanded },
            });
            if !expanded {
                continue;
            }
            match self.files.get(&change.id) {
                Some(FileState::Ready(files)) if files.is_empty() => {
                    rows.push(status_row(change.id, change_index, "(empty changelist)"))
                }
                Some(FileState::Ready(files)) => {
                    append_file_tree(&mut rows, workspace, change.id, change_index, files);
                }
                Some(FileState::Failed(message)) => {
                    rows.push(status_row(change.id, change_index, &format!("! {message}")))
                }
                Some(FileState::Loading) | None => {
                    rows.push(status_row(change.id, change_index, "… Loading files…"))
                }
            }
        }
        rows
    }

    pub fn activate_selected(
        &mut self,
        changes: &[Changelist],
        workspace: &WorkspaceIdentity,
    ) -> ReviewActivation {
        let rows = self.rows(changes, workspace);
        let Some(row) = self.selected_row(&rows).cloned() else {
            return ReviewActivation::None;
        };
        match row.kind {
            ReviewRowKind::Changelist { expanded: true } => {
                self.expanded.remove(&row.change);
                ReviewActivation::None
            }
            ReviewRowKind::Changelist { expanded: false } => {
                self.expanded.insert(row.change);
                match self.files.get(&row.change) {
                    Some(FileState::Ready(_)) | Some(FileState::Loading) => ReviewActivation::None,
                    Some(FileState::Failed(_)) | None => {
                        self.files.insert(row.change, FileState::Loading);
                        ReviewActivation::LoadFiles {
                            generation: self.generation,
                            change: row.change,
                        }
                    }
                }
            }
            ReviewRowKind::File(_) => ReviewActivation::OpenFile,
            ReviewRowKind::Directory | ReviewRowKind::Status => ReviewActivation::None,
        }
    }

    pub fn activate_index(
        &mut self,
        index: usize,
        changes: &[Changelist],
        workspace: &WorkspaceIdentity,
        mouse_click: bool,
    ) -> ReviewActivation {
        self.select_index(index, changes, workspace);
        if mouse_click && self.selected_file(changes, workspace).is_some() {
            self.toggle_selected_file(changes, workspace);
            ReviewActivation::None
        } else {
            self.activate_selected(changes, workspace)
        }
    }

    pub fn install_files(
        &mut self,
        generation: u64,
        change: ChangelistId,
        result: Result<Vec<ChangedFile>, String>,
    ) {
        if generation != self.generation {
            return;
        }
        if self
            .restore_selected_file
            .as_ref()
            .is_some_and(|(restore_change, _)| *restore_change == change)
            && let (Some((restore_change, depot_path)), Ok(files)) =
                (self.restore_selected_file.take(), &result)
            && files.iter().any(|file| file.depot_path == depot_path)
        {
            self.selected = Some(ReviewRowKey::File {
                change: restore_change,
                depot_path,
            });
        }
        self.files.insert(
            change,
            match result {
                Ok(files) => FileState::Ready(files),
                Err(message) => FileState::Failed(message),
            },
        );
    }

    pub fn begin_expanded_reloads(&mut self) -> Vec<(u64, ChangelistId)> {
        let generation = self.generation;
        self.expanded
            .iter()
            .copied()
            .map(|change| {
                self.files.insert(change, FileState::Loading);
                (generation, change)
            })
            .collect()
    }

    pub fn move_selection(
        &mut self,
        delta: isize,
        changes: &[Changelist],
        workspace: &WorkspaceIdentity,
    ) {
        let rows = self.rows(changes, workspace);
        if rows.is_empty() {
            return;
        }
        let index = self.selected_index(&rows);
        let next = if delta < 0 {
            index.saturating_sub(delta.unsigned_abs())
        } else {
            index.saturating_add(delta as usize).min(rows.len() - 1)
        };
        self.selected = Some(rows[next].key.clone());
    }

    pub fn select_index(
        &mut self,
        index: usize,
        changes: &[Changelist],
        workspace: &WorkspaceIdentity,
    ) {
        if let Some(row) = self.rows(changes, workspace).get(index) {
            self.selected = Some(row.key.clone());
        }
    }

    #[must_use]
    pub fn selected_index_for(
        &self,
        changes: &[Changelist],
        workspace: &WorkspaceIdentity,
    ) -> usize {
        self.selected_index(&self.rows(changes, workspace))
    }

    #[must_use]
    pub fn selected_change_index(
        &self,
        changes: &[Changelist],
        workspace: &WorkspaceIdentity,
    ) -> Option<usize> {
        let rows = self.rows(changes, workspace);
        self.selected_row(&rows).map(|row| row.change_index)
    }

    #[must_use]
    pub fn selected_file(
        &self,
        changes: &[Changelist],
        workspace: &WorkspaceIdentity,
    ) -> Option<ChangedFile> {
        let rows = self.rows(changes, workspace);
        let row = self.selected_row(&rows)?;
        match &row.kind {
            ReviewRowKind::File(file) => Some(file.clone()),
            _ => None,
        }
    }

    pub fn toggle_selected_file(
        &mut self,
        changes: &[Changelist],
        workspace: &WorkspaceIdentity,
    ) -> Option<CheckedFiles> {
        let rows = self.rows(changes, workspace);
        let row = self.selected_row(&rows)?;
        let ReviewRowKind::File(file) = &row.kind else {
            return None;
        };
        if self.checked_source != Some(row.change) {
            self.checked.clear();
            self.checked_source = Some(row.change);
        }
        if !self.checked.insert(file.depot_path.clone()) {
            self.checked.remove(&file.depot_path);
        }
        if self.checked.is_empty() {
            self.checked_source = None;
            return Some(CheckedFiles {
                source: row.change,
                count: 0,
            });
        }
        Some(CheckedFiles {
            source: row.change,
            count: self.checked.len(),
        })
    }

    #[must_use]
    pub fn checked_files(&self) -> Option<(ChangelistId, Vec<String>)> {
        self.checked_source
            .map(|source| (source, self.checked.iter().cloned().collect()))
    }

    #[must_use]
    pub fn is_checked(&self, file: &ChangedFile) -> bool {
        self.checked.contains(&file.depot_path)
    }

    pub fn clear_checked(&mut self) {
        self.checked.clear();
        self.checked_source = None;
    }

    #[must_use]
    pub fn selected_key(&self) -> Option<&ReviewRowKey> {
        self.selected.as_ref()
    }

    fn selected_index(&self, rows: &[ReviewRow]) -> usize {
        self.selected
            .as_ref()
            .and_then(|key| rows.iter().position(|row| &row.key == key))
            .unwrap_or(0)
    }

    fn selected_row<'a>(&self, rows: &'a [ReviewRow]) -> Option<&'a ReviewRow> {
        let index = self.selected_index(rows);
        rows.get(index)
    }
}

impl ReviewRowKey {
    fn change(&self) -> ChangelistId {
        match self {
            Self::Changelist(change)
            | Self::Directory { change, .. }
            | Self::File { change, .. }
            | Self::Status { change } => *change,
        }
    }
}

fn format_changelist(change: &Changelist, expanded: bool, file_count: Option<usize>) -> String {
    let caret = if expanded { "▾" } else { "▸" };
    let description = change
        .description
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("<no description>")
        .trim();
    let count = file_count.map_or_else(String::new, |count| format!("  ({count})"));
    format!(
        "{caret} CL {}  {}{count}  {description}",
        change.id,
        change.status.canonical_name()
    )
}

fn status_row(change: ChangelistId, change_index: usize, label: &str) -> ReviewRow {
    ReviewRow {
        key: ReviewRowKey::Status { change },
        change,
        change_index,
        depth: 1,
        label: label.to_owned(),
        kind: ReviewRowKind::Status,
    }
}

#[derive(Default)]
struct TreeNode {
    directories: BTreeMap<String, TreeNode>,
    files: Vec<ChangedFile>,
}

fn append_file_tree(
    rows: &mut Vec<ReviewRow>,
    workspace: &WorkspaceIdentity,
    change: ChangelistId,
    change_index: usize,
    files: &[ChangedFile],
) {
    let mut root = TreeNode::default();
    for file in files {
        let mut segments = display_segments(workspace, file);
        segments.pop();
        let mut node = &mut root;
        for segment in segments {
            node = node.directories.entry(segment).or_default();
        }
        node.files.push(file.clone());
        node.files.sort_by(|left, right| {
            file_name_for(workspace, left)
                .to_ascii_lowercase()
                .cmp(&file_name_for(workspace, right).to_ascii_lowercase())
        });
    }
    append_node(rows, workspace, change, change_index, &root, 1, "");
}

fn append_node(
    rows: &mut Vec<ReviewRow>,
    workspace: &WorkspaceIdentity,
    change: ChangelistId,
    change_index: usize,
    node: &TreeNode,
    depth: usize,
    parent: &str,
) {
    for (name, child) in &node.directories {
        let path = if parent.is_empty() {
            name.clone()
        } else {
            format!("{parent}/{name}")
        };
        rows.push(ReviewRow {
            key: ReviewRowKey::Directory {
                change,
                path: path.clone(),
            },
            change,
            change_index,
            depth,
            label: format!("{} {name}", explorer_icon(name, true, true)),
            kind: ReviewRowKind::Directory,
        });
        append_node(
            rows,
            workspace,
            change,
            change_index,
            child,
            depth + 1,
            &path,
        );
    }
    for file in &node.files {
        let name = file_name_for(workspace, file);
        rows.push(ReviewRow {
            key: ReviewRowKey::File {
                change,
                depot_path: file.depot_path.clone(),
            },
            change,
            change_index,
            depth,
            label: format!(
                "{}  {} {}",
                file.action.short_badge(),
                explorer_icon(&name, false, false),
                name
            ),
            kind: ReviewRowKind::File(file.clone()),
        });
    }
}

fn display_segments(workspace: &WorkspaceIdentity, file: &ChangedFile) -> Vec<String> {
    if let Some(path) = file.client_path.as_deref()
        && let Some(segments) = client_relative_segments(workspace, path)
        && !segments.is_empty()
    {
        return segments;
    }
    file.depot_path
        .trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn file_name_for(workspace: &WorkspaceIdentity, file: &ChangedFile) -> String {
    display_segments(workspace, file)
        .pop()
        .unwrap_or_else(|| file.depot_path.clone())
}

fn client_relative_segments(
    workspace: &WorkspaceIdentity,
    client_path: &Path,
) -> Option<Vec<String>> {
    let root = strip_verbatim_prefix(&workspace.root);
    let path = strip_verbatim_prefix(client_path);
    let mut path_components = path.components();
    for root_component in root.components() {
        let path_component = path_components.next()?;
        let root_key = workspace
            .case_handling
            .canonical_path_key(&root_component.as_os_str().to_string_lossy());
        let path_key = workspace
            .case_handling
            .canonical_path_key(&path_component.as_os_str().to_string_lossy());
        if root_key != path_key {
            return None;
        }
    }
    path_components
        .map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            Component::CurDir => None,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::domain::{CaseHandling, ChangelistStatus, FileAction, FileType};

    fn workspace(root: &str) -> WorkspaceIdentity {
        WorkspaceIdentity {
            server_id: "server".into(),
            user: "me".into(),
            client: "client".into(),
            root: PathBuf::from(root),
            stream: None,
            case_handling: CaseHandling::Insensitive,
        }
    }

    fn change(id: u64) -> Changelist {
        Changelist {
            id: ChangelistId::Numbered(id),
            status: ChangelistStatus::Pending,
            owner: "me".into(),
            client: "client".into(),
            description: format!("Change {id}"),
            files: Vec::new(),
            preserved_spec_fields: Default::default(),
            spec_token: None,
            content_token: None,
        }
    }

    fn file(path: &str) -> ChangedFile {
        ChangedFile {
            depot_path: format!("//depot/{path}"),
            client_path: Some(PathBuf::from("C:/ws").join(path)),
            action: FileAction::Edit,
            file_type: FileType::new("text"),
            base_revision: Some(1),
            moved_from: None,
            moved_to: None,
        }
    }

    #[test]
    fn expanded_change_builds_directory_rows_inline() {
        let changes = vec![change(42)];
        let workspace = workspace("C:/ws");
        let mut tree = ReviewTreeModel::new();
        tree.refresh(&changes, None);
        assert!(matches!(
            tree.activate_selected(&changes, &workspace),
            ReviewActivation::LoadFiles {
                change: ChangelistId::Numbered(42),
                ..
            }
        ));
        tree.install_files(
            tree.generation(),
            ChangelistId::Numbered(42),
            Ok(vec![file("src/ui/main.rs"), file("README.md")]),
        );
        let rows = tree.rows(&changes, &workspace);
        assert_eq!(rows.len(), 5);
        assert!(rows[0].label.starts_with("▾ CL 42"));
        assert_eq!(rows[1].label, "📂 src");
        assert_eq!(rows[2].label, "📂 ui");
        assert!(rows[3].label.ends_with("main.rs"));
        assert!(rows[4].label.ends_with("README.md"));
    }

    #[test]
    fn checking_a_file_is_scoped_to_one_source_change() {
        let changes = vec![change(42), change(77)];
        let workspace = workspace("C:/ws");
        let mut tree = ReviewTreeModel::new();
        tree.refresh(&changes, None);
        tree.activate_selected(&changes, &workspace);
        tree.install_files(
            tree.generation(),
            ChangelistId::Numbered(42),
            Ok(vec![file("a.txt")]),
        );
        tree.move_selection(1, &changes, &workspace);
        let checked = tree
            .toggle_selected_file(&changes, &workspace)
            .expect("file row");
        assert_eq!(checked.count, 1);
        assert_eq!(tree.checked_files().unwrap().1, vec!["//depot/a.txt"]);
        tree.refresh(&changes, Some(ChangelistId::Numbered(42)));
        assert!(tree.checked_files().is_none());
    }

    #[test]
    fn refresh_restores_the_selected_file_after_reloading_an_expanded_change() {
        let changes = vec![change(42)];
        let workspace = workspace("C:/ws");
        let mut tree = ReviewTreeModel::new();
        tree.refresh(&changes, None);
        tree.activate_selected(&changes, &workspace);
        tree.install_files(
            tree.generation(),
            ChangelistId::Numbered(42),
            Ok(vec![file("src/main.rs")]),
        );
        tree.move_selection(2, &changes, &workspace);
        assert!(matches!(
            tree.selected_key(),
            Some(ReviewRowKey::File { .. })
        ));

        tree.refresh(&changes, Some(ChangelistId::Numbered(42)));
        let reload = tree.begin_expanded_reloads();
        assert_eq!(reload.len(), 1);
        tree.install_files(reload[0].0, reload[0].1, Ok(vec![file("src/main.rs")]));

        assert!(matches!(
            tree.selected_key(),
            Some(ReviewRowKey::File { .. })
        ));
    }

    #[test]
    fn client_root_and_case_handling_define_the_display_tree() {
        let changes = vec![change(42)];
        let workspace = workspace("C:/CLIENT");
        let mut changed_file = file("source/ui/main.rs");
        changed_file.client_path = Some(PathBuf::from("C:/client/source/ui/main.rs"));
        let mut tree = ReviewTreeModel::new();
        tree.refresh(&changes, None);
        tree.activate_selected(&changes, &workspace);
        tree.install_files(
            tree.generation(),
            ChangelistId::Numbered(42),
            Ok(vec![changed_file]),
        );

        let labels = tree
            .rows(&changes, &workspace)
            .into_iter()
            .map(|row| row.label)
            .collect::<Vec<_>>();
        assert!(labels.iter().any(|label| label == "📂 source"));
        assert!(labels.iter().any(|label| label == "📂 ui"));
        assert!(!labels.iter().any(|label| label == "📂 depot"));
    }
}
