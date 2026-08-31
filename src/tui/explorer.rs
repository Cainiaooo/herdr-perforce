//! Workspace File Explorer navigation tree and view-jump state.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

use crate::{
    domain::{
        ChangelistId, ExplorerDecoration, ExplorerEntry, ExplorerEntryKind, FileAction,
        VisibleExplorerRow, WorkspaceIdentity, flatten_explorer_tree,
    },
    p4::{ExplorerError, LoadedDirectory},
};

use super::icons::explorer_icon;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplorerLoadState {
    Idle,
    Checking,
    NotInClientView,
    Failed(String),
    Ready,
}

#[derive(Debug, Clone)]
enum DirectoryState {
    Loading,
    Ready { entries: Vec<ExplorerEntry> },
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplorerAction {
    None,
    LoadDirectory,
    OpenFile,
}

#[derive(Debug)]
pub struct ExplorerModel {
    cwd: PathBuf,
    generation: u64,
    identity: Option<WorkspaceIdentity>,
    load: ExplorerLoadState,
    listings: BTreeMap<PathBuf, DirectoryState>,
    expanded: BTreeSet<PathBuf>,
    selected: Option<PathBuf>,
    pending_load: Option<PathBuf>,
    jump: Option<(ChangelistId, PathBuf, FileAction)>,
    restore_selected: Option<PathBuf>,
    scroll_y: usize,
    scroll_x: usize,
    follow_selection: bool,
    /// True between a manual `r` refresh and the matching overview install.
    /// Overview must not bump generation again or in-flight disk reloads are dropped.
    manual_refresh: bool,
}

impl ExplorerModel {
    pub fn new(cwd: PathBuf) -> Self {
        let mut expanded = BTreeSet::new();
        expanded.insert(cwd.clone());
        Self {
            selected: Some(cwd.clone()),
            cwd,
            generation: 0,
            identity: None,
            load: ExplorerLoadState::Idle,
            listings: BTreeMap::new(),
            expanded,
            pending_load: None,
            jump: None,
            restore_selected: None,
            scroll_y: 0,
            scroll_x: 0,
            follow_selection: true,
            manual_refresh: false,
        }
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    #[must_use]
    pub fn scroll_x(&self) -> usize {
        self.scroll_x
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn load_state(&self) -> &ExplorerLoadState {
        &self.load
    }

    #[must_use]
    pub fn selected_path(&self) -> Option<&Path> {
        self.selected.as_deref()
    }

    #[must_use]
    pub fn identity(&self) -> Option<&WorkspaceIdentity> {
        self.identity.as_ref()
    }

    #[must_use]
    pub fn jump_target(&self) -> Option<(ChangelistId, PathBuf, FileAction)> {
        self.jump.clone()
    }

    #[must_use]
    pub fn selected_file_path(&self) -> Option<PathBuf> {
        let rows = self.visible_rows();
        self.selected_row(&rows)
            .filter(|row| row.kind == ExplorerEntryKind::File)
            .map(|row| row.path.clone())
    }

    #[cfg(test)]
    #[must_use]
    pub fn pending_directory(&self) -> Option<&Path> {
        self.pending_load.as_deref()
    }

    pub fn take_pending_directory(&mut self) -> Option<PathBuf> {
        self.pending_load.take()
    }

    pub fn on_overview_failed(&mut self, message: String) {
        self.load = ExplorerLoadState::Failed(message);
        self.identity = None;
    }

    /// Starts a full workspace refresh before the new `p4 info` result exists.
    /// Incrementing the generation here prevents an older directory request
    /// from repainting stale rows while identity and client mapping reload.
    pub fn begin_workspace_refresh(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.load = ExplorerLoadState::Checking;
        self.restore_selected = self.selected.clone();
        self.listings.clear();
        self.jump = None;
        self.pending_load = None;
        self.manual_refresh = true;
    }

    pub fn begin_workspace_load(&mut self, identity: WorkspaceIdentity) -> u64 {
        if self.manual_refresh && self.identity.as_ref() == Some(&identity) {
            // Disk listings were already invalidated for this `r` press. Keep
            // the generation so the in-flight directory reload can install.
            self.manual_refresh = false;
            self.identity = Some(identity);
            if self.pending_load.is_none() && !self.has_listing(&self.cwd) {
                self.pending_load = Some(self.cwd.clone());
            }
            return self.generation;
        }

        let keep_expanded = self.expanded.clone();
        let keep_selected = self.selected.clone();
        let skip_generation_bump = self.manual_refresh && self.identity.is_none();
        self.manual_refresh = false;
        self.identity = Some(identity);
        if !skip_generation_bump {
            self.generation = self.generation.wrapping_add(1);
        }
        self.load = ExplorerLoadState::Checking;
        self.listings.clear();
        self.expanded = keep_expanded;
        self.expanded.insert(self.cwd.clone());
        self.selected = keep_selected.or_else(|| Some(self.cwd.clone()));
        self.restore_selected = self.selected.clone();
        self.jump = None;
        self.pending_load = Some(self.cwd.clone());
        self.generation
    }

    #[must_use]
    pub fn remaining_expanded_directories(&self) -> Vec<PathBuf> {
        self.expanded
            .iter()
            .filter(|path| !self.paths_eq(path, &self.cwd) && !self.has_listing(path))
            .cloned()
            .collect()
    }

    pub fn install_not_in_view(&mut self, generation: u64) {
        if generation != self.generation {
            return;
        }
        self.load = ExplorerLoadState::NotInClientView;
        self.pending_load = None;
    }

    pub fn install_failure(&mut self, generation: u64, message: String) {
        if generation != self.generation {
            return;
        }
        self.load = ExplorerLoadState::Failed(message);
        self.pending_load = None;
    }

    pub fn install_root(
        &mut self,
        generation: u64,
        result: Result<LoadedDirectory, ExplorerError>,
    ) {
        if generation != self.generation {
            return;
        }
        match result {
            Ok(listing) => {
                self.load = ExplorerLoadState::Ready;
                self.install_listing(listing);
            }
            Err(ExplorerError::Query(error))
                if error.kind == crate::p4::P4ErrorKind::NotInClientView =>
            {
                self.install_not_in_view(generation);
            }
            Err(error) => self.install_failure(generation, error.to_string()),
        }
    }

    pub fn install_directory(
        &mut self,
        generation: u64,
        path: PathBuf,
        result: Result<LoadedDirectory, ExplorerError>,
    ) {
        if generation != self.generation {
            return;
        }
        match result {
            Ok(listing) => self.install_listing(listing),
            Err(_) => {
                let path = self.intern_directory_path(&path);
                self.listings.insert(path, DirectoryState::Failed);
                self.restore_selection_after_listing();
            }
        }
    }

    fn install_listing(&mut self, listing: LoadedDirectory) {
        let path = self.intern_directory_path(&listing.path);
        self.listings.insert(
            path.clone(),
            DirectoryState::Ready {
                entries: listing.entries,
            },
        );
        self.expanded.insert(path);
        self.restore_selection_after_listing();
        self.update_jump();
    }

    fn restore_selection_after_listing(&mut self) {
        let Some(target) = self.restore_selected.clone() else {
            self.clamp_selection();
            return;
        };
        let rows = self.visible_rows();
        if rows.iter().any(|row| row.path == target) {
            self.selected = Some(target);
            self.restore_selected = None;
            return;
        }
        if self.remaining_expanded_directories().is_empty() {
            self.restore_selected = None;
            self.clamp_selection();
        }
    }

    #[must_use]
    pub fn visible_rows(&self) -> Vec<VisibleExplorerRow> {
        let ready = self
            .listings
            .iter()
            .filter_map(|(path, state)| match state {
                DirectoryState::Ready { entries } => Some((path.clone(), entries.clone())),
                DirectoryState::Loading | DirectoryState::Failed => None,
            })
            .collect();
        let root_name = self
            .cwd
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| self.cwd.display().to_string());
        flatten_explorer_tree(&self.cwd, &root_name, &ready, &self.expanded)
    }

    fn selected_index(&self, rows: &[VisibleExplorerRow]) -> usize {
        self.selected
            .as_ref()
            .and_then(|path| rows.iter().position(|row| &row.path == path))
            .unwrap_or(0)
    }

    fn clamp_selection(&mut self) {
        let rows = self.visible_rows();
        if rows.is_empty() {
            self.selected = Some(self.cwd.clone());
            return;
        }
        if !self
            .selected
            .as_ref()
            .is_some_and(|path| rows.iter().any(|row| &row.path == path))
        {
            self.selected = Some(rows[0].path.clone());
        }
    }

    fn selected_row<'a>(&self, rows: &'a [VisibleExplorerRow]) -> Option<&'a VisibleExplorerRow> {
        let selected = self.selected.as_ref()?;
        rows.iter().find(|row| &row.path == selected)
    }

