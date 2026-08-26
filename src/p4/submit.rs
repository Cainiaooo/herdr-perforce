use std::{
    collections::BTreeMap,
    error::Error,
    ffi::OsString,
    fmt,
    fs::{self, File},
    io::{self, Read},
    path::Path,
    time::Duration,
};

use blake3::Hasher;

use crate::domain::{
    ChangedFile, Changelist, ChangelistId, ChangelistStatus, ContentToken, FileAction, SpecToken,
    WorkspaceIdentity, compute_spec_token,
};

use super::{
    DescriptionApplyError, DomainMappingError, P4Error, P4ErrorKind, P4Query, P4Transport,
    RecordCode, changed_files_from_opened, changelist_from_describe,
    description::{P4WriteService, canonical_description},
    parser::{parse_revision_value, workspace_from_info},
};

const MAX_SUBMIT_FILES: usize = 4_096;
const HASH_BUFFER_BYTES: usize = 64 * 1024;
const DEFAULT_DESCRIPTION_PLACEHOLDER: &str = "<enter description here>";
/// Submit write and post-submit describe can exceed the 30s read default on a
/// large changelist. Timing out after the server accepted the change must not
/// look like a retryable query failure.
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitIntent {
    Cancel,
    Escape,
    Close,
    LoseFocus,
    Enter,
    SubmitButton,
    CtrlEnter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitBlockReason {
    NotPending,
    NotOwnedByCurrentUser,
    NotCurrentClient,
    EmptyDescription,
    PlaceholderDescription,
    NoFiles,
    TooManyFiles,
    UnresolvedFiles,
    OutOfDateFiles,
    UnmappedLocalFile,
    MissingLocalFile,
    UnreadableLocalFile,
    NonRegularLocalFile,
    UnsupportedAction,
    UnsupportedFileType,
    SnapshotChangedDuringPreflight,
    FileListChangedDuringPreflight,
    FileMetadataChangedDuringPreflight,
}

impl fmt::Display for SubmitBlockReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotPending => "the changelist is not pending",
            Self::NotOwnedByCurrentUser => "the changelist is owned by another user",
            Self::NotCurrentClient => "the changelist belongs to another client",
            Self::EmptyDescription => "the changelist description is empty",
            Self::PlaceholderDescription => "the changelist still has the default description",
            Self::NoFiles => "the changelist has no open files",
            Self::TooManyFiles => "the changelist exceeds the submit file budget",
            Self::UnresolvedFiles => "the changelist contains unresolved files",
            Self::OutOfDateFiles => "the changelist contains files not opened at head revision",
            Self::UnmappedLocalFile => "an open file is not safely mapped into this workspace",
            Self::MissingLocalFile => "a required local file is missing",
            Self::UnreadableLocalFile => "a required local file cannot be read",
            Self::NonRegularLocalFile => "a required local path is not a regular file",
            Self::UnsupportedAction => "the changelist contains an unsupported file action",
            Self::UnsupportedFileType => "the changelist contains an unsupported file type",
            Self::SnapshotChangedDuringPreflight => {
                "the changelist changed while preflight was running"
            }
            Self::FileListChangedDuringPreflight => {
                "the open file list changed while preflight was running"
            }
            Self::FileMetadataChangedDuringPreflight => {
                "open file metadata changed while preflight was running"
            }
        })
    }
}

#[derive(Debug)]
pub enum SubmitError {
    Query {
        stage: &'static str,
        source: P4Error,
    },
    Mapping {
        stage: &'static str,
        source: DomainMappingError,
    },
    InvalidSnapshot,
    Blocked(SubmitBlockReason),
    Stale,
    AlreadyRunning,
    TimedOut {
        stage: &'static str,
    },
    VerificationFailed,
}

impl fmt::Display for SubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Query { stage, source } => write!(formatter, "Submit {stage} failed: {source}"),
            Self::Mapping { stage, source } => {
                write!(formatter, "Submit could not map {stage}: {source}")
            }
            Self::InvalidSnapshot => {
                formatter.write_str("Submit refused an inconsistent changelist snapshot")
            }
            Self::Blocked(reason) => write!(formatter, "Submit is disabled because {reason}"),
            Self::Stale => formatter
                .write_str("Submit confirmation is stale; refresh and review the changelist again"),
            Self::AlreadyRunning => formatter.write_str("Submit is already running"),
            Self::TimedOut { stage } => write!(
                formatter,
                "Submit {stage} timed out; refresh server state before retrying"
            ),
            Self::VerificationFailed => formatter
                .write_str("Perforce accepted Submit but the refreshed changelist did not match"),
        }
    }
}

