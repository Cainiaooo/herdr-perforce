use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
};

use crate::domain::{ChangedFile, Changelist, ChangelistId, ChangelistStatus, WorkspaceIdentity};

use super::{
    DomainMappingError, P4Client, P4Error, P4Query, P4Transport, P4WriteService, WorkspaceCwdError,
    changed_files_from_opened, changelist_from_describe, default_pending_changelist,
    escape_p4_file_arg, form::ChangeForm, workspace_owning_cwd,
};

pub const MAX_MANAGED_FILES: usize = 512;
const CLIENT_PATH_QUERY_BATCH: usize = 32;

#[derive(Debug)]
pub enum ChangelistManagementError {
    Query {
        stage: &'static str,
        source: P4Error,
    },
    Mapping {
        stage: &'static str,
        source: DomainMappingError,
    },
    InvalidDescription,
    InvalidForm,
    InvalidCreatedChange,
    DefaultCannotBeDeleted,
    NotPending,
    NotOwnedByCurrentUser,
    NotCurrentClient,
    NotEmpty,
    EmptySelection,
    TooManyFiles,
    SameChangelist,
    SelectionChanged,
    VerificationFailed,
}

impl fmt::Display for ChangelistManagementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Query { stage, source } => write!(formatter, "Changelist {stage} failed: {source}"),
            Self::Mapping { stage, source } => {
                write!(formatter, "Changelist {stage} returned invalid data: {source}")
            }
            Self::InvalidDescription => formatter.write_str(
                "A new changelist needs a non-empty description within the input limit",
            ),
            Self::InvalidForm => {
                formatter.write_str("Perforce returned an invalid new changelist form")
            }
            Self::InvalidCreatedChange => formatter.write_str(
                "Perforce accepted changelist creation but did not return a valid change number",
            ),
            Self::DefaultCannotBeDeleted => {
                formatter.write_str("The default changelist cannot be deleted")
            }
            Self::NotPending => formatter.write_str("Only pending changelists can be changed"),
            Self::NotOwnedByCurrentUser => {
                formatter.write_str("The changelist belongs to another user")
            }
            Self::NotCurrentClient => {
                formatter.write_str("The changelist belongs to another client")
            }
            Self::NotEmpty => formatter.write_str(
                "Only an empty changelist can be deleted; move or revert its files first",
            ),
            Self::EmptySelection => formatter.write_str("Select at least one file first"),
            Self::TooManyFiles => formatter.write_str("The selected file count exceeds the safety limit"),
            Self::SameChangelist => {
                formatter.write_str("Source and destination changelists must be different")
            }
            Self::SelectionChanged => formatter.write_str(
                "The selected files no longer belong to the source changelist; refresh and select again",
            ),
            Self::VerificationFailed => formatter.write_str(
                "Perforce accepted the write but the refreshed changelist state did not verify",
            ),
        }
    }
}

