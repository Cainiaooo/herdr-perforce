//! Local workspace listing and read-only P4 decorations for File Explorer.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
};

use crate::domain::{
    CaseHandling, ChangelistId, ExplorerEntry, ExplorerEntryKind, FileAction, FileP4Facts,
    FileType, MAX_DIRECTORY_ENTRIES, MAX_PREVIEW_BYTES, MAX_PREVIEW_LINES, PreviewContent,
    WorkspaceIdentity, decoration_from_facts, preview_from_bytes,
};

use super::{
    P4Client, P4Error, P4ErrorKind, P4Query, P4Transport, changed_files_from_opened,
    config::{
        path_comparison_is_case_insensitive, path_is_within_root, path_lookup_key,
        strip_verbatim_prefix,
    },
    parser::{RecordCode, StructuredRecord, parse_revision_value},
};

const QUERY_PATH_BATCH: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplorerError {
    OutsideClientRoot,
    Io(String),
    Query(P4Error),
}

impl std::fmt::Display for ExplorerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutsideClientRoot => formatter
                .write_str("the path is outside the current client root and cannot be listed"),
            Self::Io(message) => formatter.write_str(message),
            Self::Query(error) => error.fmt(formatter),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEntry {
    pub name: String,
    pub path: PathBuf,
    pub kind: ExplorerEntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedDirectory {
    pub path: PathBuf,
    pub entries: Vec<ExplorerEntry>,
    pub truncated: bool,
}

pub fn cwd_is_in_client_view<T: P4Transport>(
    client: &P4Client<T>,
    cwd: &Path,
) -> Result<bool, P4Error> {
    match client.run(&P4Query::Where {
        path: cwd.join("..."),
    }) {
        Ok(response) => Ok(response.records.iter().any(|record| {
            matches!(record.code, RecordCode::Stat) && record.field("depotFile").is_some()
        })),
        Err(error) if error.kind == P4ErrorKind::NotInClientView => Ok(false),
        Err(error) => Err(error),
    }
}

pub fn load_opened_records<T: P4Transport>(
    client: &P4Client<T>,
) -> Result<Vec<StructuredRecord>, P4Error> {
    Ok(client.run_records(&P4Query::OpenedOnClient)?.records)
}

pub fn load_explorer_directory<T: P4Transport>(
    client: &P4Client<T>,
    identity: &WorkspaceIdentity,
    directory: &Path,
) -> Result<LoadedDirectory, ExplorerError> {
    let ignore_case = path_comparison_is_case_insensitive(&identity.case_handling);
    if !path_is_within_root(directory, &identity.root, ignore_case) {
        return Err(ExplorerError::OutsideClientRoot);
    }

    let mut local = list_local_directory(directory, &identity.root, &identity.case_handling)
        .map_err(ExplorerError::Io)?;
    let mut truncated = local.len() > MAX_DIRECTORY_ENTRIES;
    local.truncate(MAX_DIRECTORY_ENTRIES);

    // `*` preserves directory-level lazy loading; `...` would scan descendants.
    let opened_records = client
        .run_records(&P4Query::OpenedInDirectory {
            directory: directory.to_path_buf(),
        })
        .map(|response| response.records)
        .unwrap_or_default();

    let paths: Vec<PathBuf> = local.iter().map(|entry| entry.path.clone()).collect();
    let mut where_records = match collect_path_records(client, QueryKind::Where, &paths) {
        Ok(records) => records,
        Err(_) => {
            return Ok(LoadedDirectory {
                path: directory.to_path_buf(),
                entries: undecorated(local),
                truncated,
            });
        }
    };

    let opened_depots = opened_depot_paths(&opened_records);
    if !opened_depots.is_empty() {
        if let Ok(opened_where) = collect_path_records(client, QueryKind::Where, &opened_depots) {
            where_records.extend(opened_where);
        }
    }

    truncated |= add_missing_opened_entries(
        &mut local,
        directory,
        identity,
        &where_records,
        &opened_records,
    );
    if local.is_empty() {
        return Ok(LoadedDirectory {
            path: directory.to_path_buf(),
            entries: Vec::new(),
            truncated,
        });
    }

    let paths: Vec<PathBuf> = local.iter().map(|entry| entry.path.clone()).collect();
    let fstat_records = match collect_path_records(client, QueryKind::Fstat, &paths) {
        Ok(records) => records,
        Err(_) => {
            return Ok(LoadedDirectory {
                path: directory.to_path_buf(),
                entries: undecorated(local),
                truncated,
            });
        }
    };

    Ok(LoadedDirectory {
        path: directory.to_path_buf(),
        entries: decorate_entries(
            local,
            identity,
            &where_records,
            &fstat_records,
            &opened_records,
        ),
        truncated,
    })
}

fn opened_depot_paths(records: &[StructuredRecord]) -> Vec<PathBuf> {
    records
        .iter()
        .filter(|record| matches!(record.code, RecordCode::Stat))
        .filter_map(|record| record.string("depotFile"))
        .map(PathBuf::from)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn add_missing_opened_entries(
    entries: &mut Vec<LocalEntry>,
    directory: &Path,
    identity: &WorkspaceIdentity,
    where_records: &[StructuredRecord],
    opened_records: &[StructuredRecord],
) -> bool {
    let ignore_case = path_comparison_is_case_insensitive(&identity.case_handling);
    let depot_to_local: BTreeMap<String, PathBuf> = where_records
        .iter()
        .filter(|record| matches!(record.code, RecordCode::Stat))
        .filter_map(|record| Some((record.string("depotFile")?, record_local_path(record)?)))
        .collect();
    let mut existing: BTreeSet<String> = entries
        .iter()
        .map(|entry| path_lookup_key(&entry.path, ignore_case))
        .collect();
    let directory_key = path_lookup_key(directory, ignore_case);
    let Ok(opened) = changed_files_from_opened(opened_records) else {
        return false;
    };
    let mut truncated = false;
    for file in opened {
        if !matches!(file.action, FileAction::Delete | FileAction::MoveDelete) {
            continue;
        }
        let Some(path) = depot_to_local.get(&file.depot_path) else {
            continue;
        };
        if path
            .parent()
            .map(|parent| path_lookup_key(parent, ignore_case))
            .as_deref()
            != Some(directory_key.as_str())
        {
            continue;
        }
        let key = path_lookup_key(path, ignore_case);
        if existing.contains(&key) {
            continue;
        }
        if entries.len() >= MAX_DIRECTORY_ENTRIES {
            truncated = true;
            continue;
        }
        let Some(name) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        entries.push(LocalEntry {
            name,
            path: path.clone(),
            kind: ExplorerEntryKind::File,
        });
        existing.insert(key);
    }
    entries.sort_by(|left, right| match (left.kind, right.kind) {
        (ExplorerEntryKind::Directory, ExplorerEntryKind::File) => std::cmp::Ordering::Less,
        (ExplorerEntryKind::File, ExplorerEntryKind::Directory) => std::cmp::Ordering::Greater,
        _ => compare_entry_names(&left.name, &right.name, ignore_case),
    });
    truncated
}

#[derive(Clone, Copy)]
enum QueryKind {
    Where,
    Fstat,
}

fn collect_path_records<T: P4Transport>(
    client: &P4Client<T>,
    kind: QueryKind,
    paths: &[PathBuf],
) -> Result<Vec<StructuredRecord>, P4Error> {
    let mut records = Vec::new();
    for chunk in paths.chunks(QUERY_PATH_BATCH) {
        let query = match kind {
            QueryKind::Where => P4Query::WhereMany {
                paths: chunk.to_vec(),
            },
            QueryKind::Fstat => P4Query::Fstat {
                paths: chunk.to_vec(),
            },
        };
        records.extend(client.run_records(&query)?.records);
    }
    Ok(records)
}

fn undecorated(entries: Vec<LocalEntry>) -> Vec<ExplorerEntry> {
    entries
        .into_iter()
        .map(|entry| ExplorerEntry {
            name: entry.name,
            path: entry.path,
            kind: entry.kind,
            decoration: None,
            file_type: None,
            have_rev: None,
            head_rev: None,
        })
        .collect()
}

pub fn list_local_directory(
    directory: &Path,
    client_root: &Path,
    case_handling: &CaseHandling,
) -> Result<Vec<LocalEntry>, String> {
    let ignore_case = path_comparison_is_case_insensitive(case_handling);
    if !path_is_within_root(directory, client_root, ignore_case) {
        return Err("directory is outside the current client root".to_owned());
    }

    let canonical_root = fs::canonicalize(client_root).ok();
    let mut entries = Vec::new();
    let read = fs::read_dir(directory)
        .map_err(|error| format!("could not read directory {}: {error}", directory.display()))?;
    for child in read {
        let child = child.map_err(|error| {
            format!(
                "could not read a directory entry in {}: {error}",
                directory.display()
            )
        })?;
        let path = child.path();
        if !path_stays_in_client_root(&path, client_root, canonical_root.as_deref(), ignore_case) {
            continue;
        }
        let file_type = child
            .file_type()
            .map_err(|error| format!("could not read file type for {}: {error}", path.display()))?;
        let kind = if file_type.is_dir() {
            ExplorerEntryKind::Directory
        } else if file_type.is_symlink() {
            match fs::metadata(&path) {
                Ok(metadata) if metadata.is_dir() => ExplorerEntryKind::Directory,
                Ok(_) => ExplorerEntryKind::File,
                Err(_) => ExplorerEntryKind::File,
            }
        } else {
            ExplorerEntryKind::File
        };
        let name = child.file_name().to_string_lossy().into_owned();
        entries.push(LocalEntry { name, path, kind });
    }

    entries.sort_by(|left, right| match (left.kind, right.kind) {
        (ExplorerEntryKind::Directory, ExplorerEntryKind::File) => std::cmp::Ordering::Less,
        (ExplorerEntryKind::File, ExplorerEntryKind::Directory) => std::cmp::Ordering::Greater,
        _ => compare_entry_names(&left.name, &right.name, ignore_case),
    });
    Ok(entries)
}

fn path_stays_in_client_root(
    path: &Path,
    client_root: &Path,
    canonical_root: Option<&Path>,
    ignore_case: bool,
) -> bool {
    if !path_is_within_root(path, client_root, ignore_case) {
        return false;
    }
    let Ok(canonical) = fs::canonicalize(path) else {
        return true;
    };
    match canonical_root {
        Some(root) => path_is_within_root(&canonical, root, ignore_case),
        None => true,
    }
}

fn compare_entry_names(left: &str, right: &str, ignore_case: bool) -> std::cmp::Ordering {
    if ignore_case {
        left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase())
    } else {
        left.cmp(right)
    }
}

pub fn decorate_entries(
    entries: Vec<LocalEntry>,
    identity: &WorkspaceIdentity,
    where_records: &[StructuredRecord],
    fstat_records: &[StructuredRecord],
    opened_records: &[StructuredRecord],
) -> Vec<ExplorerEntry> {
    let ignore_case = path_comparison_is_case_insensitive(&identity.case_handling);
    let (where_index, where_unscoped_failure) = index_where_records(where_records, ignore_case);
    let (fstat_index, fstat_failed, fstat_unscoped_failure) =
        index_fstat_records(fstat_records, ignore_case);
    let opened_index = index_opened_records(opened_records, ignore_case, &where_index);

    entries
        .into_iter()
        .map(|entry| {
            let key = path_lookup_key(&entry.path, ignore_case);
            let facts = facts_for_entry(
                &key,
                entry.kind == ExplorerEntryKind::File,
                where_index.get(&key),
                fstat_index.get(&key),
                opened_index.get(&key),
                where_unscoped_failure || fstat_unscoped_failure || fstat_failed.contains(&key),
            );
            let file_type = facts.file_type.clone();
            let have_rev = facts.have_rev;
            let head_rev = facts.head_rev;
            ExplorerEntry {
                name: entry.name,
                path: entry.path,
                kind: entry.kind,
                decoration: decoration_from_facts(&facts),
                file_type,
                have_rev,
                head_rev,
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
enum WhereFact {
    Mapped { depot_file: Option<String> },
    NotInView,
    QueryFailed,
}

#[derive(Debug, Clone)]
struct FstatFact {
    mapped: bool,
    action: Option<FileAction>,
    change: Option<ChangelistId>,
    have_rev: Option<u64>,
    head_rev: Option<u64>,
    file_type: Option<FileType>,
}

#[derive(Debug, Clone)]
struct OpenedFact {
    action: FileAction,
    change: ChangelistId,
}

fn facts_for_entry(
    _key: &str,
    is_file: bool,
    where_fact: Option<&WhereFact>,
    fstat: Option<&FstatFact>,
    opened: Option<&OpenedFact>,
    query_failed: bool,
) -> FileP4Facts {
    let not_in_view = matches!(where_fact, Some(WhereFact::NotInView));
    let mapped = match (where_fact, fstat) {
        (Some(WhereFact::NotInView | WhereFact::QueryFailed), _) => false,
        (_, Some(fstat)) => fstat.mapped,
        (Some(WhereFact::Mapped { .. }), None) => true,
        (None, None) => false,
    };
    let (opened_action, opened_change) = if let Some(opened) = opened {
        (Some(opened.action.clone()), Some(opened.change))
    } else if let Some(fstat) = fstat {
        (fstat.action.clone(), fstat.change)
    } else {
        (None, None)
    };
    FileP4Facts {
        not_in_view,
        mapped,
        untracked: is_file && mapped && fstat.is_none() && opened.is_none() && !query_failed,
        opened_action,
        opened_change,
        have_rev: fstat.and_then(|fact| fact.have_rev),
        head_rev: fstat.and_then(|fact| fact.head_rev),
        file_type: fstat.and_then(|fact| fact.file_type.clone()),
        query_failed: query_failed || matches!(where_fact, Some(WhereFact::QueryFailed)),
    }
}

fn index_where_records(
    records: &[StructuredRecord],
    ignore_case: bool,
) -> (BTreeMap<String, WhereFact>, bool) {
    let mut index = BTreeMap::new();
    let mut unscoped_failure = false;
    for record in records {
        match record.code {
            RecordCode::Stat => {
                if let Some(path) = record_local_path(record) {
                    index.insert(
                        path_lookup_key(&path, ignore_case),
                        WhereFact::Mapped {
                            depot_file: record.string("depotFile"),
                        },
                    );
                }
            }
            RecordCode::Error => {
                if let Some(path) = path_from_where_error(record) {
                    index.insert(path_lookup_key(&path, ignore_case), WhereFact::NotInView);
                } else if let Some(path) = path_from_any_error(record) {
                    index.insert(path_lookup_key(&path, ignore_case), WhereFact::QueryFailed);
                } else {
                    unscoped_failure = true;
                }
            }
            _ => {}
        }
    }
    (index, unscoped_failure)
}

fn index_fstat_records(
    records: &[StructuredRecord],
    ignore_case: bool,
) -> (BTreeMap<String, FstatFact>, BTreeSet<String>, bool) {
    let mut index = BTreeMap::new();
    let mut failed = BTreeSet::new();
    let mut unscoped_failure = false;
    for record in records {
        if matches!(record.code, RecordCode::Error) {
            if let Some(path) = path_from_any_error(record) {
                if !is_expected_fstat_miss(record) {
                    failed.insert(path_lookup_key(&path, ignore_case));
                }
            } else {
                unscoped_failure = true;
            }
            continue;
        }
        if !matches!(record.code, RecordCode::Stat) {
            continue;
        }
        let Some(path) = record_local_path(record) else {
            continue;
        };
        index.insert(
            path_lookup_key(&path, ignore_case),
            FstatFact {
                mapped: record.field("isMapped").is_some(),
                action: record
                    .string("action")
                    .map(|value| FileAction::from_p4(&value)),
                change: record
                    .string("change")
                    .and_then(|value| parse_change_id(&value)),
                have_rev: record
                    .string("haveRev")
                    .and_then(|value| parse_revision_value(&value).ok().flatten()),
                head_rev: record
                    .string("headRev")
                    .and_then(|value| parse_revision_value(&value).ok().flatten()),
                file_type: record
                    .string("type")
                    .or_else(|| record.string("headType"))
                    .map(FileType::new),
            },
        );
    }
    (index, failed, unscoped_failure)
}

fn is_expected_fstat_miss(record: &StructuredRecord) -> bool {
    record.string("data").is_some_and(|data| {
        let data = data.to_ascii_lowercase();
        data.contains("no such file(s)")
            || data.contains("no such file")
            || data.contains("not in client view")
            || data.contains("client's root")
    })
}

fn index_opened_records(
    records: &[StructuredRecord],
    ignore_case: bool,
    where_index: &BTreeMap<String, WhereFact>,
) -> BTreeMap<String, OpenedFact> {
    let depot_to_local: BTreeMap<String, String> = where_index
        .iter()
        .filter_map(|(local_key, fact)| match fact {
            WhereFact::Mapped {
                depot_file: Some(depot),
            } => Some((depot.clone(), local_key.clone())),
            _ => None,
        })
        .collect();

    let Ok(files) = changed_files_from_opened(records) else {
        return BTreeMap::new();
    };
    let mut index = BTreeMap::new();
    for file in files {
        let fact = OpenedFact {
            action: file.action,
            change: change_from_opened_record(records, &file.depot_path)
                .unwrap_or(ChangelistId::Default),
        };
        if let Some(local) = file
            .client_path
            .as_ref()
            .filter(|path| is_filesystem_path(path))
        {
            index.insert(path_lookup_key(local, ignore_case), fact.clone());
        }
        if let Some(local_key) = depot_to_local.get(&file.depot_path) {
            index.insert(local_key.clone(), fact);
        }
    }
    index
}

fn change_from_opened_record(
    records: &[StructuredRecord],
    depot_path: &str,
) -> Option<ChangelistId> {
    records.iter().find_map(|record| {
        if record.string("depotFile").as_deref() != Some(depot_path) {
            return None;
        }
        record
            .string("change")
            .and_then(|value| parse_change_id(&value))
    })
}

fn parse_change_id(value: &str) -> Option<ChangelistId> {
    if value.eq_ignore_ascii_case("default") {
        return Some(ChangelistId::Default);
    }
    value.parse().ok().map(ChangelistId::Numbered)
}

fn is_filesystem_path(path: &Path) -> bool {
    let raw = path.as_os_str().to_string_lossy();
    !raw.starts_with("//")
}

fn record_local_path(record: &StructuredRecord) -> Option<PathBuf> {
    record
        .string("path")
        .or_else(|| {
            record
                .string("clientFile")
                .filter(|value| !value.starts_with("//"))
        })
        .map(PathBuf::from)
}

fn path_from_where_error(record: &StructuredRecord) -> Option<PathBuf> {
    let data = record.string("data")?;
    if let Some((path, rest)) = data.split_once(" - ") {
        if rest.to_ascii_lowercase().contains("client view")
            || rest.to_ascii_lowercase().contains("client's root")
        {
            return Some(PathBuf::from(path.trim()));
        }
    }
    let lower = data.to_ascii_lowercase();
    if !lower.contains("client view") && !lower.contains("client's root") {
        return None;
    }
    let start = data.find('\'')?;
    let rest = &data[start + 1..];
    let end = rest.find('\'')?;
    Some(PathBuf::from(&rest[..end]))
}

fn path_from_any_error(record: &StructuredRecord) -> Option<PathBuf> {
    let data = record.string("data")?;
    let (path, _) = data.split_once(" - ")?;
    let path = PathBuf::from(path.trim());
    is_filesystem_path(&path).then_some(path)
}

pub fn read_workspace_preview(
    path: &Path,
    file_type: Option<&FileType>,
    have_rev: Option<u64>,
    head_rev: Option<u64>,
) -> PreviewContent {
    let local_size = fs::metadata(path).ok().map(|metadata| metadata.len());
    match read_preview_bytes(path) {
        Ok((bytes, truncated)) => {
            preview_from_bytes(&bytes, truncated, file_type, local_size, have_rev, head_rev)
        }
        Err(error) => PreviewContent::Failed {
            message: format!("could not read {}: {error}", display_path(path)),
        },
    }
}

/// Loads the unified workspace diff for an opened file.
///
/// This intentionally uses raw `p4 diff` output: `-Mj` would turn diagnostics
/// into records but would not preserve the unified diff stream.
pub fn load_workspace_diff<T: P4Transport>(
    client: &P4Client<T>,
    path: &Path,
) -> Result<Vec<String>, P4Error> {
    let output = client.run_raw(
        vec![
            OsString::from("diff"),
            OsString::from("-du"),
            super::command::escape_p4_file_arg(path.as_os_str()),
        ],
        Vec::new(),
    )?;
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| super::error::known_error(P4ErrorKind::MalformedOutput))?;
    let mut lines = text.lines().map(ToOwned::to_owned).collect::<Vec<_>>();
    if lines.len() > MAX_PREVIEW_LINES {
        lines.truncate(MAX_PREVIEW_LINES);
        lines.push(format!(
            "truncated: {MAX_PREVIEW_LINES} line diff budget exceeded"
        ));
    }
    Ok(lines)
}

fn read_preview_bytes(path: &Path) -> io::Result<(Vec<u8>, bool)> {
    let file = File::open(path)?;
    let mut limited = file.take(MAX_PREVIEW_BYTES as u64 + 1);
    let mut bytes = Vec::new();
    limited.read_to_end(&mut bytes)?;
    let truncated = bytes.len() > MAX_PREVIEW_BYTES;
    if truncated {
        bytes.truncate(MAX_PREVIEW_BYTES);
    }
    Ok((bytes, truncated))
}

fn display_path(path: &Path) -> String {
    strip_verbatim_prefix(path).display().to_string()
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use std::collections::BTreeMap;

    use super::*;
    use crate::{
        domain::ExplorerDecoration,
        p4::{
            fake::FakeP4Transport,
            parse_json_records,
            transport::{P4Client, RawP4Output},
        },
    };

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "herdr-p4-explorer-{tag}-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).expect("temp tree");
            Self { root }
        }

        fn mkdir(&self, relative: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(&path).expect("dir");
            path
        }

        fn write(&self, relative: &str, contents: &[u8]) -> PathBuf {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent");
            }
            fs::write(&path, contents).expect("write");
            path
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn identity_for(root: &Path) -> WorkspaceIdentity {
        WorkspaceIdentity {
            server_id: "server".into(),
            user: "ExampleUser".into(),
            client: "ExampleClient".into(),
            root: root.to_path_buf(),
            stream: None,
            case_handling: CaseHandling::Insensitive,
        }
    }

    fn json_line(fields: &str) -> String {
        format!(r#"{{"code":"stat",{fields}}}"#)
    }

    fn parse(lines: &str) -> Vec<StructuredRecord> {
        parse_json_records(lines.as_bytes()).expect("records")
    }

    #[test]
    fn local_listing_stays_inside_client_root_and_sorts_directories_first() {
        let tree = TempTree::new("list");
        tree.mkdir("src");
        tree.write("README.md", b"hi");
        tree.write("src/main.rs", b"fn main() {}");
        tree.mkdir("Docs");
        let identity = identity_for(&tree.root);
        let entries = list_local_directory(&tree.root, &identity.root, &identity.case_handling)
            .expect("list");
        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["Docs", "src", "README.md"]);
        let outside = list_local_directory(
            tree.root.parent().expect("parent"),
            &identity.root,
            &identity.case_handling,
        );
        assert!(outside.is_err());
    }

    #[test]
    fn decorations_cover_opened_unopened_out_of_date_and_not_in_view() {
        let tree = TempTree::new("deco");
        let edit = tree.write("edit.rs", b"e");
        let add = tree.write("add.rs", b"a");
        let unopened = tree.write("clean.rs", b"c");
        let stale = tree.write("stale.rs", b"s");
        let excluded = tree.write("secret.rs", b"x");
        let local =
            list_local_directory(&tree.root, &tree.root, &CaseHandling::Insensitive).expect("list");

        let where_records = parse(&format!(
            "{}\n{}\n{}\n{}\n{}\n",
            json_line(&format!(
                r#""depotFile":"//d/edit.rs","path":{}"#,
                json_path(&edit)
            )),
            json_line(&format!(
                r#""depotFile":"//d/add.rs","path":{}"#,
                json_path(&add)
            )),
            json_line(&format!(
                r#""depotFile":"//d/clean.rs","path":{}"#,
                json_path(&unopened)
            )),
            json_line(&format!(
                r#""depotFile":"//d/stale.rs","path":{}"#,
                json_path(&stale)
            )),
            serde_json::json!({
                "code": "error",
                "data": format!("{} - file(s) not in client view.\n", excluded.display())
            }),
        ));
        let fstat_records = parse(&format!(
            "{}\n{}\n{}\n{}\n",
            json_line(&format!(
                r#""depotFile":"//d/edit.rs","path":{},"isMapped":"1","action":"edit","change":"42","type":"text","haveRev":"2","headRev":"2""#,
                json_path(&edit)
            )),
            json_line(&format!(
                r#""depotFile":"//d/add.rs","path":{},"isMapped":"1","action":"add","change":"default","type":"text","haveRev":"none","headRev":"1""#,
                json_path(&add)
            )),
            json_line(&format!(
                r#""depotFile":"//d/clean.rs","path":{},"isMapped":"1","type":"text","haveRev":"4","headRev":"4""#,
                json_path(&unopened)
            )),
            json_line(&format!(
                r#""depotFile":"//d/stale.rs","path":{},"isMapped":"1","type":"text","haveRev":"1","headRev":"5""#,
                json_path(&stale)
            )),
        ));

        let decorated = decorate_entries(
            local,
            &identity_for(&tree.root),
            &where_records,
            &fstat_records,
            &[],
        );
        let by_name: BTreeMap<_, _> = decorated
            .into_iter()
            .map(|entry| (entry.name.clone(), entry.decoration))
            .collect();
        assert!(matches!(
            by_name["edit.rs"],
            Some(ExplorerDecoration::Opened {
                action: FileAction::Edit,
                change: Some(ChangelistId::Numbered(42)),
            })
        ));
        assert!(matches!(
            by_name["add.rs"],
            Some(ExplorerDecoration::Opened {
                action: FileAction::Add,
                change: Some(ChangelistId::Default),
            })
        ));
        assert_eq!(by_name["clean.rs"], Some(ExplorerDecoration::Unopened));
        assert_eq!(by_name["stale.rs"], Some(ExplorerDecoration::OutOfDate));
        assert_eq!(by_name["secret.rs"], Some(ExplorerDecoration::NotInView));
    }

    #[test]
    fn mapped_local_file_missing_from_fstat_is_untracked() {
        let tree = TempTree::new("untracked");
        let path = tree.write("notes.txt", b"local only");
        let local =
            list_local_directory(&tree.root, &tree.root, &CaseHandling::Insensitive).expect("list");
        let where_records = parse(&format!(
            "{}\n",
            json_line(&format!(
                r#""depotFile":"//d/notes.txt","path":{}"#,
                json_path(&path)
            ))
        ));
        let fstat_records = parse(
            &serde_json::json!({
                "code": "error",
                "data": format!("{} - no such file(s).\n", path.display())
            })
            .to_string(),
        );
        let decorated = decorate_entries(
            local,
            &identity_for(&tree.root),
            &where_records,
            &fstat_records,
            &[],
        );
        assert_eq!(decorated[0].decoration, Some(ExplorerDecoration::Untracked));
    }

    #[test]
    fn opened_delete_is_restored_as_a_lazy_directory_ghost_row() {
        let tree = TempTree::new("deleted-ghost");
        let deleted = tree.root.join("gone.txt");
        let where_records = parse(&format!(
            "{}\n",
            json_line(&format!(
                r#""depotFile":"//d/gone.txt","path":{}"#,
                json_path(&deleted)
            ))
        ));
        let opened_records = parse(
            r#"{"code":"stat","depotFile":"//d/gone.txt","clientFile":"//ExampleClient/gone.txt","action":"delete","change":"42","type":"text","rev":"3"}"#,
        );
        let mut entries = Vec::new();
        let truncated = add_missing_opened_entries(
            &mut entries,
            &tree.root,
            &identity_for(&tree.root),
            &where_records,
            &opened_records,
        );
        assert!(!truncated);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "gone.txt");
        assert_eq!(entries[0].path, deleted);
    }

    fn json_path(path: &Path) -> String {
        format!("\"{}\"", path.display().to_string().replace('\\', "\\\\"))
    }

    #[test]
    fn missing_file_is_a_preview_failure_not_an_empty_document() {
        let tree = TempTree::new("missing");
        let path = tree.root.join("gone.txt");
        let preview = read_workspace_preview(&path, None, None, None);
        assert!(preview.is_failure());
        assert!(!matches!(
            preview,
            PreviewContent::Text {
                lines, ..
            } if lines.is_empty()
        ));
    }

    #[test]
    fn empty_workspace_file_previews_as_empty_text() {
        let tree = TempTree::new("empty");
        let path = tree.write("empty.txt", b"");
        let preview = read_workspace_preview(&path, Some(&FileType::new("text")), Some(1), Some(1));
        assert_eq!(
            preview,
            PreviewContent::Text {
                lines: Vec::new(),
                truncated: None,
            }
        );
    }

    #[test]
    fn cwd_where_without_mapping_records_is_not_in_view() {
        let fake = FakeP4Transport::default();
        fake.push_output(RawP4Output {
            stdout: br#"{"code":"error","data":"... - file(s) not in client view.\n"}"#.to_vec(),
            stderr: Vec::new(),
            exit_code: 1,
            elapsed: Duration::from_millis(1),
        });
        let client = P4Client::new_with_directory_environment(
            fake,
            "p4",
            PathBuf::from("C:/Example"),
            BTreeMap::new(),
        );
        let mapped = cwd_is_in_client_view(&client, Path::new("C:/Example")).expect("classified");
        assert!(!mapped);
    }

    #[test]
    fn load_directory_omits_decorations_when_fstat_cannot_run() {
        let tree = TempTree::new("fail-deco");
        tree.write("a.txt", b"a");
        let fake = FakeP4Transport::default();
        fake.push_output(RawP4Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::ZERO,
        });
        fake.push_output(RawP4Output {
            stdout: format!(
                "{}\n",
                json_line(&format!(
                    r#""depotFile":"//d/a.txt","path":{}"#,
                    json_path(&tree.root.join("a.txt"))
                ))
            )
            .into_bytes(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::ZERO,
        });
        fake.push_error(crate::p4::TransportError::TimedOut);
        let client =
            P4Client::new_with_directory_environment(fake, "p4", &tree.root, BTreeMap::new());
        let loaded = load_explorer_directory(&client, &identity_for(&tree.root), &tree.root)
            .expect("local listing still works");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].decoration, None);
    }

    #[test]
    fn opened_query_failure_keeps_independent_file_decorations() {
        let tree = TempTree::new("opened-failure");
        let root_file = tree.write("root.txt", b"root");
        let fake = FakeP4Transport::default();
        fake.push_error(crate::p4::TransportError::TimedOut);
        fake.push_output(RawP4Output {
            stdout: format!(
                "{}\n",
                json_line(&format!(
                    r#""depotFile":"//d/root.txt","path":{}"#,
                    json_path(&root_file)
                ))
            )
            .into_bytes(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::ZERO,
        });
        fake.push_output(RawP4Output {
            stdout: format!(
                "{}\n",
                json_line(&format!(
                    r#""depotFile":"//d/root.txt","path":{},"isMapped":"1","type":"text","haveRev":"1","headRev":"2""#,
                    json_path(&root_file)
                ))
            )
            .into_bytes(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::ZERO,
        });
        let client =
            P4Client::new_with_directory_environment(fake, "p4", &tree.root, BTreeMap::new());

        let loaded = load_explorer_directory(&client, &identity_for(&tree.root), &tree.root)
            .expect("opened discovery is optional");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(
            loaded.entries[0].decoration,
            Some(ExplorerDecoration::OutOfDate)
        );
    }

    #[test]
    fn opened_where_failure_skips_ghosts_without_losing_local_decorations() {
        let tree = TempTree::new("opened-where-failure");
        let root_file = tree.write("root.txt", b"root");
        let fake = FakeP4Transport::default();
        fake.push_output(RawP4Output {
            stdout: format!(
                "{}\n",
                json_line(
                    r#""depotFile":"//d/gone.txt","clientFile":"//ExampleClient/gone.txt","action":"delete","change":"42","type":"text","rev":"3""#
                )
            )
            .into_bytes(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::ZERO,
        });
        fake.push_output(RawP4Output {
            stdout: format!(
                "{}\n",
                json_line(&format!(
                    r#""depotFile":"//d/root.txt","path":{}"#,
                    json_path(&root_file)
                ))
            )
            .into_bytes(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::ZERO,
        });
        fake.push_error(crate::p4::TransportError::TimedOut);
        fake.push_output(RawP4Output {
            stdout: format!(
                "{}\n",
                json_line(&format!(
                    r#""depotFile":"//d/root.txt","path":{},"isMapped":"1","type":"text","haveRev":"1","headRev":"1""#,
                    json_path(&root_file)
                ))
            )
            .into_bytes(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::ZERO,
        });
        let client =
            P4Client::new_with_directory_environment(fake, "p4", &tree.root, BTreeMap::new());

        let loaded = load_explorer_directory(&client, &identity_for(&tree.root), &tree.root)
            .expect("ghost recovery is optional");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].name, "root.txt");
        assert_eq!(
            loaded.entries[0].decoration,
            Some(ExplorerDecoration::Unopened)
        );
    }

    #[test]
    fn directory_load_queries_only_immediate_rows_and_scopes_opened() {
        let tree = TempTree::new("lazy-scope");
        let child = tree.mkdir("src");
        let root_file = tree.write("root.txt", b"root");
        let nested = tree.write("src/nested.txt", b"nested");
        let fake = FakeP4Transport::default();
        fake.push_output(RawP4Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::ZERO,
        });
        fake.push_output(RawP4Output {
            stdout: format!(
                "{}\n{}\n",
                json_line(&format!(
                    r#""depotFile":"//d/src","path":{}"#,
                    json_path(&child)
                )),
                json_line(&format!(
                    r#""depotFile":"//d/root.txt","path":{}"#,
                    json_path(&root_file)
                ))
            )
            .into_bytes(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::ZERO,
        });
        fake.push_output(RawP4Output {
            stdout: format!(
                "{}\n{}\n",
                serde_json::json!({
                    "code": "error",
                    "data": format!("{} - no such file(s).\n", child.display())
                }),
                json_line(&format!(
                    r#""depotFile":"//d/root.txt","path":{},"isMapped":"1","type":"text","haveRev":"1","headRev":"1""#,
                    json_path(&root_file)
                ))
            )
            .into_bytes(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::ZERO,
        });
        let client = P4Client::new_with_directory_environment(
            fake.clone(),
            "p4",
            &tree.root,
            BTreeMap::new(),
        );

        let loaded = load_explorer_directory(&client, &identity_for(&tree.root), &tree.root)
            .expect("root directory");
        assert_eq!(loaded.entries.len(), 2);
        let requests = fake.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].args[2], "opened");
        assert!(requests[0].args[3].to_string_lossy().ends_with('*'));
        for request in &requests {
            assert!(
                request.args.iter().all(|arg| arg != nested.as_os_str()),
                "collapsed descendant leaked into request: {:?}",
                request.args
            );
        }
    }

    #[test]
    fn mixed_where_errors_do_not_fail_the_directory_load() {
        let tree = TempTree::new("mixed");
        let mapped = tree.write("ok.txt", b"ok");
        let skipped = tree.write("skip.txt", b"no");
        let fake = FakeP4Transport::default();
        fake.push_output(RawP4Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::ZERO,
        });
        fake.push_output(RawP4Output {
            stdout: format!(
                "{}\n{}\n",
                json_line(&format!(
                    r#""depotFile":"//d/ok.txt","path":{}"#,
                    json_path(&mapped)
                )),
                serde_json::json!({
                    "code": "error",
                    "data": format!("{} - file(s) not in client view.\n", skipped.display())
                })
            )
            .into_bytes(),
            stderr: Vec::new(),
            exit_code: 1,
            elapsed: Duration::ZERO,
        });
        fake.push_output(RawP4Output {
            stdout: format!(
                "{}\n",
                json_line(&format!(
                    r#""depotFile":"//d/ok.txt","path":{},"isMapped":"1","type":"text","haveRev":"1","headRev":"1""#,
                    json_path(&mapped)
                ))
            )
            .into_bytes(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::ZERO,
        });
        let client =
            P4Client::new_with_directory_environment(fake, "p4", &tree.root, BTreeMap::new());
        let loaded = load_explorer_directory(&client, &identity_for(&tree.root), &tree.root)
            .expect("mixed where");
        let by_name: BTreeMap<_, _> = loaded
            .entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.decoration.clone()))
            .collect();
        assert_eq!(by_name["ok.txt"], Some(ExplorerDecoration::Unopened));
        assert_eq!(by_name["skip.txt"], Some(ExplorerDecoration::NotInView));
    }

    #[test]
    fn scoped_fstat_failure_leaves_only_that_entry_undecorated() {
        let tree = TempTree::new("scoped-failure");
        let ok = tree.write("ok.txt", b"ok");
        let failed = tree.write("failed.txt", b"no");
        let local = list_local_directory(&tree.root, &tree.root, &CaseHandling::Insensitive)
            .expect("listing");
        let where_records = parse(&format!(
            "{}\n{}\n",
            json_line(&format!(
                r#""depotFile":"//d/ok.txt","path":{}"#,
                json_path(&ok)
            )),
            json_line(&format!(
                r#""depotFile":"//d/failed.txt","path":{}"#,
                json_path(&failed)
            ))
        ));
        let fstat_records = parse(&format!(
            "{}\n{}\n",
            json_line(&format!(
                r#""path":{},"isMapped":"1","haveRev":"1","headRev":"1""#,
                json_path(&ok)
            )),
            serde_json::json!({
                "code": "error",
                "data": format!("{} - unexpected fstat failure.\n", failed.display())
            })
        ));
        let decorated = decorate_entries(
            local,
            &identity_for(&tree.root),
            &where_records,
            &fstat_records,
            &[],
        );
        let by_name: BTreeMap<_, _> = decorated
            .into_iter()
            .map(|entry| (entry.name, entry.decoration))
            .collect();
        assert_eq!(by_name["ok.txt"], Some(ExplorerDecoration::Unopened));
        assert_eq!(by_name["failed.txt"], None);
    }

    #[test]
    fn workspace_diff_is_raw_bounded_read_only_output() {
        let fake = FakeP4Transport::default();
        fake.push_output(RawP4Output {
            stdout: b"--- //d/a.txt\n+++ C:/ws/a.txt\n@@ -1 +1 @@\n-old\n+new\n".to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::ZERO,
        });
        let client =
            P4Client::new_with_directory_environment(fake.clone(), "p4", "C:/ws", BTreeMap::new());
        let lines = load_workspace_diff(&client, Path::new("C:/ws/a#1.txt")).expect("diff");
        assert_eq!(lines[2], "@@ -1 +1 @@");
        let requests = fake.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].args,
            ["diff", "-du", "C:/ws/a%231.txt"].map(OsString::from)
        );
    }
}
