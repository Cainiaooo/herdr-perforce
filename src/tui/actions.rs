//! File-tree context menu model and filesystem effects.
//!
//! UI-free so the menu shape and name validation stay unit-testable. The
//! navigation pane owns popup rendering and input routing.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuAction {
    NewFile,
    NewFolder,
    CopyPath,
    CopyRelativePath,
    Rename,
    Delete,
    OpenExternal,
    OpenDiff,
    Reveal,
    OpenChangelist,
    NewChangelist,
    DeleteChangelist,
    OpenReviewDiff,
    ToggleFileSelection,
    MoveSelectedFiles,
    CopyChangelist,
    SubmitReview,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuEntry {
    Action(MenuAction, &'static str),
    Separator,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExplorerMenuTarget {
    pub is_dir: bool,
    pub opened: bool,
    pub is_root: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ReviewMenuTarget {
    pub is_changelist: bool,
    pub is_file: bool,
    pub expanded: bool,
    pub can_submit: bool,
    pub can_delete: bool,
    pub has_checked_files: bool,
}

/// VS Code-style context menu for a tree row. `target` is `None` when the
/// click landed on empty space (workspace root: creation + reveal only).
/// The visible workspace root row is treated the same way: it must never
/// expose Rename or Delete, because confirming Delete would `remove_dir_all`
/// the checkout. Explorer stays a filesystem menu: it does not offer
/// `p4 add/edit/delete`.
#[must_use]
pub fn explorer_menu_entries(target: Option<ExplorerMenuTarget>) -> Vec<MenuEntry> {
    let mut entries = vec![
        MenuEntry::Action(MenuAction::NewFile, "New File…"),
        MenuEntry::Action(MenuAction::NewFolder, "New Folder…"),
    ];
    if let Some(row) = target.filter(|row| !row.is_dir && !row.is_root) {
        entries.extend([
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::OpenExternal, "Open with Default App"),
        ]);
        if row.opened {
            entries.push(MenuEntry::Action(MenuAction::OpenDiff, "Open Diff"));
        }
    }
    if target.is_some_and(|row| !row.is_root) {
        entries.extend([
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::CopyPath, "Copy Path"),
            MenuEntry::Action(MenuAction::CopyRelativePath, "Copy Relative Path"),
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::Rename, "Rename…"),
            MenuEntry::Action(MenuAction::Delete, "Delete"),
        ]);
    }
    entries.extend([
        MenuEntry::Separator,
        MenuEntry::Action(MenuAction::Reveal, "Reveal in File Explorer"),
    ]);
    entries
}

#[must_use]
pub fn review_menu_entries(target: ReviewMenuTarget) -> Vec<MenuEntry> {
    let mut entries = vec![MenuEntry::Action(
        MenuAction::NewChangelist,
        "New Changelist…",
    )];
    if target.is_changelist {
        entries.extend([
            MenuEntry::Separator,
            MenuEntry::Action(
                MenuAction::OpenChangelist,
                if target.expanded {
                    "Collapse Files"
                } else {
                    "Expand Files"
                },
            ),
            MenuEntry::Action(MenuAction::CopyChangelist, "Copy Changelist Number"),
        ]);
        if target.can_delete {
            entries.push(MenuEntry::Action(
                MenuAction::DeleteChangelist,
                "Delete Empty Changelist…",
            ));
        }
    }
    if target.is_file {
        entries.extend([
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::OpenReviewDiff, "Open Diff"),
            MenuEntry::Action(MenuAction::ToggleFileSelection, "Toggle File Selection"),
        ]);
    }
    if target.has_checked_files {
        entries.extend([
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::MoveSelectedFiles, "Move Selected Files…"),
        ]);
    }
    if target.can_submit {
        entries.extend([
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::SubmitReview, "Submit Review…"),
        ]);
    }
    entries.extend([
        MenuEntry::Separator,
        MenuEntry::Action(MenuAction::Reveal, "Reveal in File Explorer"),
    ]);
    entries
}

#[must_use]
pub fn first_action_index(entries: &[MenuEntry]) -> usize {
    entries
        .iter()
        .position(|entry| matches!(entry, MenuEntry::Action(..)))
        .unwrap_or(0)
}

#[must_use]
pub fn step_action_index(entries: &[MenuEntry], selected: usize, delta: isize) -> usize {
    if entries.is_empty() {
        return 0;
    }
    let mut index = selected as isize;
    let last = entries.len().saturating_sub(1) as isize;
    for _ in 0..entries.len() {
        index = (index + delta).clamp(0, last);
        if matches!(entries[index as usize], MenuEntry::Action(..)) {
            return index as usize;
        }
        if (delta < 0 && index == 0) || (delta > 0 && index == last) {
            break;
        }
    }
    selected
}