    pub fn move_selection(&mut self, delta: isize) {
        let rows = self.visible_rows();
        if rows.is_empty() {
            return;
        }
        let index = self.selected_index(&rows);
        let next = if delta < 0 {
            index.saturating_sub(delta.unsigned_abs())
        } else {
            index.saturating_add(delta as usize).min(rows.len() - 1)
        };
        self.selected = Some(rows[next].path.clone());
        self.follow_selection = true;
        self.update_jump();
    }

    pub fn expand_or_collapse(&mut self, expand: bool) -> ExplorerAction {
        let rows = self.visible_rows();
        let Some(row) = self.selected_row(&rows).cloned() else {
            return ExplorerAction::None;
        };
        if row.kind != ExplorerEntryKind::Directory {
            return ExplorerAction::None;
        }
        if expand {
            self.expand_directory(row.path)
        } else {
            self.expanded.remove(&row.path);
            ExplorerAction::None
        }
    }

    fn expand_directory(&mut self, path: PathBuf) -> ExplorerAction {
        let path = self.intern_directory_path(&path);
        match self.listing_state(&path).cloned() {
            Some(DirectoryState::Ready { .. }) => {
                self.expanded.insert(path);
                ExplorerAction::None
            }
            Some(DirectoryState::Loading) => ExplorerAction::None,
            Some(DirectoryState::Failed) | None => {
                self.listings.insert(path.clone(), DirectoryState::Loading);
                self.expanded.insert(path.clone());
                self.pending_load = Some(path);
                ExplorerAction::LoadDirectory
            }
        }
    }