impl Error for SubmitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Query { source, .. } => Some(source),
            Self::Mapping { source, .. } => Some(source),
            Self::InvalidSnapshot
            | Self::Blocked(_)
            | Self::Stale
            | Self::AlreadyRunning
            | Self::TimedOut { .. }
            | Self::VerificationFailed => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubmitActionCounts {
    pub adds: usize,
    pub edits: usize,
    pub deletes: usize,
    pub branches: usize,
    pub move_adds: usize,
    pub move_deletes: usize,
    pub integrates: usize,
    pub imports: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitPreview {
    pub change: u64,
    pub description: String,
    pub file_count: usize,
    pub actions: SubmitActionCounts,
    pub spec_token: SpecToken,
    pub content_token: ContentToken,
    workspace: WorkspaceIdentity,
    changelist: Changelist,
}

impl SubmitPreview {
    #[must_use]
    pub const fn default_intent() -> SubmitIntent {
        SubmitIntent::Cancel
    }

    #[must_use]
    pub fn authorize(self, intent: SubmitIntent) -> Option<AuthorizedSubmit> {
        matches!(intent, SubmitIntent::SubmitButton | SubmitIntent::CtrlEnter).then_some(
            AuthorizedSubmit {
                change: self.change,
                expected_spec_token: self.spec_token,
                expected_content_token: self.content_token,
                expected_workspace: self.workspace,
                expected_changelist: self.changelist,
            },
        )
    }
}

#[derive(Debug)]
pub struct AuthorizedSubmit {
    change: u64,
    expected_spec_token: SpecToken,
    expected_content_token: ContentToken,
    expected_workspace: WorkspaceIdentity,
    expected_changelist: Changelist,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmitResult {
    pub requested_change: u64,
    pub submitted_change: u64,
    pub file_count: usize,
}

impl<T: P4Transport> P4WriteService<T> {
    pub fn preview_submit(&self, change: u64) -> Result<SubmitPreview, SubmitError> {
        let snapshot = self.load_submit_snapshot(change)?;
        Ok(SubmitPreview {
            change,
            description: snapshot.changelist.description.clone(),
            file_count: snapshot.changelist.files.len(),
            actions: action_counts(&snapshot.changelist.files),
            spec_token: snapshot.spec_token,
            content_token: snapshot.content_token,
            workspace: snapshot.workspace,
            changelist: snapshot.changelist,
        })
    }

    pub fn submit_change(
        &self,
        authorization: AuthorizedSubmit,
    ) -> Result<SubmitResult, SubmitError> {
        let _flight = self.try_begin_submit().ok_or(SubmitError::AlreadyRunning)?;
        let snapshot = self.load_submit_snapshot(authorization.change)?;
        if snapshot.spec_token != authorization.expected_spec_token
            || snapshot.content_token != authorization.expected_content_token
            || snapshot.workspace != authorization.expected_workspace
        {
            return Err(SubmitError::Stale);
        }

        let change_arg = authorization.change.to_string();
        self.client
            .run_structured_with_timeout(
                ["-ztag", "-Mj", "submit", "-c", &change_arg]
                    .map(OsString::from)
                    .to_vec(),
                Vec::new(),
                SUBMIT_TIMEOUT,
            )
            .map_err(|source| map_submit_command_error("write", source))?;

        let (workspace, refreshed) = self.load_submitted_changelist(authorization.change)?;
        if workspace != authorization.expected_workspace
            || !submitted_matches(&refreshed, &authorization.expected_changelist)
        {
            return Err(SubmitError::VerificationFailed);
        }

        Ok(SubmitResult {
            requested_change: authorization.change,
            submitted_change: authorization.change,
            file_count: refreshed.files.len(),
        })
    }

    fn load_submit_snapshot(&self, change: u64) -> Result<SubmitSnapshot, SubmitError> {
        let mut base = self
            .load_snapshot(change)
            .map_err(map_description_snapshot_error)?;
        validate_submit_eligibility(&base.workspace, &base.changelist)?;

        let file_limit = (MAX_SUBMIT_FILES + 1).to_string();
        let change_arg = change.to_string();
        let response = self
            .client
            .run_structured(
                [
                    "-ztag",
                    "-Mj",
                    "fstat",
                    "-m",
                    &file_limit,
                    "-e",
                    &change_arg,
                    "-Ro",
                    "-Or",
                    "//...",
                ]
                .map(OsString::from)
                .to_vec(),
                Vec::new(),
            )
            .map_err(|source| SubmitError::Query {
                stage: "file preflight",
                source,
            })?;
        validate_fstat_preflight(&response.records, change)?;
        let opened = changed_files_from_opened(&response.records).map_err(|source| {
            SubmitError::Mapping {
                stage: "open files",
                source,
            }
        })?;
        reconcile_open_files(&base.workspace, &mut base.changelist.files, &opened)?;
        let spec_token = compute_spec_token(&base.workspace, &base.changelist);
        let content_token = hash_submit_content(&base.workspace, &opened)?;

        Ok(SubmitSnapshot {
            workspace: base.workspace,
            changelist: base.changelist,
            spec_token,
            content_token,
        })
    }

    fn load_submitted_changelist(
        &self,
        change: u64,
    ) -> Result<(WorkspaceIdentity, Changelist), SubmitError> {
        let info = self
            .client
            .run(&P4Query::Info)
            .map_err(|source| map_submit_command_error("post-submit workspace refresh", source))?;
        let workspace =
            workspace_from_info(&info.records).map_err(|source| SubmitError::Mapping {
                stage: "post-submit workspace identity",
                source,
            })?;
        let describe = self
            .client
            .run_with_timeout(&P4Query::DescribeSummary { change }, SUBMIT_TIMEOUT)
            .map_err(|source| map_submit_command_error("post-submit changelist refresh", source))?;
        let changelist =
            changelist_from_describe(&describe.records).map_err(|source| SubmitError::Mapping {
                stage: "post-submit changelist",
                source,
            })?;
        Ok((workspace, changelist))
    }
}

struct SubmitSnapshot {
    workspace: WorkspaceIdentity,
    changelist: Changelist,
    spec_token: SpecToken,
    content_token: ContentToken,
}

fn map_description_snapshot_error(error: DescriptionApplyError) -> SubmitError {
    match error {
        DescriptionApplyError::Query { stage, source } => SubmitError::Query { stage, source },
        DescriptionApplyError::Mapping { stage, source } => SubmitError::Mapping { stage, source },
        DescriptionApplyError::InvalidForm
        | DescriptionApplyError::InvalidDescription
        | DescriptionApplyError::NoChange
        | DescriptionApplyError::Ineligible(_)
        | DescriptionApplyError::Stale
        | DescriptionApplyError::VerificationFailed => SubmitError::InvalidSnapshot,
    }
}

fn map_submit_command_error(stage: &'static str, source: P4Error) -> SubmitError {
    if source.kind == P4ErrorKind::TimedOut {
        SubmitError::TimedOut { stage }
    } else {
        SubmitError::Query { stage, source }
    }
}

fn validate_submit_eligibility(
    workspace: &WorkspaceIdentity,
    changelist: &Changelist,
) -> Result<(), SubmitError> {
    if changelist.id == ChangelistId::Default || changelist.status != ChangelistStatus::Pending {
        return Err(SubmitError::Blocked(SubmitBlockReason::NotPending));
    }
    if changelist.owner != workspace.user {
        return Err(SubmitError::Blocked(
            SubmitBlockReason::NotOwnedByCurrentUser,
        ));
    }
    if changelist.client != workspace.client {
        return Err(SubmitError::Blocked(SubmitBlockReason::NotCurrentClient));
    }
    let description = canonical_description(&changelist.description);
    if description.trim().is_empty() {
        return Err(SubmitError::Blocked(SubmitBlockReason::EmptyDescription));
    }
    if description
        .trim()
        .eq_ignore_ascii_case(DEFAULT_DESCRIPTION_PLACEHOLDER)
    {
        return Err(SubmitError::Blocked(
            SubmitBlockReason::PlaceholderDescription,
        ));
    }
    if changelist.files.is_empty() {
        return Err(SubmitError::Blocked(SubmitBlockReason::NoFiles));
    }
    if changelist.files.len() > MAX_SUBMIT_FILES {
        return Err(SubmitError::Blocked(SubmitBlockReason::TooManyFiles));
    }
    Ok(())
}

fn validate_fstat_preflight(
    records: &[super::StructuredRecord],
    change: u64,
) -> Result<(), SubmitError> {
    let change_arg = change.to_string();
    let mut file_count = 0usize;
    for record in records.iter().filter(|record| {
        matches!(record.code, RecordCode::Stat) && record.field("depotFile").is_some()
    }) {
        file_count += 1;
        if file_count > MAX_SUBMIT_FILES {
            return Err(SubmitError::Blocked(SubmitBlockReason::TooManyFiles));
        }
        if record.string("change").as_deref() != Some(change_arg.as_str()) {
            return Err(SubmitError::Blocked(
                SubmitBlockReason::SnapshotChangedDuringPreflight,
            ));
        }
        if record.field("isMapped").is_none() {
            return Err(SubmitError::Blocked(SubmitBlockReason::UnmappedLocalFile));
        }
        let unresolved = optional_u64(record, "unresolved")?;
        if unresolved.is_some_and(|count| count > 0) {
            return Err(SubmitError::Blocked(SubmitBlockReason::UnresolvedFiles));
        }
        let head = optional_revision(record, "headRev")?;
        let have = optional_revision(record, "haveRev")?;
        let resolved = optional_u64(record, "resolved")?;
        if head.zip(have).is_some_and(|(head, have)| head != have)
            && resolved.is_none_or(|count| count == 0)
        {
            return Err(SubmitError::Blocked(SubmitBlockReason::OutOfDateFiles));
        }
    }
    Ok(())
}

fn optional_u64(record: &super::StructuredRecord, field: &str) -> Result<Option<u64>, SubmitError> {
    record
        .string(field)
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| SubmitError::InvalidSnapshot)
        })
        .transpose()
}

