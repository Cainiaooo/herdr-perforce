use std::{
    error::Error,
    ffi::OsString,
    fmt,
    sync::{Mutex, MutexGuard, PoisonError},
};

use crate::domain::{
    Changelist, ChangelistId, ChangelistStatus, SpecToken, WorkspaceIdentity, compute_spec_token,
};

use super::{
    P4Client, P4Error, P4Query, P4Transport, WorkspaceCwdError,
    form::ChangeForm,
    parser::{DomainMappingError, changelist_from_describe},
    workspace_owning_cwd,
};

pub const MAX_DESCRIPTION_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescriptionApplyIntent {
    Cancel,
    Escape,
    Close,
    LoseFocus,
    Enter,
    ApplyButton,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescriptionApplyBlockReason {
    NotPending,
    NotOwnedByCurrentUser,
    NotCurrentClient,
}

impl fmt::Display for DescriptionApplyBlockReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotPending => "the changelist is not pending",
            Self::NotOwnedByCurrentUser => "the changelist is owned by another user",
            Self::NotCurrentClient => "the changelist belongs to another client",
        })
    }
}

#[derive(Debug)]
pub enum DescriptionApplyError {
    Query {
        stage: &'static str,
        source: P4Error,
    },
    Mapping {
        stage: &'static str,
        source: DomainMappingError,
    },
    InvalidForm,
    InvalidDescription,
    NoChange,
    Ineligible(DescriptionApplyBlockReason),
    Stale,
    VerificationFailed,
}

impl fmt::Display for DescriptionApplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Query { stage, source } => {
                write!(formatter, "Description Apply {stage} failed: {source}")
            }
            Self::Mapping { stage, source } => {
                write!(
                    formatter,
                    "Description Apply could not map {stage}: {source}"
                )
            }
            Self::InvalidForm => {
                formatter.write_str("Description Apply refused an invalid changelist form")
            }
            Self::InvalidDescription => formatter.write_str(
                "Description Apply requires a non-empty description within the byte budget",
            ),
            Self::NoChange => {
                formatter.write_str("Description Apply requires a description change")
            }
            Self::Ineligible(reason) => {
                write!(formatter, "Description Apply is disabled because {reason}")
            }
            Self::Stale => formatter.write_str(
                "Description Apply confirmation is stale; refresh and review the changelist again",
            ),
            Self::VerificationFailed => formatter.write_str(
                "Perforce accepted Description Apply but the refreshed changelist did not match",
            ),
        }
    }
}

impl Error for DescriptionApplyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Query { source, .. } => Some(source),
            Self::Mapping { source, .. } => Some(source),
            Self::InvalidForm
            | Self::InvalidDescription
            | Self::NoChange
            | Self::Ineligible(_)
            | Self::Stale
            | Self::VerificationFailed => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptionApplyPreview {
    pub change: u64,
    pub current_description: String,
    pub proposed_description: String,
    pub file_count: usize,
    pub spec_token: SpecToken,
}

impl DescriptionApplyPreview {
    #[must_use]
    pub const fn default_intent() -> DescriptionApplyIntent {
        DescriptionApplyIntent::Cancel
    }

    #[must_use]
    pub fn authorize(self, intent: DescriptionApplyIntent) -> Option<AuthorizedDescriptionApply> {
        (intent == DescriptionApplyIntent::ApplyButton).then_some(AuthorizedDescriptionApply {
            change: self.change,
            proposed_description: self.proposed_description,
            expected_spec_token: self.spec_token,
        })
    }
}

#[derive(Debug)]
pub struct AuthorizedDescriptionApply {
    change: u64,
    proposed_description: String,
    expected_spec_token: SpecToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DescriptionApplyResult {
    pub change: u64,
    pub spec_token: SpecToken,
}

pub struct P4WriteService<T> {
    pub(crate) client: P4Client<T>,
    submit_in_flight: Mutex<bool>,
}

impl<T: P4Transport> P4WriteService<T> {
    #[must_use]
    pub fn new(client: P4Client<T>) -> Self {
        Self {
            client,
            submit_in_flight: Mutex::new(false),
        }
    }

    pub fn preview_description_apply(
        &self,
        change: u64,
        proposed_description: impl Into<String>,
    ) -> Result<DescriptionApplyPreview, DescriptionApplyError> {
        let proposed_description = proposed_description.into();
        validate_description(&proposed_description)?;
        let snapshot = self.load_snapshot(change)?;
        validate_eligibility(&snapshot.workspace, &snapshot.changelist)?;
        if canonical_description(&snapshot.changelist.description)
            == canonical_description(&proposed_description)
        {
            return Err(DescriptionApplyError::NoChange);
        }

        Ok(DescriptionApplyPreview {
            change,
            current_description: snapshot.changelist.description,
            proposed_description,
            file_count: snapshot.changelist.files.len(),
            spec_token: snapshot.spec_token,
        })
    }