    pub fn activate_selection(&mut self) -> ExplorerAction {
        let rows = self.visible_rows();
        let Some(row) = self.selected_row(&rows).cloned() else {
            return ExplorerAction::None;
        };
        match row.kind {
            ExplorerEntryKind::Directory => {
                if self.expanded.contains(&row.path) {
                    self.expanded.remove(&row.path);
                    ExplorerAction::None
                } else {
                    self.expand_directory(row.path)
                }
            }
            ExplorerEntryKind::File => {
                self.update_jump();
                ExplorerAction::OpenFile
            }
        }
    }

    pub fn select_index(&mut self, index: usize) {
        let rows = self.visible_rows();
        if let Some(row) = rows.get(index) {
            self.selected = Some(row.path.clone());
            self.update_jump();
        }
    }

    /// Mouse activation follows the same path as keyboard Enter: directories
    /// expand/collapse and files open in the shared content pane.
    pub fn activate_index(&mut self, index: usize) -> ExplorerAction {
        self.select_index(index);
        self.activate_selection()
    }

    fn update_jump(&mut self) {
        let rows = self.visible_rows();
        self.jump = self
            .selected_row(&rows)
            .and_then(|row| match row.decoration.as_ref()? {
                ExplorerDecoration::Opened {
                    action,
                    change: Some(change),
                } => Some((*change, row.path.clone(), action.clone())),
                _ => None,
            });
    }

