//! Explicitly enabled, read-only compatibility checks against a real P4 setup.

use std::{error::Error, fmt, path::Path};

use crate::domain::ChangelistId;

use super::{
    P4Query,
    error::{P4Error, P4ErrorKind},
    parser::{
        DomainMappingError, RecordCode, changed_files_from_opened, changelist_from_describe,
        changelists_from_changes, workspace_from_info,
    },
    transport::{P4Client, P4Transport},
};

pub const MAX_LEVEL_B_CHANGES: u16 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelBIdentitySummary {
    pub server: String,
    pub user: String,
    pub client: String,
    pub charset: &'static str,
    pub case_handling: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LevelBWhereStatus {
    Mapped { records: usize },
    SkippedNotInClientView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LevelBSampleStatus {
    NoNumberedPendingChange,
    Checked {
        described_files: usize,
        opened_files: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelBReport {
    pub identity: LevelBIdentitySummary,
    pub pending_changes_sampled: usize,
    pub sample: LevelBSampleStatus,
    pub where_status: LevelBWhereStatus,
}

impl fmt::Display for LevelBReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "Level B READ-ONLY compatibility check")?;
        writeln!(formatter, "write_commands=disabled")?;
        writeln!(formatter, "server={}", self.identity.server)?;
        writeln!(formatter, "user={}", self.identity.user)?;
        writeln!(formatter, "client={}", self.identity.client)?;
        writeln!(formatter, "charset={}", self.identity.charset)?;
        writeln!(formatter, "case_handling={}", self.identity.case_handling)?;
        writeln!(
            formatter,
            "pending_changes_sampled={} limit={MAX_LEVEL_B_CHANGES}",
            self.pending_changes_sampled
        )?;
        match self.sample {
            LevelBSampleStatus::NoNumberedPendingChange => {
                writeln!(
                    formatter,
                    "sample_change=skipped:no-numbered-pending-change"
                )?;
            }
            LevelBSampleStatus::Checked {
                described_files,
                opened_files,
            } => {
                writeln!(
                    formatter,
                    "sample_change=checked described_files={described_files} opened_files={opened_files}"
                )?;
            }
        }
        match self.where_status {
            LevelBWhereStatus::Mapped { records } => {
                writeln!(formatter, "cwd_where=mapped records={records}")?;
                formatter.write_str("result=passed")
            }
            LevelBWhereStatus::SkippedNotInClientView => {
                writeln!(formatter, "cwd_where=skipped:not-in-client-view")?;
                formatter.write_str("result=completed-with-skip")
            }
        }
    }
}

#[derive(Debug)]
pub enum LevelBError {
    Query {
        command: &'static str,
        source: P4Error,
    },
    Mapping {
        command: &'static str,
        source: DomainMappingError,
    },
}

impl fmt::Display for LevelBError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Query { command, source } => {
                write!(formatter, "Level B {command} query failed: {source}")
            }
            Self::Mapping { command, source } => {
                write!(
                    formatter,
                    "Level B {command} output is unsupported: {source}"
                )
            }
        }
    }
}

impl Error for LevelBError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Query { source, .. } => Some(source),
            Self::Mapping { source, .. } => Some(source),
        }
    }
}