    pub fn apply_description(
        &self,
        authorization: AuthorizedDescriptionApply,
    ) -> Result<DescriptionApplyResult, DescriptionApplyError> {
        let snapshot = self.load_snapshot(authorization.change)?;
        validate_eligibility(&snapshot.workspace, &snapshot.changelist)?;
        if snapshot.spec_token != authorization.expected_spec_token {
            return Err(DescriptionApplyError::Stale);
        }

        let updated_form = snapshot
            .form
            .replace_description(&authorization.proposed_description)
            .map_err(|_| DescriptionApplyError::InvalidForm)?;
        let mut expected_changelist = snapshot.changelist.clone();
        expected_changelist.description =
            canonical_description(&authorization.proposed_description);
        let expected_spec_token = compute_spec_token(&snapshot.workspace, &expected_changelist);
        self.client
            .run_raw(["change", "-i"].map(OsString::from).to_vec(), updated_form)
            .map_err(|source| DescriptionApplyError::Query {
                stage: "write",
                source,
            })?;

        let refreshed = self.load_snapshot(authorization.change)?;
        if canonical_description(&refreshed.changelist.description)
            != expected_changelist.description
            || refreshed.spec_token != expected_spec_token
        {
            return Err(DescriptionApplyError::VerificationFailed);
        }

        Ok(DescriptionApplyResult {
            change: authorization.change,
            spec_token: refreshed.spec_token,
        })
    }

    pub(crate) fn load_snapshot(
        &self,
        change: u64,
    ) -> Result<DescriptionSnapshot, DescriptionApplyError> {
        let info =
            self.client
                .run(&P4Query::Info)
                .map_err(|source| DescriptionApplyError::Query {
                    stage: "workspace refresh",
                    source,
                })?;
        let workspace = match workspace_owning_cwd(self.client.cwd(), &info.records) {
            Ok(workspace) => workspace,
            Err(WorkspaceCwdError::Mapping(source)) => {
                return Err(DescriptionApplyError::Mapping {
                    stage: "workspace identity",
                    source,
                });
            }
            Err(WorkspaceCwdError::Query(source)) => {
                return Err(DescriptionApplyError::Query {
                    stage: "workspace identity",
                    source,
                });
            }
        };

        let describe = self
            .client
            .run(&P4Query::DescribeSummary { change })
            .map_err(|source| DescriptionApplyError::Query {
                stage: "changelist refresh",
                source,
            })?;
        let mut changelist = changelist_from_describe(&describe.records).map_err(|source| {
            DescriptionApplyError::Mapping {
                stage: "changelist",
                source,
            }
        })?;

        let form_output = self
            .client
            .run_raw(
                ["change", "-o", &change.to_string()]
                    .map(OsString::from)
                    .to_vec(),
                Vec::new(),
            )
            .map_err(|source| DescriptionApplyError::Query {
                stage: "form refresh",
                source,
            })?;
        let form = ChangeForm::parse(&form_output.stdout)
            .map_err(|_| DescriptionApplyError::InvalidForm)?;
        validate_form_identity(change, &changelist, &form)?;
        changelist.description = form
            .field("Description")
            .expect("validated form must contain Description");
        changelist.preserved_spec_fields = form.preserved_fields();
        let spec_token = compute_spec_token(&workspace, &changelist);

        Ok(DescriptionSnapshot {
            workspace,
            changelist,
            form,
            spec_token,
        })
    }

    pub(crate) fn try_begin_submit(&self) -> Option<SubmitFlightGuard<'_>> {
        let mut in_flight = recover_mutex(&self.submit_in_flight);
        if *in_flight {
            return None;
        }
        *in_flight = true;
        Some(SubmitFlightGuard {
            in_flight: &self.submit_in_flight,
        })
    }
}

pub(crate) struct DescriptionSnapshot {
    pub(crate) workspace: WorkspaceIdentity,
    pub(crate) changelist: Changelist,
    pub(crate) form: ChangeForm,
    pub(crate) spec_token: SpecToken,
}

pub(crate) struct SubmitFlightGuard<'a> {
    in_flight: &'a Mutex<bool>,
}

impl Drop for SubmitFlightGuard<'_> {
    fn drop(&mut self) {
        *recover_mutex(self.in_flight) = false;
    }
}

fn recover_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn validate_description(description: &str) -> Result<(), DescriptionApplyError> {
    if description.trim().is_empty()
        || description.contains('\0')
        || description.len() > MAX_DESCRIPTION_BYTES
    {
        return Err(DescriptionApplyError::InvalidDescription);
    }
    Ok(())
}