fn optional_revision(
    record: &super::StructuredRecord,
    field: &str,
) -> Result<Option<u64>, SubmitError> {
    match record.string(field) {
        None => Ok(None),
        Some(value) => parse_revision_value(&value).map_err(|()| SubmitError::InvalidSnapshot),
    }
}

fn reconcile_open_files(
    workspace: &WorkspaceIdentity,
    described: &mut [ChangedFile],
    opened: &[ChangedFile],
) -> Result<(), SubmitError> {
    if described.len() != opened.len() {
        return Err(SubmitError::Blocked(
            SubmitBlockReason::FileListChangedDuringPreflight,
        ));
    }
    let opened_by_path = opened
        .iter()
        .map(|file| {
            (
                workspace.case_handling.canonical_path_key(&file.depot_path),
                file,
            )
        })
        .collect::<BTreeMap<_, _>>();
    if opened_by_path.len() != opened.len() {
        return Err(SubmitError::InvalidSnapshot);
    }
    for described_file in described {
        let key = workspace
            .case_handling
            .canonical_path_key(&described_file.depot_path);
        let Some(opened_file) = opened_by_path.get(&key) else {
            return Err(SubmitError::Blocked(
                SubmitBlockReason::FileListChangedDuringPreflight,
            ));
        };
        if described_file.depot_path != opened_file.depot_path
            || described_file.action != opened_file.action
            || described_file.file_type != opened_file.file_type
            || !described_move_matches_opened(
                described_file.moved_from.as_deref(),
                opened_file.moved_from.as_deref(),
            )
            || !described_move_matches_opened(
                described_file.moved_to.as_deref(),
                opened_file.moved_to.as_deref(),
            )
        {
            return Err(SubmitError::Blocked(
                SubmitBlockReason::FileMetadataChangedDuringPreflight,
            ));
        }
        // `p4 describe -s` omits movedFileN; copy fstat endpoints so spec_token
        // still fingerprints the rename pair.
        described_file
            .moved_from
            .clone_from(&opened_file.moved_from);
        described_file.moved_to.clone_from(&opened_file.moved_to);
    }
    Ok(())
}