/// Runs the Level B query allowlist once, without retrying through another P4
/// configuration. The caller provides the exact executable and cwd through the
/// client; this function never creates a write-capable request.
pub fn run_level_b_read_only<T: P4Transport>(
    client: &P4Client<T>,
    cwd: &Path,
) -> Result<LevelBReport, LevelBError> {
    let info = run_query(client, "info", &P4Query::Info)?;
    let workspace = workspace_from_info(&info.records).map_err(|source| LevelBError::Mapping {
        command: "info",
        source,
    })?;
    let charset = info
        .records
        .iter()
        .find_map(|record| record.string("unicode"))
        .map_or("unknown", |value| {
            match value.to_ascii_lowercase().as_str() {
                "enabled" => "unicode-enabled",
                "disabled" => "unicode-disabled",
                _ => "unknown",
            }
        });
    let case_handling = match workspace.case_handling.canonical_name() {
        "sensitive" => "sensitive",
        "insensitive" => "insensitive",
        "hybrid" => "hybrid",
        _ => "unknown",
    };

    let changes = run_query(
        client,
        "changes",
        &P4Query::PendingChangesLimited {
            user: workspace.user.clone(),
            client: workspace.client.clone(),
            max_results: MAX_LEVEL_B_CHANGES,
        },
    )?;
    let pending =
        changelists_from_changes(&changes.records).map_err(|source| LevelBError::Mapping {
            command: "changes",
            source,
        })?;

    let sample = if let Some(change) = pending.iter().find_map(|change| match change.id {
        ChangelistId::Numbered(number) => Some(number),
        ChangelistId::Default => None,
    }) {
        let describe = run_query(client, "describe", &P4Query::DescribeSummary { change })?;
        let described =
            changelist_from_describe(&describe.records).map_err(|source| LevelBError::Mapping {
                command: "describe",
                source,
            })?;
        let opened = run_query(
            client,
            "opened",
            &P4Query::Opened {
                change: ChangelistId::Numbered(change),
            },
        )?;
        let opened_files =
            changed_files_from_opened(&opened.records).map_err(|source| LevelBError::Mapping {
                command: "opened",
                source,
            })?;
        LevelBSampleStatus::Checked {
            described_files: described.files.len(),
            opened_files: opened_files.len(),
        }
    } else {
        LevelBSampleStatus::NoNumberedPendingChange
    };

    let where_status = match client.run(&P4Query::Where {
        path: cwd.join("..."),
    }) {
        Ok(response) => {
            let records = response
                .records
                .iter()
                .filter(|record| matches!(record.code, RecordCode::Stat))
                .count();
            if records == 0 {
                LevelBWhereStatus::SkippedNotInClientView
            } else {
                LevelBWhereStatus::Mapped { records }
            }
        }
        Err(error) if error.kind == P4ErrorKind::NotInClientView => {
            LevelBWhereStatus::SkippedNotInClientView
        }
        Err(source) => {
            return Err(LevelBError::Query {
                command: "where",
                source,
            });
        }
    };

    Ok(LevelBReport {
        identity: LevelBIdentitySummary {
            server: redacted_fingerprint("server", &workspace.server_id),
            user: redacted_fingerprint("user", &workspace.user),
            client: redacted_fingerprint("client", &workspace.client),
            charset,
            case_handling,
        },
        pending_changes_sampled: pending.len(),
        sample,
        where_status,
    })
}

fn run_query<T: P4Transport>(
    client: &P4Client<T>,
    command: &'static str,
    query: &P4Query,
) -> Result<super::transport::P4Response, LevelBError> {
    client
        .run(query)
        .map_err(|source| LevelBError::Query { command, source })
}