    pub fn tree_window(&self, visible_rows: usize) -> (usize, usize, Vec<VisibleExplorerRow>) {
        let rows = self.visible_rows();
        if visible_rows == 0 || rows.is_empty() {
            return (0, 0, rows);
        }
        let height = visible_rows.min(rows.len());
        let max_offset = rows.len().saturating_sub(height);
        let offset = if self.follow_selection {
            self.selected_index(&rows)
                .saturating_add(1)
                .saturating_sub(height)
                .min(max_offset)
        } else {
            self.scroll_y.min(max_offset)
        };
        (offset, height, rows)
    }

    pub fn scroll_vertical(&mut self, delta: isize, visible_rows: usize) {
        let (offset, _, rows) = self.tree_window(visible_rows);
        if rows.is_empty() || visible_rows == 0 {
            return;
        }
        let max_offset = rows.len().saturating_sub(visible_rows.min(rows.len()));
        self.follow_selection = false;
        self.scroll_y = (offset as isize + delta).clamp(0, max_offset as isize) as usize;
    }

    pub fn set_scroll_x(&mut self, value: usize, view_width: usize, content_width: usize) {
        self.scroll_x = value.min(content_width.saturating_sub(view_width));
    }

    pub fn set_scroll_y(&mut self, value: usize, visible_rows: usize) {
        let rows = self.visible_rows();
        if rows.is_empty() || visible_rows == 0 {
            self.scroll_y = 0;
            self.follow_selection = false;
            return;
        }
        let max_offset = rows.len().saturating_sub(visible_rows.min(rows.len()));
        self.follow_selection = false;
        self.scroll_y = value.min(max_offset);
    }

    pub fn selected_row_info(&self) -> Option<(PathBuf, ExplorerEntryKind, bool)> {
        let rows = self.visible_rows();
        let row = self.selected_row(&rows)?;
        let opened = matches!(row.decoration, Some(ExplorerDecoration::Opened { .. }));
        Some((row.path.clone(), row.kind, opened))
    }

    pub fn selected_status(&self) -> Option<(String, String)> {
        let rows = self.visible_rows();
        let row = self.selected_row(&rows)?;
        if row.kind == ExplorerEntryKind::Directory {
            if let Some(
                decoration @ (ExplorerDecoration::NotInView | ExplorerDecoration::Unmapped),
            ) = row.decoration.as_ref()
            {
                return Some((decoration.badge().to_owned(), decoration.label()));
            }
            return Some((
                "📁".to_owned(),
                "folder status loads on expand; descendants are not scanned".to_owned(),
            ));
        }
        let decoration = row.decoration.as_ref()?;
        Some((decoration.badge().to_owned(), decoration.label()))
    }

    pub fn select_path(&mut self, path: PathBuf) {
        self.selected = Some(path);
        self.follow_selection = true;
        self.update_jump();
    }

    pub fn invalidate_directory(&mut self, path: PathBuf) {
        let path = self.intern_directory_path(&path);
        let ignore_case = self.ignore_path_case();
        // Drop in-flight listings for this directory. Otherwise a stale
        // `opened`/`read_dir` result with the previous generation can put a
        // just-deleted file back after the fresh reload.
        self.generation = self.generation.wrapping_add(1);
        self.listings.retain(|listed, _| {
            !path_components_equal(listed, &path, ignore_case)
                && !path_is_strict_descendant(listed, &path, ignore_case)
        });
        self.expanded.retain(|listed| {
            path_components_equal(listed, &path, ignore_case)
                || !path_is_strict_descendant(listed, &path, ignore_case)
        });
        // A mutation inside this directory should reveal its result once the
        // asynchronous reload completes. Keeping the parent expanded also
        // prevents an invisible child selection from aliasing to row zero.
        self.expanded.insert(path.clone());
        self.pending_load = Some(path);
    }