impl Error for ChangelistManagementError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Query { source, .. } => Some(source),
            Self::Mapping { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateChangelistResult {
    pub change: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeleteChangelistResult {
    pub change: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveFilesResult {
    pub source: ChangelistId,
    pub target: ChangelistId,
    pub file_count: usize,
}

pub fn load_pending_changelist<T: P4Transport>(
    client: &P4Client<T>,
    workspace: &WorkspaceIdentity,
    change: ChangelistId,
) -> Result<Changelist, ChangelistManagementError> {
    let opened = client
        .run_records(&P4Query::Opened { change })
        .map_err(|source| ChangelistManagementError::Query {
            stage: "file refresh",
            source,
        })?;
    let files = changed_files_from_opened(&opened.records).map_err(|source| {
        ChangelistManagementError::Mapping {
            stage: "file refresh",
            source,
        }
    })?;
    let mut changelist = match change {
        ChangelistId::Default => default_pending_changelist(&workspace.user, &workspace.client),
        ChangelistId::Numbered(number) => {
            let describe = client
                .run(&P4Query::DescribeSummary { change: number })
                .map_err(|source| ChangelistManagementError::Query {
                    stage: "detail refresh",
                    source,
                })?;
            changelist_from_describe(&describe.records).map_err(|source| {
                ChangelistManagementError::Mapping {
                    stage: "detail refresh",
                    source,
                }
            })?
        }
    };
    changelist.files = files;
    validate_owned_pending(workspace, &changelist)?;
    Ok(changelist)
}

/// Loads pending files for Review and resolves Perforce `//client/...` names
/// to local filesystem paths before the Content pane tries to read them.
pub fn load_pending_changelist_files_for_review<T: P4Transport>(
    client: &P4Client<T>,
    workspace: &WorkspaceIdentity,
    change: ChangelistId,
) -> Result<Vec<ChangedFile>, ChangelistManagementError> {
    let mut files = load_pending_changelist(client, workspace, change)?.files;
    let mut local_by_depot = BTreeMap::new();
    let depot_paths = files
        .iter()
        .map(|file| PathBuf::from(&file.depot_path))
        .collect::<Vec<_>>();

    for paths in depot_paths.chunks(CLIENT_PATH_QUERY_BATCH) {
        let mapped = client
            .run_records(&P4Query::WhereMany {
                paths: paths.to_vec(),
            })
            .map_err(|source| ChangelistManagementError::Query {
                stage: "client path mapping",
                source,
            })?;
        for record in mapped.records {
            let Some(depot_path) = record.string("depotFile") else {
                continue;
            };
            let Some(local_path) = record.string("path").map(PathBuf::from).or_else(|| {
                record
                    .string("clientFile")
                    .map(PathBuf::from)
                    .filter(|path| is_local_filesystem_path(path))
            }) else {
                continue;
            };
            local_by_depot.insert(
                workspace.case_handling.canonical_path_key(&depot_path),
                local_path,
            );
        }
    }

    for file in &mut files {
        let key = workspace.case_handling.canonical_path_key(&file.depot_path);
        if let Some(local_path) = local_by_depot.get(&key) {
            file.client_path = Some(local_path.clone());
        } else if file
            .client_path
            .as_deref()
            .is_some_and(is_local_filesystem_path)
        {
            // Older servers may already return an OS path in `clientFile`.
        } else {
            file.client_path = None;
        }
    }
    Ok(files)
}

fn is_local_filesystem_path(path: &Path) -> bool {
    !path.as_os_str().to_string_lossy().starts_with("//")
}

impl<T: P4Transport> P4WriteService<T> {
    pub fn create_changelist(
        &self,
        description: impl Into<String>,
    ) -> Result<CreateChangelistResult, ChangelistManagementError> {
        let description = description.into();
        if description.trim().is_empty()
            || description.contains('\0')
            || description.len() > super::MAX_DESCRIPTION_BYTES
        {
            return Err(ChangelistManagementError::InvalidDescription);
        }
        let workspace = load_workspace(&self.client)?;
        let output = self
            .client
            .run_raw(["change", "-o"].map(OsString::from).to_vec(), Vec::new())
            .map_err(|source| ChangelistManagementError::Query {
                stage: "template refresh",
                source,
            })?;
        let form = ChangeForm::parse(&output.stdout)
            .map_err(|_| ChangelistManagementError::InvalidForm)?;
        if form.field("Change").as_deref() != Some("new")
            || form.field("User").as_deref() != Some(workspace.user.as_str())
            || form.field("Client").as_deref() != Some(workspace.client.as_str())
        {
            return Err(ChangelistManagementError::InvalidForm);
        }
        let input = form
            .prepare_new_change(&description)
            .map_err(|_| ChangelistManagementError::InvalidForm)?;
        let response = self
            .client
            .run_structured(
                ["-ztag", "-Mj", "change", "-i"]
                    .map(OsString::from)
                    .to_vec(),
                input,
            )
            .map_err(|source| ChangelistManagementError::Query {
                stage: "creation",
                source,
            })?;
        let change = response
            .records
            .iter()
            .find_map(|record| record.field("change"))
            .and_then(serde_json::Value::as_str)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|change| *change > 0)
            .ok_or(ChangelistManagementError::InvalidCreatedChange)?;
        let created =
            load_pending_changelist(&self.client, &workspace, ChangelistId::Numbered(change))?;
        if created.description.trim_end_matches(['\r', '\n'])
            != description.trim_end_matches(['\r', '\n'])
            || !created.files.is_empty()
        {
            return Err(ChangelistManagementError::VerificationFailed);
        }
        Ok(CreateChangelistResult { change })
    }

    pub fn delete_changelist(
        &self,
        change: ChangelistId,
    ) -> Result<DeleteChangelistResult, ChangelistManagementError> {
        let ChangelistId::Numbered(change_number) = change else {
            return Err(ChangelistManagementError::DefaultCannotBeDeleted);
        };
        let workspace = load_workspace(&self.client)?;
        let snapshot = load_pending_changelist(&self.client, &workspace, change)?;
        if !snapshot.files.is_empty() {
            return Err(ChangelistManagementError::NotEmpty);
        }
        self.client
            .run_structured(
                ["-ztag", "-Mj", "change", "-d", &change_number.to_string()]
                    .map(OsString::from)
                    .to_vec(),
                Vec::new(),
            )
            .map_err(|source| ChangelistManagementError::Query {
                stage: "deletion",
                source,
            })?;
        Ok(DeleteChangelistResult {
            change: change_number,
        })
    }

    pub fn move_files(
        &self,
        source: ChangelistId,
        target: ChangelistId,
        depot_paths: Vec<String>,
    ) -> Result<MoveFilesResult, ChangelistManagementError> {
        if source == target {
            return Err(ChangelistManagementError::SameChangelist);
        }
        if depot_paths.is_empty() {
            return Err(ChangelistManagementError::EmptySelection);
        }
        if depot_paths.len() > MAX_MANAGED_FILES {
            return Err(ChangelistManagementError::TooManyFiles);
        }
        let workspace = load_workspace(&self.client)?;
        let source_snapshot = load_pending_changelist(&self.client, &workspace, source)?;
        let _target_snapshot = load_pending_changelist(&self.client, &workspace, target)?;
        let selected = canonical_depot_paths(&workspace, depot_paths.iter().map(String::as_str));
        if selected.len() != depot_paths.len() {
            return Err(ChangelistManagementError::SelectionChanged);
        }
        let source_paths = canonical_file_paths(&workspace, &source_snapshot.files);
        if !selected.is_subset(&source_paths) {
            return Err(ChangelistManagementError::SelectionChanged);
        }

        let mut args = vec![
            OsString::from("-ztag"),
            OsString::from("-Mj"),
            OsString::from("reopen"),
            OsString::from("-c"),
            OsString::from(target.as_p4_arg()),
        ];
        args.extend(
            depot_paths
                .iter()
                .map(|path| escape_p4_file_arg(path.as_ref())),
        );
        self.client
            .run_structured(args, Vec::new())
            .map_err(|source| ChangelistManagementError::Query {
                stage: "file move",
                source,
            })?;

        let refreshed_source = load_pending_changelist(&self.client, &workspace, source)?;
        let refreshed_target = load_pending_changelist(&self.client, &workspace, target)?;
        let remaining = canonical_file_paths(&workspace, &refreshed_source.files);
        let moved = canonical_file_paths(&workspace, &refreshed_target.files);
        if !selected.is_disjoint(&remaining) || !selected.is_subset(&moved) {
            return Err(ChangelistManagementError::VerificationFailed);
        }
        Ok(MoveFilesResult {
            source,
            target,
            file_count: depot_paths.len(),
        })
    }
}

fn load_workspace<T: P4Transport>(
    client: &P4Client<T>,
) -> Result<WorkspaceIdentity, ChangelistManagementError> {
    let info = client
        .run(&P4Query::Info)
        .map_err(|source| ChangelistManagementError::Query {
            stage: "workspace refresh",
            source,
        })?;
    match workspace_owning_cwd(client.cwd(), &info.records) {
        Ok(workspace) => Ok(workspace),
        Err(WorkspaceCwdError::Mapping(source)) => Err(ChangelistManagementError::Mapping {
            stage: "workspace identity",
            source,
        }),
        Err(WorkspaceCwdError::Query(source)) => Err(ChangelistManagementError::Query {
            stage: "workspace identity",
            source,
        }),
    }
}

fn validate_owned_pending(
    workspace: &WorkspaceIdentity,
    changelist: &Changelist,
) -> Result<(), ChangelistManagementError> {
    if changelist.status != ChangelistStatus::Pending {
        return Err(ChangelistManagementError::NotPending);
    }
    if changelist.owner != workspace.user {
        return Err(ChangelistManagementError::NotOwnedByCurrentUser);
    }
    if changelist.client != workspace.client {
        return Err(ChangelistManagementError::NotCurrentClient);
    }
    Ok(())
}

fn canonical_depot_paths<'a>(
    workspace: &WorkspaceIdentity,
    paths: impl Iterator<Item = &'a str>,
) -> BTreeSet<String> {
    paths
        .map(|path| workspace.case_handling.canonical_path_key(path))
        .collect()
}

fn canonical_file_paths(workspace: &WorkspaceIdentity, files: &[ChangedFile]) -> BTreeSet<String> {
    canonical_depot_paths(workspace, files.iter().map(|file| file.depot_path.as_str()))
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use super::*;
    use crate::domain::CaseHandling;
    use crate::p4::{RawP4Output, fake::FakeP4Transport};

    const INFO: &[u8] = br#"{"clientName":"ExampleClient","clientRoot":"C:/ws","serverAddress":"p4:1666","userName":"me","caseHandling":"insensitive"}"#;
    const EMPTY_42: &[u8] = br#"{"change":"42","status":"pending","user":"me","client":"ExampleClient","desc":"Target"}"#;

    fn output(stdout: &[u8]) -> RawP4Output {
        RawP4Output {
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::from_millis(1),
        }
    }

    fn service(fake: FakeP4Transport) -> P4WriteService<FakeP4Transport> {
        P4WriteService::new(P4Client::new_with_directory_environment(
            fake,
            "p4",
            PathBuf::from("C:/ws"),
            Default::default(),
        ))
    }

    fn workspace() -> WorkspaceIdentity {
        WorkspaceIdentity {
            server_id: "p4:1666".into(),
            user: "me".into(),
            client: "ExampleClient".into(),
            root: PathBuf::from("C:/ws"),
            stream: None,
            case_handling: CaseHandling::Insensitive,
        }
    }

    #[test]
    fn review_files_resolve_client_syntax_to_local_paths() {
        let fake = FakeP4Transport::default();
        fake.push_output(output(br#"{"depotFile":"//D/a#b.txt","clientFile":"//ExampleClient/a#b.txt","action":"edit","change":"42","type":"text"}"#));
        fake.push_output(output(br#"{"change":"42","status":"pending","user":"me","client":"ExampleClient","desc":"Work"}"#));
        fake.push_output(output(br#"{"depotFile":"//d/A#B.txt","clientFile":"//ExampleClient/a#b.txt","path":"C:/ws/a#b.txt"}"#));
        let client = P4Client::new_with_directory_environment(
            fake.clone(),
            "p4",
            PathBuf::from("C:/ws"),
            Default::default(),
        );

        let files = load_pending_changelist_files_for_review(
            &client,
            &workspace(),
            ChangelistId::Numbered(42),
        )
        .expect("review files");

        assert_eq!(files[0].client_path, Some(PathBuf::from("C:/ws/a#b.txt")));
        assert_eq!(
            fake.requests()[2].args,
            ["-ztag", "-Mj", "where", "//D/a%23b.txt"].map(OsString::from)
        );
    }

    #[test]
    fn review_files_do_not_expose_unresolved_client_syntax_as_a_local_path() {
        let fake = FakeP4Transport::default();
        fake.push_output(output(br#"{"depotFile":"//d/a.txt","clientFile":"//ExampleClient/a.txt","action":"edit","change":"default","type":"text"}"#));
        fake.push_output(output(
            br#"{"code":"error","data":"//d/a.txt - file(s) not in client view."}"#,
        ));
        let client = P4Client::new_with_directory_environment(
            fake,
            "p4",
            PathBuf::from("C:/ws"),
            Default::default(),
        );

        let files =
            load_pending_changelist_files_for_review(&client, &workspace(), ChangelistId::Default)
                .expect("per-path where miss remains a usable review result");

        assert_eq!(files[0].client_path, None);
    }

    #[test]
    fn delete_refuses_a_non_empty_change_before_the_write() {
        let fake = FakeP4Transport::default();
        fake.push_output(output(INFO));
        fake.push_output(output(br#"{"depotFile":"//d/a.txt","clientFile":"C:/ws/a.txt","action":"edit","change":"42","type":"text"}"#));
        fake.push_output(output(br#"{"change":"42","status":"pending","user":"me","client":"ExampleClient","desc":"Work"}"#));
        let error = service(fake.clone())
            .delete_changelist(ChangelistId::Numbered(42))
            .expect_err("non-empty CL must be retained");
        assert!(matches!(error, ChangelistManagementError::NotEmpty));
        assert_eq!(fake.requests().len(), 3);
    }

    #[test]
    fn delete_empty_change_uses_an_argv_safe_numbered_command() {
        let fake = FakeP4Transport::default();
        fake.push_output(output(INFO));
        fake.push_output(output(b""));
        fake.push_output(output(EMPTY_42));
        fake.push_output(output(br#"{"change":"42","action":"deleted"}"#));
        let result = service(fake.clone())
            .delete_changelist(ChangelistId::Numbered(42))
            .expect("delete empty CL");
        assert_eq!(result.change, 42);
        assert_eq!(
            fake.requests()[3].args,
            ["-ztag", "-Mj", "change", "-d", "42"].map(OsString::from)
        );
    }

    #[test]
    fn create_uses_the_server_template_and_verifies_the_new_empty_change() {
        let fake = FakeP4Transport::default();
        fake.push_output(output(INFO));
        fake.push_output(output(b"Change:\tnew\nClient:\tExampleClient\nUser:\tme\nStatus:\tnew\nDescription:\n\t<enter description here>\n\nFiles:\n\t//d/default-a.txt\n\t//d/default-b.txt\n"));
        fake.push_output(output(br#"{"change":"77","action":"created"}"#));
        fake.push_output(output(b""));
        fake.push_output(output(br#"{"change":"77","status":"pending","user":"me","client":"ExampleClient","desc":"My work"}"#));
        let result = service(fake.clone())
            .create_changelist("My work")
            .expect("create CL");
        assert_eq!(result.change, 77);
        let request = &fake.requests()[2];
        assert_eq!(
            request.args,
            ["-ztag", "-Mj", "change", "-i"].map(OsString::from)
        );
        let submitted = String::from_utf8_lossy(&request.stdin);
        assert!(submitted.contains("\tMy work\n"));
        assert!(submitted.contains("Files:\n"));
        assert!(!submitted.contains("//d/default-a.txt"));
        assert!(!submitted.contains("//d/default-b.txt"));
    }

    #[test]
    fn move_revalidates_source_then_checks_both_sides() {
        let fake = FakeP4Transport::default();
        fake.push_output(output(INFO));
        // Source snapshot.
        fake.push_output(output(br#"{"depotFile":"//d/a#b.txt","clientFile":"C:/ws/a#b.txt","action":"edit","change":"42","type":"text"}"#));
        fake.push_output(output(br#"{"change":"42","status":"pending","user":"me","client":"ExampleClient","desc":"Source"}"#));
        // Target snapshot.
        fake.push_output(output(b""));
        fake.push_output(output(br#"{"change":"77","status":"pending","user":"me","client":"ExampleClient","desc":"Target"}"#));
        fake.push_output(output(
            br#"{"depotFile":"//d/a#b.txt","action":"edit","change":"77"}"#,
        ));
        // Refreshed source and target.
        fake.push_output(output(b""));
        fake.push_output(output(br#"{"change":"42","status":"pending","user":"me","client":"ExampleClient","desc":"Source"}"#));
        fake.push_output(output(br#"{"depotFile":"//d/a#b.txt","clientFile":"C:/ws/a#b.txt","action":"edit","change":"77","type":"text"}"#));
        fake.push_output(output(br#"{"change":"77","status":"pending","user":"me","client":"ExampleClient","desc":"Target"}"#));

        let result = service(fake.clone())
            .move_files(
                ChangelistId::Numbered(42),
                ChangelistId::Numbered(77),
                vec!["//d/a#b.txt".into()],
            )
            .expect("move selected file");
        assert_eq!(result.file_count, 1);
        assert_eq!(fake.requests()[5].args[4], "77");
        assert_eq!(fake.requests()[5].args[5], "//d/a%23b.txt");
    }
}