/// First visible menu entry so the highlighted row stays on screen when the
/// popup is shorter than the full list.
#[must_use]
pub fn menu_window(selected: usize, count: usize, inner_height: usize) -> usize {
    if inner_height == 0 || count == 0 {
        return 0;
    }
    let height = inner_height.min(count);
    selected
        .saturating_add(1)
        .saturating_sub(height)
        .min(count.saturating_sub(height))
}

#[must_use]
pub fn validate_name(input: &str) -> Option<&str> {
    let name = input.trim();
    (!name.is_empty() && !name.contains(['/', '\\', ':']) && name != "." && name != "..")
        .then_some(name)
}

fn child_path(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let name = validate_name(name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "enter a file name without path separators",
        )
    })?;
    Ok(dir.join(name))
}

fn entry_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

#[must_use]
pub fn is_same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.to_str(), right.to_str()) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

fn is_same_directory_entry(left: &Path, right: &Path) -> bool {
    if is_same_path(left, right) {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

pub fn create_file(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let path = child_path(dir, name)?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    Ok(path)
}

pub fn create_folder(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let path = child_path(dir, name)?;
    std::fs::create_dir(&path)?;
    Ok(path)
}

pub fn rename(path: &Path, new_name: &str) -> io::Result<PathBuf> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to rename a filesystem root",
        )
    })?;
    let target = child_path(parent, new_name)?;
    if path == target {
        return Ok(target);
    }
    if entry_exists(&target) {
        if is_same_directory_entry(path, &target) {
            return rename_case_only(path, &target);
        }
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{new_name} already exists"),
        ));
    }
    std::fs::rename(path, &target)?;
    Ok(target)
}

fn rename_case_only(from: &Path, to: &Path) -> io::Result<PathBuf> {
    let parent = from.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to rename a filesystem root",
        )
    })?;
    let mut serial = 0u32;
    let temp = loop {
        let candidate = parent.join(format!(".{}.ren-{}", std::process::id(), serial));
        if !entry_exists(&candidate) {
            break candidate;
        }
        serial = serial.saturating_add(1);
        if serial > 1_000 {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate a temporary rename path",
            ));
        }
    };
    std::fs::rename(from, &temp)?;
    if let Err(error) = std::fs::rename(&temp, to) {
        let _ = std::fs::rename(&temp, from);
        return Err(error);
    }
    Ok(to.to_path_buf())
}

pub fn delete(path: &Path, is_dir: bool) -> io::Result<()> {
    if path.parent().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to delete a filesystem root",
        ));
    }
    if is_dir {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

pub fn copy_to_clipboard(text: &str) -> io::Result<()> {
    #[cfg(windows)]
    let candidates: &[&[&str]] = &[&["clip"]];
    #[cfg(not(windows))]
    let candidates: &[&[&str]] = &[
        &["pbcopy"],
        &["wl-copy"],
        &["xclip", "-selection", "clipboard"],
    ];

    let mut last_err = io::Error::new(io::ErrorKind::NotFound, "no clipboard tool found");
    for argv in candidates {
        match copy_with(argv, text) {
            Ok(()) => return Ok(()),
            Err(err) => last_err = err,
        }
    }
    Err(last_err)
}

fn copy_with(argv: &[&str], text: &str) -> io::Result<()> {
    let mut child = Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let Some(mut stdin) = child.stdin.take() else {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!("{} opened without stdin", argv[0]),
        ));
    };
    stdin.write_all(text.as_bytes())?;
    drop(stdin);
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} exited with {status}",
            argv[0]
        )))
    }
}