    fn ignore_path_case(&self) -> bool {
        if cfg!(windows) {
            return true;
        }
        matches!(
            self.identity
                .as_ref()
                .map(|identity| &identity.case_handling),
            Some(crate::domain::CaseHandling::Insensitive | crate::domain::CaseHandling::Hybrid)
                | None
        )
    }

    fn paths_eq(&self, left: &Path, right: &Path) -> bool {
        path_components_equal(left, right, self.ignore_path_case())
    }

    fn has_listing(&self, path: &Path) -> bool {
        self.listings
            .keys()
            .any(|listed| self.paths_eq(listed, path))
    }

    fn listing_state(&self, path: &Path) -> Option<&DirectoryState> {
        self.listings
            .iter()
            .find(|(listed, _)| self.paths_eq(listed, path))
            .map(|(_, state)| state)
    }

    fn intern_directory_path(&self, path: &Path) -> PathBuf {
        if self.paths_eq(path, &self.cwd) {
            return self.cwd.clone();
        }
        for state in self.listings.values() {
            if let DirectoryState::Ready { entries } = state
                && let Some(entry) = entries
                    .iter()
                    .find(|entry| self.paths_eq(&entry.path, path))
            {
                return entry.path.clone();
            }
        }
        self.expanded
            .iter()
            .find(|expanded| self.paths_eq(expanded, path))
            .cloned()
            .unwrap_or_else(|| path.to_path_buf())
    }

    #[must_use]
    pub fn format_row(row: &VisibleExplorerRow, selected: bool) -> String {
        let caret = if selected { ">" } else { " " };
        let glyph = explorer_icon(
            &row.name,
            row.kind == ExplorerEntryKind::Directory,
            row.expanded,
        );
        let indent = "  ".repeat(row.depth);
        let badge = row
            .decoration
            .as_ref()
            .map(ExplorerDecoration::badge)
            .unwrap_or("");
        if badge.is_empty() {
            format!("{caret}{indent}{glyph} {}", row.name)
        } else {
            format!("{caret}{indent}{glyph} {}  {badge}", row.name)
        }
    }

    #[cfg(test)]
    pub fn install_ready_listing_for_test(&mut self, listing: LoadedDirectory) {
        self.generation = self.generation.wrapping_add(1);
        self.load = ExplorerLoadState::Ready;
        self.identity = Some(WorkspaceIdentity {
            server_id: "server".into(),
            user: "ExampleUser".into(),
            client: "ExampleClient".into(),
            root: listing.path.clone(),
            stream: None,
            case_handling: crate::domain::CaseHandling::Insensitive,
        });
        self.cwd = listing.path.clone();
        self.expanded.insert(listing.path.clone());
        self.selected = Some(listing.path.clone());
        self.install_listing(listing);
    }
}

fn path_components_equal(left: &Path, right: &Path, ignore_case: bool) -> bool {
    if left == right {
        return true;
    }
    let left = left.components();
    let right = right.components();
    if left.clone().count() != right.clone().count() {
        return false;
    }
    left.zip(right)
        .all(|(left, right)| component_eq(&left, &right, ignore_case))
}

fn path_is_strict_descendant(path: &Path, ancestor: &Path, ignore_case: bool) -> bool {
    let ancestor: Vec<_> = ancestor.components().collect();
    let path: Vec<_> = path.components().collect();
    path.len() > ancestor.len()
        && ancestor
            .iter()
            .zip(path.iter())
            .all(|(left, right)| component_eq(left, right, ignore_case))
}

fn component_eq(left: &Component, right: &Component, ignore_case: bool) -> bool {
    if left == right {
        return true;
    }
    ignore_case && left.as_os_str().eq_ignore_ascii_case(right.as_os_str())
}

pub fn connection_message() -> &'static str {
    "the workspace path is not in the current client view; open a directory mapped by the current client"
}