fn described_move_matches_opened(described: Option<&str>, opened: Option<&str>) -> bool {
    described.is_none_or(|path| opened == Some(path))
}

fn hash_submit_content(
    workspace: &WorkspaceIdentity,
    files: &[ChangedFile],
) -> Result<ContentToken, SubmitError> {
    let canonical_root = fs::canonicalize(&workspace.root)
        .map_err(|_| SubmitError::Blocked(SubmitBlockReason::UnmappedLocalFile))?;
    let mut files = files.iter().collect::<Vec<_>>();
    files.sort_by(|left, right| {
        workspace
            .case_handling
            .canonical_path_key(&left.depot_path)
            .cmp(
                &workspace
                    .case_handling
                    .canonical_path_key(&right.depot_path),
            )
            .then_with(|| left.depot_path.cmp(&right.depot_path))
    });

    let mut token = Hasher::new();
    write_sized(&mut token, b"herdr-p4/content-token/v1");
    write_sized(&mut token, workspace.server_id.as_bytes());
    write_sized(&mut token, workspace.user.as_bytes());
    write_sized(&mut token, workspace.client.as_bytes());
    for file in files {
        write_sized(&mut token, file.depot_path.as_bytes());
        write_sized(&mut token, file.action.canonical_name().as_bytes());
        write_sized(&mut token, file.file_type.as_str().as_bytes());
        match file.action {
            FileAction::Delete | FileAction::MoveDelete => {
                write_sized(&mut token, b"no-content");
            }
            FileAction::Add
            | FileAction::Edit
            | FileAction::Branch
            | FileAction::MoveAdd
            | FileAction::Integrate
            | FileAction::Import => {
                if file.file_type.as_str().split('+').next() == Some("symlink") {
                    return Err(SubmitError::Blocked(SubmitBlockReason::UnsupportedFileType));
                }
                let path = file
                    .client_path
                    .as_deref()
                    .ok_or(SubmitError::Blocked(SubmitBlockReason::UnmappedLocalFile))?;
                let canonical = canonical_local_file(path, &canonical_root)?;
                let digest = hash_file(&canonical)?;
                write_sized(&mut token, b"content");
                write_sized(&mut token, digest.as_bytes());
            }
            FileAction::Purge | FileAction::Archive | FileAction::Unknown(_) => {
                return Err(SubmitError::Blocked(SubmitBlockReason::UnsupportedAction));
            }
        }
    }
    Ok(ContentToken::from_bytes(*token.finalize().as_bytes()))
}