fn redacted_fingerprint(kind: &str, value: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"herdr-p4/level-b-redaction/v1");
    hasher.update(kind.as_bytes());
    hasher.update(&[0]);
    hasher.update(value.as_bytes());
    let hex = hasher.finalize().to_hex().to_string();
    format!("configured#{}", &hex[..12])
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::p4::RawP4Output;
    use crate::p4::fake::FakeP4Transport;

    fn output(stdout: &[u8], exit_code: i32) -> RawP4Output {
        RawP4Output {
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
            exit_code,
            elapsed: Duration::from_millis(1),
        }
    }

    fn client(fake: FakeP4Transport) -> P4Client<FakeP4Transport> {
        P4Client::new_with_directory_environment(
            fake,
            "p4",
            "C:/Secret Workspace",
            Default::default(),
        )
    }

    fn info_fixture() -> &'static [u8] {
        br#"{"serverAddress":"internal.example:1666","userName":"SecretUser","clientName":"SecretClient","clientRoot":"C:/Secret Workspace","caseHandling":"insensitive","unicode":"enabled"}"#
    }

    #[test]
    fn runner_only_emits_allowlisted_read_queries_and_redacts_identity() {
        let fake = FakeP4Transport::default();
        fake.push_output(output(info_fixture(), 0));
        fake.push_output(output(
            br#"{"change":"42","status":"pending","user":"SecretUser","client":"SecretClient","desc":"Internal work"}"#,
            0,
        ));
        fake.push_output(output(
            br#"{"change":"42","status":"pending","user":"SecretUser","client":"SecretClient","desc":"Internal work","depotFile0":"//SecretDepot/a.txt","action0":"edit","type0":"text","rev0":"3"}"#,
            0,
        ));
        fake.push_output(output(
            br#"{"change":"42","user":"SecretUser","client":"SecretClient","depotFile":"//SecretDepot/a.txt","clientFile":"C:/Secret Workspace/a.txt","action":"edit","type":"text","haveRev":"3"}"#,
            0,
        ));
        fake.push_output(output(
            br#"{"depotFile":"//SecretDepot","clientFile":"//SecretClient","path":"C:/Secret Workspace"}"#,
            0,
        ));

        let report = run_level_b_read_only(&client(fake.clone()), Path::new("C:/Secret Workspace"))
            .expect("read-only compatibility check should pass");
        let rendered = report.to_string();

        assert_eq!(report.pending_changes_sampled, 1);
        assert_eq!(
            report.sample,
            LevelBSampleStatus::Checked {
                described_files: 1,
                opened_files: 1
            }
        );
        for secret in [
            "internal.example",
            "SecretUser",
            "SecretClient",
            "SecretDepot",
            "Secret Workspace",
            "Internal work",
        ] {
            assert!(!rendered.contains(secret), "report leaked {secret:?}");
        }
        assert!(rendered.contains("write_commands=disabled"));
        assert!(rendered.contains("result=passed"));

        let requests = fake.requests();
        let command_names = requests
            .iter()
            .map(|request| request.args[2].to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            command_names,
            ["info", "changes", "describe", "opened", "where"]
        );
        assert_eq!(requests[1].args[3], "-m");
        assert_eq!(requests[1].args[4].to_string_lossy(), "8");
        assert_eq!(
            requests[4].args[3],
            Path::new("C:/Secret Workspace").join("...").as_os_str()
        );
        assert!(requests.iter().all(|request| request.stdin.is_empty()));
    }

    #[test]
    fn unmapped_cwd_is_a_clear_skip_without_configuration_fallback() {
        let fake = FakeP4Transport::default();
        fake.push_output(output(info_fixture(), 0));
        fake.push_output(output(b"", 0));
        fake.push_output(output(
            br#"{"data":"Path is not under client's root","generic":17,"severity":3}"#,
            1,
        ));

        let report = run_level_b_read_only(&client(fake.clone()), Path::new("D:/Outside"))
            .expect("unmapped paths are an allowed Level B skip");

        assert_eq!(report.sample, LevelBSampleStatus::NoNumberedPendingChange);
        assert_eq!(
            report.where_status,
            LevelBWhereStatus::SkippedNotInClientView
        );
        assert!(report.to_string().contains("result=completed-with-skip"));
        assert_eq!(fake.requests().len(), 3);
        assert_eq!(
            fake.requests()[2].args[3],
            Path::new("D:/Outside").join("...").as_os_str()
        );
        assert_eq!(fake.remaining_steps(), 0);
    }

    #[test]
    fn empty_where_mapping_is_a_clear_skip() {
        let fake = FakeP4Transport::default();
        fake.push_output(output(info_fixture(), 0));
        fake.push_output(output(b"", 0));
        fake.push_output(output(b"", 0));

        let report = run_level_b_read_only(&client(fake.clone()), Path::new("C:/Secret Workspace"))
            .expect("zero mapping records are an allowed Level B skip");

        assert_eq!(
            report.where_status,
            LevelBWhereStatus::SkippedNotInClientView
        );
        assert!(report.to_string().contains("result=completed-with-skip"));
        assert_eq!(fake.requests().len(), 3);
        assert_eq!(fake.remaining_steps(), 0);
    }

    #[test]
    fn info_failure_stops_without_trying_another_configuration() {
        let fake = FakeP4Transport::default();
        fake.push_output(output(
            br#"{"data":"Connect to server failed","generic":13,"severity":3}"#,
            1,
        ));

        let error = run_level_b_read_only(&client(fake.clone()), Path::new("C:/Secret Workspace"))
            .expect_err("connection failure must stop the run");

        assert!(matches!(
            error,
            LevelBError::Query {
                command: "info",
                source: P4Error {
                    kind: P4ErrorKind::NetworkUnavailable,
                    ..
                }
            }
        ));
        assert_eq!(fake.requests().len(), 1);
    }

    #[test]
    fn fingerprints_are_stable_but_domain_separated() {
        assert_eq!(
            redacted_fingerprint("server", "same"),
            redacted_fingerprint("server", "same")
        );
        assert_ne!(
            redacted_fingerprint("server", "same"),
            redacted_fingerprint("user", "same")
        );
    }
}