pub fn open_with_default_app(path: &Path) -> Result<(), String> {
    let mut command = open_command(path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not open {}: {error}", path.display()))
}

fn open_command(path: &Path) -> Command {
    #[cfg(windows)]
    {
        // `cmd /C start` interprets metacharacters in valid Windows filenames.
        // Explorer delegates files to their registered default application
        // without passing the path through a command shell.
        let mut command = Command::new("explorer.exe");
        command.arg(path);
        command
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("open");
        command.arg(path);
        command
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let mut command = Command::new("xdg-open");
        command.arg(path);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{FileAction, FileType};

    fn file_entry(dir: &Path, name: &str, decoration: Option<ExplorerDecoration>) -> ExplorerEntry {
        ExplorerEntry {
            name: name.to_owned(),
            path: dir.join(name),
            kind: ExplorerEntryKind::File,
            decoration,
            file_type: Some(FileType::new("text")),
            have_rev: Some(1),
            head_rev: Some(1),
        }
    }

    #[test]
    fn expand_collapse_and_selection_survive_flatten() {
        let root = PathBuf::from("C:/ws");
        let mut explorer = ExplorerModel::new(root.clone());
        explorer.install_ready_listing_for_test(LoadedDirectory {
            path: root.clone(),
            entries: vec![
                ExplorerEntry {
                    name: "src".into(),
                    path: root.join("src"),
                    kind: ExplorerEntryKind::Directory,
                    decoration: None,
                    file_type: None,
                    have_rev: None,
                    head_rev: None,
                },
                file_entry(
                    &root,
                    "a.txt",
                    Some(ExplorerDecoration::Opened {
                        action: FileAction::Edit,
                        change: Some(ChangelistId::Numbered(7)),
                    }),
                ),
            ],
            truncated: false,
        });
        explorer.select_index(1);
        assert_eq!(explorer.activate_selection(), ExplorerAction::LoadDirectory);
        assert_eq!(
            explorer.pending_directory(),
            Some(root.join("src").as_path())
        );
        explorer.install_directory(
            explorer.generation(),
            root.join("src"),
            Ok(LoadedDirectory {
                path: root.join("src"),
                entries: vec![file_entry(&root.join("src"), "main.rs", None)],
                truncated: false,
            }),
        );
        let names: Vec<_> = explorer
            .visible_rows()
            .into_iter()
            .map(|row| row.name)
            .collect();
        assert_eq!(names, ["ws", "src", "main.rs", "a.txt"]);

        explorer.select_index(3);
        assert_eq!(explorer.activate_selection(), ExplorerAction::OpenFile);
        assert_eq!(
            explorer.jump_target(),
            Some((
                ChangelistId::Numbered(7),
                root.join("a.txt"),
                FileAction::Edit,
            ))
        );
    }

    #[test]
    fn mouse_style_activation_toggles_a_directory() {
        let root = PathBuf::from("C:/ws");
        let child = root.join("src");
        let mut explorer = ExplorerModel::new(root.clone());
        explorer.install_ready_listing_for_test(LoadedDirectory {
            path: root,
            entries: vec![ExplorerEntry {
                name: "src".into(),
                path: child.clone(),
                kind: ExplorerEntryKind::Directory,
                decoration: None,
                file_type: None,
                have_rev: None,
                head_rev: None,
            }],
            truncated: false,
        });

        assert_eq!(explorer.activate_index(1), ExplorerAction::LoadDirectory);
        assert_eq!(explorer.pending_directory(), Some(child.as_path()));
        explorer.install_directory(
            explorer.generation(),
            child,
            Ok(LoadedDirectory {
                path: PathBuf::from("C:/ws/src"),
                entries: Vec::new(),
                truncated: false,
            }),
        );
        assert_eq!(explorer.activate_index(1), ExplorerAction::None);
        assert_eq!(explorer.visible_rows().len(), 2);
    }

    #[test]
    fn refresh_restores_nested_selection_after_expanded_directories_reload() {
        let root = PathBuf::from("C:/ws");
        let child = root.join("src");
        let file = child.join("main.rs");
        let mut explorer = ExplorerModel::new(root.clone());
        explorer.install_ready_listing_for_test(LoadedDirectory {
            path: root.clone(),
            entries: vec![ExplorerEntry {
                name: "src".into(),
                path: child.clone(),
                kind: ExplorerEntryKind::Directory,
                decoration: None,
                file_type: None,
                have_rev: None,
                head_rev: None,
            }],
            truncated: false,
        });
        explorer.expanded.insert(child.clone());
        explorer.install_directory(
            explorer.generation(),
            child.clone(),
            Ok(LoadedDirectory {
                path: child.clone(),
                entries: vec![file_entry(&child, "main.rs", None)],
                truncated: false,
            }),
        );
        explorer.select_index(2);
        assert_eq!(explorer.selected_path(), Some(file.as_path()));

        let generation = explorer.begin_workspace_load(identity_for_test(&root));
        explorer.install_root(
            generation,
            Ok(LoadedDirectory {
                path: root.clone(),
                entries: vec![ExplorerEntry {
                    name: "src".into(),
                    path: child.clone(),
                    kind: ExplorerEntryKind::Directory,
                    decoration: None,
                    file_type: None,
                    have_rev: None,
                    head_rev: None,
                }],
                truncated: false,
            }),
        );
        assert_eq!(explorer.selected_path(), Some(file.as_path()));
        explorer.install_directory(
            generation,
            child.clone(),
            Ok(LoadedDirectory {
                path: child,
                entries: vec![file_entry(Path::new("C:/ws/src"), "main.rs", None)],
                truncated: false,
            }),
        );
        assert_eq!(explorer.selected_path(), Some(file.as_path()));
    }

    #[test]
    fn wheel_scroll_does_not_move_selection() {
        let root = PathBuf::from("C:/ws");
        let mut explorer = ExplorerModel::new(root.clone());
        let entries = (0..30)
            .map(|index| file_entry(&root, &format!("file-{index:02}.txt"), None))
            .collect();
        explorer.install_ready_listing_for_test(LoadedDirectory {
            path: root,
            entries,
            truncated: false,
        });
        let selected = explorer.selected_path().map(Path::to_path_buf);
        let (before, _, _) = explorer.tree_window(8);
        explorer.scroll_vertical(3, 8);
        let (after, _, _) = explorer.tree_window(8);
        assert_eq!(explorer.selected_path(), selected.as_deref());
        assert_eq!(before, 0);
        assert_eq!(after, 3);
        explorer.move_selection(1);
        let (followed, _, _) = explorer.tree_window(8);
        assert_eq!(followed, 0);
    }

    #[test]
    fn pointer_selection_keeps_manual_scroll() {
        let root = PathBuf::from("C:/ws");
        let mut explorer = ExplorerModel::new(root.clone());
        let entries = (0..30)
            .map(|index| file_entry(&root, &format!("file-{index:02}.txt"), None))
            .collect();
        explorer.install_ready_listing_for_test(LoadedDirectory {
            path: root,
            entries,
            truncated: false,
        });
        explorer.scroll_vertical(3, 8);
        explorer.select_index(3);
        let (offset, _, _) = explorer.tree_window(8);
        assert_eq!(offset, 3);
        assert_eq!(
            explorer.selected_path().and_then(|path| path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())),
            Some("file-02.txt".into())
        );
    }

    #[test]
    fn directory_status_reports_its_own_mapping_failure() {
        let root = PathBuf::from("C:/ws");
        let mut explorer = ExplorerModel::new(root.clone());
        explorer.install_ready_listing_for_test(LoadedDirectory {
            path: root.clone(),
            entries: vec![
                ExplorerEntry {
                    name: "outside".into(),
                    path: root.join("outside"),
                    kind: ExplorerEntryKind::Directory,
                    decoration: Some(ExplorerDecoration::NotInView),
                    file_type: None,
                    have_rev: None,
                    head_rev: None,
                },
                ExplorerEntry {
                    name: "unmapped".into(),
                    path: root.join("unmapped"),
                    kind: ExplorerEntryKind::Directory,
                    decoration: Some(ExplorerDecoration::Unmapped),
                    file_type: None,
                    have_rev: None,
                    head_rev: None,
                },
            ],
            truncated: false,
        });

        explorer.select_index(1);
        assert_eq!(
            explorer.selected_status(),
            Some(("⊘".into(), "not in view".into()))
        );
        explorer.select_index(2);
        assert_eq!(
            explorer.selected_status(),
            Some(("?".into(), "unmapped".into()))
        );
    }

    #[test]
    fn nested_mutation_reload_never_aliases_an_invisible_selection_to_root() {
        let root = PathBuf::from("C:/ws");
        let child = root.join("src");
        let created = child.join("new.txt");
        let mut explorer = ExplorerModel::new(root.clone());
        explorer.install_ready_listing_for_test(LoadedDirectory {
            path: root.clone(),
            entries: vec![ExplorerEntry {
                name: "src".into(),
                path: child.clone(),
                kind: ExplorerEntryKind::Directory,
                decoration: None,
                file_type: None,
                have_rev: None,
                head_rev: None,
            }],
            truncated: false,
        });
        explorer.select_index(1);
        explorer.select_path(created.clone());
        explorer.invalidate_directory(child.clone());

        assert_eq!(explorer.selected_path(), Some(created.as_path()));
        assert_eq!(explorer.selected_row_info(), None);
        assert!(explorer.expanded.contains(&child));

        explorer.install_directory(
            explorer.generation(),
            child.clone(),
            Ok(LoadedDirectory {
                path: child.clone(),
                entries: vec![file_entry(&child, "new.txt", None)],
                truncated: false,
            }),
        );
        assert_eq!(explorer.selected_row_info().map(|row| row.0), Some(created));
    }

    #[test]
    fn invalidate_directory_ignores_stale_listings_from_the_previous_generation() {
        let root = PathBuf::from("C:/ws");
        let gone = root.join("gone.txt");
        let mut explorer = ExplorerModel::new(root.clone());
        explorer.install_ready_listing_for_test(LoadedDirectory {
            path: root.clone(),
            entries: vec![file_entry(&root, "gone.txt", None)],
            truncated: false,
        });
        let stale_generation = explorer.generation();
        explorer.invalidate_directory(root.clone());
        assert_ne!(explorer.generation(), stale_generation);
        assert!(
            explorer
                .visible_rows()
                .iter()
                .all(|row| row.name != "gone.txt")
        );

        explorer.install_directory(
            stale_generation,
            root.clone(),
            Ok(LoadedDirectory {
                path: root.clone(),
                entries: vec![file_entry(&root, "gone.txt", None)],
                truncated: false,
            }),
        );
        assert!(
            explorer
                .visible_rows()
                .iter()
                .all(|row| row.name != "gone.txt"),
            "stale generation must not restore a deleted file"
        );

        explorer.install_directory(
            explorer.generation(),
            root,
            Ok(LoadedDirectory {
                path: PathBuf::from("C:/ws"),
                entries: Vec::new(),
                truncated: false,
            }),
        );
        assert!(explorer.visible_rows().iter().all(|row| row.path != gone));
    }

    fn identity_for_test(root: &Path) -> WorkspaceIdentity {
        WorkspaceIdentity {
            server_id: "server".into(),
            user: "user".into(),
            client: "client".into(),
            root: root.to_path_buf(),
            stream: None,
            case_handling: crate::domain::CaseHandling::Insensitive,
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_default_open_does_not_route_paths_through_cmd() {
        let command = open_command(Path::new(r"C:\ws\name&whoami.txt"));
        assert_eq!(command.get_program(), "explorer.exe");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![std::ffi::OsStr::new(r"C:\ws\name&whoami.txt")]
        );
    }
}