fn canonical_local_file(
    path: &Path,
    canonical_root: &Path,
) -> Result<std::path::PathBuf, SubmitError> {
    if !path.is_absolute() {
        return Err(SubmitError::Blocked(SubmitBlockReason::UnmappedLocalFile));
    }
    let canonical = fs::canonicalize(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => SubmitError::Blocked(SubmitBlockReason::MissingLocalFile),
        _ => SubmitError::Blocked(SubmitBlockReason::UnreadableLocalFile),
    })?;
    if !canonical.starts_with(canonical_root) {
        return Err(SubmitError::Blocked(SubmitBlockReason::UnmappedLocalFile));
    }
    let metadata = fs::metadata(&canonical)
        .map_err(|_| SubmitError::Blocked(SubmitBlockReason::UnreadableLocalFile))?;
    if !metadata.is_file() {
        return Err(SubmitError::Blocked(SubmitBlockReason::NonRegularLocalFile));
    }
    Ok(canonical)
}

fn hash_file(path: &Path) -> Result<blake3::Hash, SubmitError> {
    let mut file = File::open(path)
        .map_err(|_| SubmitError::Blocked(SubmitBlockReason::UnreadableLocalFile))?;
    let mut hasher = Hasher::new();
    let mut buffer = [0u8; HASH_BUFFER_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| SubmitError::Blocked(SubmitBlockReason::UnreadableLocalFile))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize())
}

fn write_sized(hasher: &mut Hasher, bytes: &[u8]) {
    hasher.update(
        &u64::try_from(bytes.len())
            .expect("field length must fit in u64")
            .to_le_bytes(),
    );
    hasher.update(bytes);
}

fn action_counts(files: &[ChangedFile]) -> SubmitActionCounts {
    let mut counts = SubmitActionCounts::default();
    for file in files {
        match file.action {
            FileAction::Add => counts.adds += 1,
            FileAction::Edit => counts.edits += 1,
            FileAction::Delete => counts.deletes += 1,
            FileAction::Branch => counts.branches += 1,
            FileAction::MoveAdd => counts.move_adds += 1,
            FileAction::MoveDelete => counts.move_deletes += 1,
            FileAction::Integrate => counts.integrates += 1,
            FileAction::Import => counts.imports += 1,
            FileAction::Purge | FileAction::Archive | FileAction::Unknown(_) => {}
        }
    }
    counts
}

fn submitted_matches(refreshed: &Changelist, expected: &Changelist) -> bool {
    refreshed.id == expected.id
        && refreshed.status == ChangelistStatus::Submitted
        && refreshed.owner == expected.owner
        && refreshed.client == expected.client
        && canonical_description(&refreshed.description)
            == canonical_description(&expected.description)
        && file_projection(&refreshed.files) == file_projection(&expected.files)
}