/// Open the platform file manager with the path selected (best-effort).
pub fn reveal(path: &Path) {
    #[cfg(windows)]
    {
        let _ = Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open")
            .arg("-R")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let parent = path.parent().unwrap_or(path);
        let _ = Command::new("xdg-open")
            .arg(parent)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

pub fn relative_path_text(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(entries: &[MenuEntry], action: MenuAction) -> bool {
        entries
            .iter()
            .any(|entry| matches!(entry, MenuEntry::Action(found, _) if *found == action))
    }

    #[test]
    fn explorer_menu_shape_for_rows_and_empty_space() {
        let file = explorer_menu_entries(Some(ExplorerMenuTarget {
            is_dir: false,
            opened: false,
            is_root: false,
        }));
        assert!(has(&file, MenuAction::NewFile));
        assert!(has(&file, MenuAction::Rename));
        assert!(has(&file, MenuAction::CopyPath));
        assert!(has(&file, MenuAction::Reveal));
        assert!(has(&file, MenuAction::OpenExternal));
        assert!(!has(&file, MenuAction::OpenDiff));
        assert!(!has(&file, MenuAction::SubmitReview));

        let opened = explorer_menu_entries(Some(ExplorerMenuTarget {
            is_dir: false,
            opened: true,
            is_root: false,
        }));
        assert!(has(&opened, MenuAction::OpenDiff));

        let dir = explorer_menu_entries(Some(ExplorerMenuTarget {
            is_dir: true,
            opened: false,
            is_root: false,
        }));
        assert!(!has(&dir, MenuAction::OpenExternal));
        assert!(has(&dir, MenuAction::Delete));

        let empty = explorer_menu_entries(None);
        assert!(!has(&empty, MenuAction::Rename));
        assert!(!has(&empty, MenuAction::Delete));
        assert!(has(&empty, MenuAction::NewFolder));
        assert!(has(&empty, MenuAction::Reveal));

        let root = explorer_menu_entries(Some(ExplorerMenuTarget {
            is_dir: true,
            opened: false,
            is_root: true,
        }));
        assert!(has(&root, MenuAction::NewFile));
        assert!(has(&root, MenuAction::Reveal));
        assert!(!has(&root, MenuAction::Rename));
        assert!(!has(&root, MenuAction::Delete));
        assert!(!has(&root, MenuAction::CopyPath));
    }

    #[test]
    fn review_menu_exposes_inline_tree_and_management_actions_by_row_kind() {
        let pending = review_menu_entries(ReviewMenuTarget {
            is_changelist: true,
            can_submit: true,
            can_delete: true,
            ..ReviewMenuTarget::default()
        });
        assert!(has(&pending, MenuAction::NewChangelist));
        assert!(has(&pending, MenuAction::OpenChangelist));
        assert!(has(&pending, MenuAction::DeleteChangelist));
        assert!(has(&pending, MenuAction::SubmitReview));

        let file = review_menu_entries(ReviewMenuTarget {
            is_file: true,
            has_checked_files: true,
            ..ReviewMenuTarget::default()
        });
        assert!(has(&file, MenuAction::OpenReviewDiff));
        assert!(has(&file, MenuAction::ToggleFileSelection));
        assert!(has(&file, MenuAction::MoveSelectedFiles));
        assert!(!has(&file, MenuAction::SubmitReview));
    }

    #[test]
    fn validate_name_rejects_paths_and_empty() {
        assert_eq!(validate_name("  Foo.cpp  "), Some("Foo.cpp"));
        assert_eq!(validate_name(""), None);
        assert_eq!(validate_name("a/b"), None);
        assert_eq!(validate_name("a\\b"), None);
        assert_eq!(validate_name(".."), None);
    }

    #[test]
    fn step_action_index_skips_separators() {
        let entries = explorer_menu_entries(Some(ExplorerMenuTarget {
            is_dir: false,
            opened: true,
            is_root: false,
        }));
        let first = first_action_index(&entries);
        assert!(matches!(entries[first], MenuEntry::Action(..)));
        let next = step_action_index(&entries, first, 1);
        assert!(next > first);
        assert!(matches!(entries[next], MenuEntry::Action(..)));
    }

    #[test]
    fn create_rename_and_delete_round_trip() {
        let root = std::env::temp_dir().join(format!(
            "herdr-p4-actions-{}-{}",
            std::process::id(),
            "round"
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let file = create_file(&root, "a.txt").unwrap();
        assert!(file.is_file());
        assert!(create_file(&root, "a.txt").is_err());
        let renamed = rename(&file, "b.txt").unwrap();
        assert!(renamed.is_file());
        assert!(!file.exists());
        delete(&renamed, false).unwrap();
        assert!(!renamed.exists());
        let folder = create_folder(&root, "dir").unwrap();
        assert!(folder.is_dir());
        delete(&folder, true).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn menu_window_keeps_the_selection_visible() {
        assert_eq!(menu_window(0, 12, 5), 0);
        assert_eq!(menu_window(11, 12, 5), 7);
        assert_eq!(menu_window(3, 12, 5), 0);
        assert_eq!(menu_window(6, 12, 5), 2);
    }

    #[test]
    fn delete_refuses_a_filesystem_root() {
        assert!(delete(Path::new("/"), true).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_case_only_rename_succeeds() {
        let root =
            std::env::temp_dir().join(format!("herdr-p4-actions-{}-case", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let file = create_file(&root, "Foo.cpp").unwrap();
        let renamed = rename(&file, "foo.cpp").unwrap();
        assert_eq!(
            renamed.file_name().and_then(|name| name.to_str()),
            Some("foo.cpp")
        );
        assert!(renamed.is_file());
        let _ = std::fs::remove_dir_all(&root);
    }
}
