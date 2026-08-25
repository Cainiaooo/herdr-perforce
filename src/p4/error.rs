use std::{error::Error, fmt};

use super::parser::{RecordCode, StructuredRecord};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum P4ErrorKind {
    ExecutableMissing,
    NotInClientView,
    NetworkUnavailable,
    AuthenticationExpired,
    TrustRequired,
    PermissionDenied,
    MalformedOutput,
    CommandFailed,
    TimedOut,
    Cancelled,
    OutputLimitExceeded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct P4Error {
    pub kind: P4ErrorKind,
    pub message: String,
}

impl P4Error {
    #[must_use]
    pub fn new(kind: P4ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for P4Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for P4Error {}

pub(crate) fn classify_command_failure(records: &[StructuredRecord], stderr: &[u8]) -> P4Error {
    let structured_error = records
        .iter()
        .find(|record| matches!(record.code, RecordCode::Error));

    if let Some(generic) = structured_error.and_then(|record| record.string("generic")) {
        match generic.as_str() {
            "6" => return known_error(P4ErrorKind::PermissionDenied),
            "13" => return known_error(P4ErrorKind::NetworkUnavailable),
            _ => {}
        }
    }

    let structured_text = structured_error.and_then(|record| record.string("data"));
    let stderr_text = String::from_utf8_lossy(stderr);
    let diagnostic = structured_text.as_deref().unwrap_or(&stderr_text);
    let lower = diagnostic.to_ascii_lowercase();

    let kind = if contains_any(&lower, &["p4 trust", "authenticity of", "fingerprint"]) {
        P4ErrorKind::TrustRequired
    } else if contains_any(
        &lower,
        &[
            "password invalid",
            "password unset",
            "ticket expired",
            "session has expired",
            "session expired",
            "not logged in",
            "login required",
        ],
    ) {
        P4ErrorKind::AuthenticationExpired
    } else if contains_any(
        &lower,
        &[
            "connect to server failed",
            "tcp connect",
            "network is unreachable",
            "connection refused",
        ],
    ) {
        P4ErrorKind::NetworkUnavailable
    } else if contains_any(
        &lower,
        &[
            "not in client view",
            "not under client's root",
            "file(s) not in client view",
        ],
    ) {
        P4ErrorKind::NotInClientView
    } else if contains_any(
        &lower,
        &["no permission", "protections table", "permission denied"],
    ) {
        P4ErrorKind::PermissionDenied
    } else {
        P4ErrorKind::CommandFailed
    };

    known_error(kind)
}

pub(crate) fn known_error(kind: P4ErrorKind) -> P4Error {
    let message = match kind {
        P4ErrorKind::ExecutableMissing => {
            "the configured p4 executable was not found; install Helix Command-Line Client or set the p4 path"
        }
        P4ErrorKind::NotInClientView => {
            "the workspace path is not in the current client view; open a directory mapped by the current client"
        }
        P4ErrorKind::NetworkUnavailable => {
            "the Perforce server is unavailable; check the network and P4PORT, then retry"
        }
        P4ErrorKind::AuthenticationExpired => {
            "Perforce authentication is required or expired; run p4 login in this workspace"
        }
        P4ErrorKind::TrustRequired => {
            "the Perforce server requires an explicit trust decision; verify the fingerprint before trusting"
        }
        P4ErrorKind::PermissionDenied => {
            "the Perforce server denied access; the current user cannot read this resource"
        }
        P4ErrorKind::MalformedOutput => {
            "p4 returned malformed or unsupported structured output; retry after confirming the p4 version supports -ztag -Mj"
        }
        P4ErrorKind::CommandFailed => {
            "the p4 command failed; retry the same read-only query after checking the classified error"
        }
        P4ErrorKind::TimedOut => "the p4 command timed out; retry or increase the timeout",
        P4ErrorKind::Cancelled => "the p4 command was cancelled",
        P4ErrorKind::OutputLimitExceeded => {
            "p4 output exceeded the configured byte budget; narrow the query or raise the limit"
        }
    };
    P4Error::new(kind, message)
}

fn contains_any(value: &str, candidates: &[&str]) -> bool {
    candidates.iter().any(|candidate| value.contains(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p4::parse_json_records;

    #[test]
    fn structured_protection_error_wins_over_localized_text() {
        let records = parse_json_records(
            br#"{"code":"error","severity":"3","generic":"6","data":"localized"}"#,
        )
        .expect("fixture should parse");

        assert_eq!(
            classify_command_failure(&records, b"").kind,
            P4ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn raw_server_diagnostic_is_not_exposed() {
        let error = classify_command_failure(&[], b"Connect to server failed; tcp:internal:1666");
        assert_eq!(error.kind, P4ErrorKind::NetworkUnavailable);
        assert!(!error.message.contains("internal"));
    }

    #[test]
    fn structured_comm_error_is_network_unavailable() {
        let records = parse_json_records(
            br#"{"code":"error","severity":"3","generic":"13","data":"localized"}"#,
        )
        .expect("fixture should parse");

        assert_eq!(
            classify_command_failure(&records, b"").kind,
            P4ErrorKind::NetworkUnavailable
        );
    }

    #[test]
    fn trust_login_and_client_view_are_distinct() {
        assert_eq!(
            classify_command_failure(
                &[],
                b"The authenticity of 'ssl:example:1666' can't be established"
            )
            .kind,
            P4ErrorKind::TrustRequired
        );
        assert_eq!(
            classify_command_failure(&[], b"Your session has expired, please login again").kind,
            P4ErrorKind::AuthenticationExpired
        );
        assert_eq!(
            classify_command_failure(
                &[],
                b"Path 'C:/Example/secret.txt' is not under client's root"
            )
            .kind,
            P4ErrorKind::NotInClientView
        );
    }
}