fn file_projection(files: &[ChangedFile]) -> BTreeMap<&str, (&FileAction, &str)> {
    files
        .iter()
        .map(|file| {
            (
                file.depot_path.as_str(),
                (&file.action, file.file_type.as_str()),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        time::Duration,
    };

    use serde_json::json;

    use super::*;
    use crate::{
        domain::{CaseHandling, FileType},
        p4::{P4Client, RawP4Output, TransportError, fake::FakeP4Transport},
    };

    struct TestWorkspace {
        root: PathBuf,
        text: PathBuf,
        binary: PathBuf,
    }

    impl TestWorkspace {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "herdr-submit-unit-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            fs::create_dir_all(&root).expect("temp workspace");
            let text = root.join("a.txt");
            let binary = root.join("b.bin");
            fs::write(&text, b"one\n").expect("text fixture");
            fs::write(&binary, [0, 1, 0, 255]).expect("binary fixture");
            Self { root, text, binary }
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn output(stdout: impl Into<Vec<u8>>) -> RawP4Output {
        RawP4Output {
            stdout: stdout.into(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::from_millis(1),
        }
    }

    fn info(root: &Path) -> Vec<u8> {
        json!({
            "clientName": "ExampleClientA",
            "clientRoot": root,
            "serverAddress": "127.0.0.1:1666",
            "userName": "ExampleAuthor",
            "caseHandling": "insensitive"
        })
        .to_string()
        .into_bytes()
    }

    fn describe(status: &str, files: usize) -> Vec<u8> {
        let mut value = json!({
            "change": "42",
            "status": status,
            "user": "ExampleAuthor",
            "client": "ExampleClientA",
            "desc": "Ready to submit"
        });
        if files > 0 {
            let object = value.as_object_mut().expect("describe object");
            object.insert("depotFile0".into(), json!("//SampleDepot/a.txt"));
            object.insert("action0".into(), json!("edit"));
            object.insert("type0".into(), json!("text"));
            object.insert("rev0".into(), json!("1"));
        }
        value.to_string().into_bytes()
    }

    fn form(status: &str) -> Vec<u8> {
        form_with_files(status, &["//SampleDepot/a.txt"])
    }

    fn describe_with_files(status: &str, files: &[(&str, &str, &str, &str)]) -> Vec<u8> {
        let mut value = json!({
            "change": "42",
            "status": status,
            "user": "ExampleAuthor",
            "client": "ExampleClientA",
            "desc": "Ready to submit"
        });
        let object = value.as_object_mut().expect("describe object");
        for (index, (depot, action, file_type, rev)) in files.iter().enumerate() {
            object.insert(format!("depotFile{index}"), json!(depot));
            object.insert(format!("action{index}"), json!(action));
            object.insert(format!("type{index}"), json!(file_type));
            object.insert(format!("rev{index}"), json!(rev));
        }
        value.to_string().into_bytes()
    }

    fn form_with_files(status: &str, files: &[&str]) -> Vec<u8> {
        let mut body = format!(
            "Change:\t42\nClient:\tExampleClientA\nUser:\tExampleAuthor\nStatus:\t{status}\nDescription:\n\tReady to submit\n\nFiles:\n"
        );
        for file in files {
            body.push('\t');
            body.push_str(file);
            body.push('\n');
        }
        body.into_bytes()
    }

    fn fstat(path: &Path, unresolved: u64) -> Vec<u8> {
        json!({
            "depotFile": "//SampleDepot/a.txt",
            "clientFile": path,
            "isMapped": true,
            "headRev": "1",
            "haveRev": "1",
            "action": "edit",
            "type": "text",
            "change": "42",
            "unresolved": unresolved.to_string()
        })
        .to_string()
        .into_bytes()
    }

    fn push_pending_snapshot(fake: &FakeP4Transport, workspace: &TestWorkspace, unresolved: u64) {
        fake.push_output(output(info(&workspace.root)));
        fake.push_output(output(describe("pending", 1)));
        fake.push_output(output(form("pending")));
        fake.push_output(output(fstat(&workspace.text, unresolved)));
    }

    fn service(fake: FakeP4Transport, root: &Path) -> P4WriteService<FakeP4Transport> {
        P4WriteService::new(P4Client::new(fake, "p4", root))
    }

    #[test]
    fn only_explicit_submit_controls_authorize() {
        let workspace = TestWorkspace::new();
        let fake = FakeP4Transport::default();
        push_pending_snapshot(&fake, &workspace, 0);
        let preview = service(fake, &workspace.root)
            .preview_submit(42)
            .expect("preview");
        assert_eq!(SubmitPreview::default_intent(), SubmitIntent::Cancel);
        for intent in [
            SubmitIntent::Cancel,
            SubmitIntent::Escape,
            SubmitIntent::Close,
            SubmitIntent::LoseFocus,
            SubmitIntent::Enter,
        ] {
            assert!(preview.clone().authorize(intent).is_none());
        }
        assert!(
            preview
                .clone()
                .authorize(SubmitIntent::SubmitButton)
                .is_some()
        );
        assert!(preview.authorize(SubmitIntent::CtrlEnter).is_some());
    }

    #[test]
    fn content_token_changes_for_text_and_binary_bytes() {
        let workspace = TestWorkspace::new();
        let identity = WorkspaceIdentity {
            server_id: "server".into(),
            user: "user".into(),
            client: "client".into(),
            root: workspace.root.clone(),
            stream: None,
            case_handling: CaseHandling::Insensitive,
        };
        let files = vec![
            ChangedFile {
                depot_path: "//depot/a.txt".into(),
                client_path: Some(workspace.text.clone()),
                action: FileAction::Edit,
                file_type: FileType::new("text"),
                base_revision: Some(1),
                moved_from: None,
                moved_to: None,
            },
            ChangedFile {
                depot_path: "//depot/b.bin".into(),
                client_path: Some(workspace.binary.clone()),
                action: FileAction::Edit,
                file_type: FileType::new("binary+l"),
                base_revision: Some(1),
                moved_from: None,
                moved_to: None,
            },
        ];
        let first = hash_submit_content(&identity, &files).expect("first token");
        fs::write(&workspace.text, b"two\n").expect("change text");
        let text_changed = hash_submit_content(&identity, &files).expect("text token");
        assert_ne!(first, text_changed);
        fs::write(&workspace.text, b"one\n").expect("restore text");
        fs::write(&workspace.binary, [0, 1, 0, 254]).expect("change binary");
        let binary_changed = hash_submit_content(&identity, &files).expect("binary token");
        assert_ne!(first, binary_changed);
    }

    #[test]
    fn unresolved_preflight_never_reaches_submit() {
        let workspace = TestWorkspace::new();
        let fake = FakeP4Transport::default();
        push_pending_snapshot(&fake, &workspace, 1);
        let error = service(fake.clone(), &workspace.root)
            .preview_submit(42)
            .expect_err("unresolved must block");
        assert!(matches!(
            error,
            SubmitError::Blocked(SubmitBlockReason::UnresolvedFiles)
        ));
        assert_eq!(fake.requests().len(), 4);
    }

    #[test]
    fn missing_local_file_blocks_before_submit() {
        let workspace = TestWorkspace::new();
        let fake = FakeP4Transport::default();
        fake.push_output(output(info(&workspace.root)));
        fake.push_output(output(describe("pending", 1)));
        fake.push_output(output(form("pending")));
        fake.push_output(output(fstat(&workspace.root.join("missing.txt"), 0)));

        let error = service(fake.clone(), &workspace.root)
            .preview_submit(42)
            .expect_err("missing content must block");
        assert!(matches!(
            error,
            SubmitError::Blocked(SubmitBlockReason::MissingLocalFile)
        ));
        assert_eq!(fake.requests().len(), 4);
    }

    #[test]
    fn resolved_out_of_date_file_is_not_misclassified_as_unresolved() {
        let resolved = json!({
            "depotFile": "//SampleDepot/a.txt",
            "clientFile": "C:/Example/a.txt",
            "isMapped": true,
            "headRev": "2",
            "haveRev": "1",
            "resolved": "1",
            "action": "edit",
            "type": "text",
            "change": "42"
        })
        .to_string();
        let records = crate::p4::parse_json_records(resolved.as_bytes()).expect("records");
        assert!(validate_fstat_preflight(&records, 42).is_ok());

        let unresolved_revision = resolved.replace(r#""resolved":"1""#, r#""resolved":"0""#);
        let records =
            crate::p4::parse_json_records(unresolved_revision.as_bytes()).expect("records");
        assert!(matches!(
            validate_fstat_preflight(&records, 42),
            Err(SubmitError::Blocked(SubmitBlockReason::OutOfDateFiles))
        ));
    }

    #[test]
    fn content_change_invalidates_authorization_without_write() {
        let workspace = TestWorkspace::new();
        let fake = FakeP4Transport::default();
        push_pending_snapshot(&fake, &workspace, 0);
        push_pending_snapshot(&fake, &workspace, 0);
        let service = service(fake.clone(), &workspace.root);
        let authorization = service
            .preview_submit(42)
            .expect("preview")
            .authorize(SubmitIntent::SubmitButton)
            .expect("authorization");
        fs::write(&workspace.text, b"changed after preview\n").expect("stale content");

        assert!(matches!(
            service.submit_change(authorization),
            Err(SubmitError::Stale)
        ));
        assert_eq!(fake.requests().len(), 8);
        assert!(
            fake.requests()
                .iter()
                .all(|request| !request.args.iter().any(|argument| argument == "submit"))
        );
    }

    #[test]
    fn successful_submit_runs_once_and_refreshes_submitted_state() {
        let workspace = TestWorkspace::new();
        let fake = FakeP4Transport::default();
        push_pending_snapshot(&fake, &workspace, 0);
        push_pending_snapshot(&fake, &workspace, 0);
        fake.push_output(output(br#"{"code":"info","data":"submitted"}"#.as_slice()));
        fake.push_output(output(info(&workspace.root)));
        fake.push_output(output(describe("submitted", 1)));
        let service = service(fake.clone(), &workspace.root);
        let authorization = service
            .preview_submit(42)
            .expect("preview")
            .authorize(SubmitIntent::CtrlEnter)
            .expect("authorization");

        let result = service.submit_change(authorization).expect("submit");
        assert_eq!(result.submitted_change, 42);
        assert_eq!(result.file_count, 1);
        let submit_requests = fake
            .requests()
            .into_iter()
            .filter(|request| request.args.iter().any(|argument| argument == "submit"))
            .collect::<Vec<_>>();
        assert_eq!(submit_requests.len(), 1);
        assert_eq!(
            submit_requests[0].args,
            ["-ztag", "-Mj", "submit", "-c", "42"].map(OsString::from)
        );
        assert_eq!(submit_requests[0].timeout, SUBMIT_TIMEOUT);
    }

    #[test]
    fn server_rejection_is_not_retried_or_reported_as_success() {
        let workspace = TestWorkspace::new();
        let fake = FakeP4Transport::default();
        push_pending_snapshot(&fake, &workspace, 0);
        push_pending_snapshot(&fake, &workspace, 0);
        fake.push_output(RawP4Output {
            stdout: Vec::new(),
            stderr: b"Out of date files must be resolved or reverted.".to_vec(),
            exit_code: 1,
            elapsed: Duration::from_millis(1),
        });
        let service = service(fake.clone(), &workspace.root);
        let authorization = service
            .preview_submit(42)
            .expect("preview")
            .authorize(SubmitIntent::SubmitButton)
            .expect("authorization");

        assert!(matches!(
            service.submit_change(authorization),
            Err(SubmitError::Query { stage: "write", .. })
        ));
        assert_eq!(
            fake.requests()
                .iter()
                .filter(|request| request.args.iter().any(|argument| argument == "submit"))
                .count(),
            1
        );
        assert_eq!(fake.remaining_steps(), 0);
    }

    #[test]
    fn coordinator_rejects_a_second_submit_while_one_is_running() {
        let workspace = TestWorkspace::new();
        let service = service(FakeP4Transport::default(), &workspace.root);
        let first = service.try_begin_submit().expect("first flight");
        assert!(service.try_begin_submit().is_none());
        drop(first);
        assert!(service.try_begin_submit().is_some());
    }

    #[test]
    fn add_have_rev_none_is_not_an_invalid_snapshot() {
        let add = json!({
            "depotFile": "//SampleDepot/new.txt",
            "clientFile": "C:/Example/new.txt",
            "isMapped": true,
            "haveRev": "none",
            "action": "add",
            "type": "text",
            "change": "42"
        })
        .to_string();
        let records = crate::p4::parse_json_records(add.as_bytes()).expect("records");
        assert!(validate_fstat_preflight(&records, 42).is_ok());

        let malformed = add.replace(r#""haveRev":"none""#, r#""haveRev":"head""#);
        let records = crate::p4::parse_json_records(malformed.as_bytes()).expect("records");
        assert!(matches!(
            validate_fstat_preflight(&records, 42),
            Err(SubmitError::InvalidSnapshot)
        ));
    }

    #[test]
    fn add_file_preview_accepts_have_rev_none() {
        let workspace = TestWorkspace::new();
        let fake = FakeP4Transport::default();
        fake.push_output(output(info(&workspace.root)));
        fake.push_output(output(describe_with_files(
            "pending",
            &[("//SampleDepot/new.txt", "add", "text", "1")],
        )));
        fake.push_output(output(form_with_files(
            "pending",
            &["//SampleDepot/new.txt"],
        )));
        fake.push_output(output(
            json!({
                "depotFile": "//SampleDepot/new.txt",
                "clientFile": workspace.text,
                "isMapped": true,
                "haveRev": "none",
                "action": "add",
                "type": "text",
                "change": "42"
            })
            .to_string()
            .into_bytes(),
        ));

        let preview = service(fake, &workspace.root)
            .preview_submit(42)
            .expect("add preview");
        assert_eq!(preview.actions.adds, 1);
        assert_eq!(preview.file_count, 1);
    }

    #[test]
    fn move_describe_without_moved_file_reconciles_fstat_endpoints() {
        let workspace = TestWorkspace::new();
        let fake = FakeP4Transport::default();
        fake.push_output(output(info(&workspace.root)));
        fake.push_output(output(describe_with_files(
            "pending",
            &[
                ("//SampleDepot/a.txt", "move/add", "text", "1"),
                ("//SampleDepot/old.txt", "move/delete", "text", "1"),
            ],
        )));
        fake.push_output(output(form_with_files(
            "pending",
            &["//SampleDepot/a.txt", "//SampleDepot/old.txt"],
        )));
        let fstat = format!(
            "{}\n{}",
            json!({
                "depotFile": "//SampleDepot/a.txt",
                "clientFile": workspace.text,
                "isMapped": true,
                "haveRev": "none",
                "action": "move/add",
                "type": "text",
                "change": "42",
                "movedFile": "//SampleDepot/old.txt"
            }),
            json!({
                "depotFile": "//SampleDepot/old.txt",
                "isMapped": true,
                "headRev": "1",
                "haveRev": "1",
                "action": "move/delete",
                "type": "text",
                "change": "42",
                "movedFile": "//SampleDepot/a.txt"
            })
        );
        fake.push_output(output(fstat.into_bytes()));

        let preview = service(fake, &workspace.root)
            .preview_submit(42)
            .expect("move preview");
        assert_eq!(preview.actions.move_adds, 1);
        assert_eq!(preview.actions.move_deletes, 1);
        assert_eq!(preview.file_count, 2);
    }

    #[test]
    fn timed_out_submit_write_tells_the_caller_to_refresh() {
        let workspace = TestWorkspace::new();
        let fake = FakeP4Transport::default();
        push_pending_snapshot(&fake, &workspace, 0);
        push_pending_snapshot(&fake, &workspace, 0);
        fake.push_error(TransportError::TimedOut);
        let service = service(fake.clone(), &workspace.root);
        let authorization = service
            .preview_submit(42)
            .expect("preview")
            .authorize(SubmitIntent::SubmitButton)
            .expect("authorization");

        assert!(matches!(
            service.submit_change(authorization),
            Err(SubmitError::TimedOut { stage: "write" })
        ));
        let submit_requests = fake
            .requests()
            .into_iter()
            .filter(|request| request.args.iter().any(|argument| argument == "submit"))
            .collect::<Vec<_>>();
        assert_eq!(submit_requests.len(), 1);
        assert_eq!(submit_requests[0].timeout, SUBMIT_TIMEOUT);
        assert_eq!(fake.remaining_steps(), 0);
    }
}