fn validate_eligibility(
    workspace: &WorkspaceIdentity,
    changelist: &Changelist,
) -> Result<(), DescriptionApplyError> {
    if changelist.status != ChangelistStatus::Pending {
        return Err(DescriptionApplyError::Ineligible(
            DescriptionApplyBlockReason::NotPending,
        ));
    }
    if changelist.owner != workspace.user {
        return Err(DescriptionApplyError::Ineligible(
            DescriptionApplyBlockReason::NotOwnedByCurrentUser,
        ));
    }
    if changelist.client != workspace.client {
        return Err(DescriptionApplyError::Ineligible(
            DescriptionApplyBlockReason::NotCurrentClient,
        ));
    }
    Ok(())
}

fn validate_form_identity(
    change: u64,
    changelist: &Changelist,
    form: &ChangeForm,
) -> Result<(), DescriptionApplyError> {
    let form_change = form
        .field("Change")
        .and_then(|value| value.parse::<u64>().ok());
    let form_description = form.field("Description");
    let valid = changelist.id == ChangelistId::Numbered(change)
        && form_change == Some(change)
        && form.field("User").as_deref() == Some(changelist.owner.as_str())
        && form.field("Client").as_deref() == Some(changelist.client.as_str())
        && form_description.is_some_and(|description| {
            canonical_description(&description) == canonical_description(&changelist.description)
        })
        && form
            .field("Status")
            .is_some_and(|status| status.eq_ignore_ascii_case(changelist.status.canonical_name()));
    if !valid {
        return Err(DescriptionApplyError::InvalidForm);
    }
    Ok(())
}

pub(crate) fn canonical_description(description: &str) -> String {
    description
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim_end_matches('\n')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use super::*;
    use crate::p4::{RawP4Output, fake::FakeP4Transport};

    const INFO: &[u8] = br#"{"clientName":"ExampleClientA","clientRoot":"C:/Example","serverAddress":"127.0.0.1:1666","userName":"ExampleAuthor","caseHandling":"insensitive"}"#;
    const DESCRIBE_OLD: &[u8] = br#"{"change":"42","status":"pending","user":"ExampleAuthor","client":"ExampleClientA","desc":"Old description","depotFile0":"//SampleDepot/a.txt","action0":"edit","type0":"text","rev0":"1"}"#;
    const DESCRIBE_NEW: &[u8] = br#"{"change":"42","status":"pending","user":"ExampleAuthor","client":"ExampleClientA","desc":"New description","depotFile0":"//SampleDepot/a.txt","action0":"edit","type0":"text","rev0":"1"}"#;
    const FORM_OLD: &[u8] = b"Change:\t42\nDate:\t2026/08/25 12:00:00\nClient:\tExampleClientA\nUser:\tExampleAuthor\nStatus:\tpending\nDescription:\n\tOld description\n\nJobs:\n\tJOB-1\n\nType:\tpublic\n\nFiles:\n\t//SampleDepot/a.txt\n";
    const FORM_NEW: &[u8] = b"Change:\t42\nDate:\t2026/08/25 12:01:00\nClient:\tExampleClientA\nUser:\tExampleAuthor\nStatus:\tpending\nDescription:\n\tNew description\n\nJobs:\n\tJOB-1\n\nType:\tpublic\n\nFiles:\n\t//SampleDepot/a.txt\n";

    fn output(stdout: &[u8]) -> RawP4Output {
        RawP4Output {
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::from_millis(1),
        }
    }

    fn push_snapshot(fake: &FakeP4Transport, describe: &[u8], form: &[u8]) {
        fake.push_output(output(INFO));
        fake.push_output(output(describe));
        fake.push_output(output(form));
    }

    fn service(fake: FakeP4Transport) -> P4WriteService<FakeP4Transport> {
        P4WriteService::new(P4Client::new_with_directory_environment(
            fake,
            "p4",
            PathBuf::from("C:/Example"),
            Default::default(),
        ))
    }

    #[test]
    fn only_explicit_apply_button_creates_write_authorization() {
        assert_eq!(
            DescriptionApplyPreview::default_intent(),
            DescriptionApplyIntent::Cancel
        );
        for intent in [
            DescriptionApplyIntent::Cancel,
            DescriptionApplyIntent::Escape,
            DescriptionApplyIntent::Close,
            DescriptionApplyIntent::LoseFocus,
            DescriptionApplyIntent::Enter,
        ] {
            let preview = DescriptionApplyPreview {
                change: 42,
                current_description: "Old".into(),
                proposed_description: "New".into(),
                file_count: 1,
                spec_token: SpecToken::from_bytes_for_test([1; 32]),
            };
            assert!(preview.authorize(intent).is_none());
        }
    }

    #[test]
    fn apply_rechecks_token_and_preserves_non_description_form_fields() {
        let fake = FakeP4Transport::default();
        push_snapshot(&fake, DESCRIBE_OLD, FORM_OLD);
        push_snapshot(&fake, DESCRIBE_OLD, FORM_OLD);
        fake.push_output(output(b"Change 42 updated.\n"));
        push_snapshot(&fake, DESCRIBE_NEW, FORM_NEW);
        let service = service(fake.clone());

        let preview = service
            .preview_description_apply(42, "New description")
            .expect("preview");
        let authorization = preview
            .authorize(DescriptionApplyIntent::ApplyButton)
            .expect("explicit confirmation");
        let result = service
            .apply_description(authorization)
            .expect("Description Apply");

        assert_eq!(result.change, 42);
        let requests = fake.requests();
        assert_eq!(requests.len(), 10);
        assert_eq!(requests[6].args, ["change", "-i"].map(OsString::from));
        let written = String::from_utf8(requests[6].stdin.clone()).expect("form stdin");
        assert!(written.contains("Description:\n\tNew description\n"));
        assert!(written.contains("Jobs:\n\tJOB-1\n"));
        assert!(written.contains("Type:\tpublic\n"));
        assert!(written.contains("Files:\n\t//SampleDepot/a.txt\n"));
        assert_eq!(
            requests
                .iter()
                .filter(|request| !request.stdin.is_empty())
                .count(),
            1
        );
    }

    #[test]
    fn stale_preview_refuses_write_without_consuming_a_write_step() {
        let fake = FakeP4Transport::default();
        push_snapshot(&fake, DESCRIBE_OLD, FORM_OLD);
        push_snapshot(&fake, DESCRIBE_NEW, FORM_NEW);
        let service = service(fake.clone());
        let preview = service
            .preview_description_apply(42, "My replacement")
            .expect("preview");
        let authorization = preview
            .authorize(DescriptionApplyIntent::ApplyButton)
            .expect("confirmation");

        let error = service
            .apply_description(authorization)
            .expect_err("stale token must fail");

        assert!(matches!(error, DescriptionApplyError::Stale));
        assert_eq!(fake.requests().len(), 6);
        assert!(
            fake.requests()
                .iter()
                .all(|request| request.stdin.is_empty())
        );
    }

    #[test]
    fn capability_gate_rejects_other_owner_client_and_status() {
        let workspace = WorkspaceIdentity {
            server_id: "server".into(),
            user: "ExampleAuthor".into(),
            client: "ExampleClientA".into(),
            root: PathBuf::from("C:/Example"),
            stream: None,
            case_handling: crate::domain::CaseHandling::Insensitive,
        };
        let mut changelist = Changelist {
            id: ChangelistId::Numbered(42),
            status: ChangelistStatus::Pending,
            owner: workspace.user.clone(),
            client: workspace.client.clone(),
            description: "Work".into(),
            files: Vec::new(),
            preserved_spec_fields: Default::default(),
            spec_token: None,
            content_token: None,
        };
        assert!(validate_eligibility(&workspace, &changelist).is_ok());

        changelist.owner = "ExampleOther".into();
        assert!(matches!(
            validate_eligibility(&workspace, &changelist),
            Err(DescriptionApplyError::Ineligible(
                DescriptionApplyBlockReason::NotOwnedByCurrentUser
            ))
        ));
        changelist.owner = workspace.user.clone();
        changelist.client = "ExampleClientB".into();
        assert!(matches!(
            validate_eligibility(&workspace, &changelist),
            Err(DescriptionApplyError::Ineligible(
                DescriptionApplyBlockReason::NotCurrentClient
            ))
        ));
        changelist.client = workspace.client.clone();
        changelist.status = ChangelistStatus::Submitted;
        assert!(matches!(
            validate_eligibility(&workspace, &changelist),
            Err(DescriptionApplyError::Ineligible(
                DescriptionApplyBlockReason::NotPending
            ))
        ));
    }

    #[test]
    fn descriptions_have_a_finite_non_empty_budget() {
        let fake = FakeP4Transport::default();
        let service = service(fake.clone());
        assert!(matches!(
            service.preview_description_apply(42, "  "),
            Err(DescriptionApplyError::InvalidDescription)
        ));
        assert!(matches!(
            service.preview_description_apply(42, "x".repeat(MAX_DESCRIPTION_BYTES + 1)),
            Err(DescriptionApplyError::InvalidDescription)
        ));
        assert!(fake.requests().is_empty());
    }

    #[test]
    fn unchanged_description_is_not_authorizable() {
        let fake = FakeP4Transport::default();
        push_snapshot(&fake, DESCRIBE_OLD, FORM_OLD);
        let service = service(fake.clone());
        assert!(matches!(
            service.preview_description_apply(42, "Old description\r\n"),
            Err(DescriptionApplyError::NoChange)
        ));
        assert_eq!(fake.requests().len(), 3);
        assert!(
            fake.requests()
                .iter()
                .all(|request| request.stdin.is_empty())
        );
    }
}
