use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, time::Duration};

use super::{
    command::P4Query,
    env::herdr_control_variable_names,
    error::{P4Error, P4ErrorKind, classify_command_failure, known_error},
    parser::{StructuredRecord, parse_json_records},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputLimits {
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

impl Default for OutputLimits {
    fn default() -> Self {
        Self {
            stdout_bytes: 8 * 1024 * 1024,
            stderr_bytes: 256 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct P4Request {
    pub executable: PathBuf,
    pub cwd: PathBuf,
    pub args: Vec<OsString>,
    pub stdin: Vec<u8>,
    /// Extra variables overlaid on the inherited process environment.
    ///
    /// An empty map means "do not add variables", not "clear the environment".
    pub environment: BTreeMap<OsString, OsString>,
    /// Names stripped from the inherited environment before `environment` is applied.
    pub removed_environment: Vec<OsString>,
    pub timeout: Duration,
    pub output_limits: OutputLimits,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawP4Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
    pub elapsed: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    ExecutableMissing,
    SpawnFailed,
    TimedOut,
    Cancelled,
    OutputLimitExceeded,
}

/// Executes one P4 request.
///
/// Implementations must honor `timeout` and `output_limits` *during* execution:
/// kill the child when time or bytes are exceeded. Returning a complete late or
/// oversize buffer for a later client check is not sufficient.
pub trait P4Transport: Send + Sync {
    fn execute(&self, request: &P4Request) -> Result<RawP4Output, TransportError>;
}

pub(crate) fn enforce_request_bounds(
    request: &P4Request,
    output: RawP4Output,
) -> Result<RawP4Output, TransportError> {
    if output.elapsed > request.timeout {
        return Err(TransportError::TimedOut);
    }
    if output.stdout.len() > request.output_limits.stdout_bytes
        || output.stderr.len() > request.output_limits.stderr_bytes
    {
        return Err(TransportError::OutputLimitExceeded);
    }
    Ok(output)
}

#[derive(Debug, Clone, PartialEq)]
pub struct P4Response {
    pub records: Vec<StructuredRecord>,
    pub elapsed: Duration,
}

pub struct P4Client<T> {
    transport: T,
    executable: PathBuf,
    cwd: PathBuf,
    timeout: Duration,
    output_limits: OutputLimits,
}

impl<T: P4Transport> P4Client<T> {
    #[must_use]
    pub fn new(transport: T, executable: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            transport,
            executable: executable.into(),
            cwd: cwd.into(),
            timeout: Duration::from_secs(30),
            output_limits: OutputLimits::default(),
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_output_limits(mut self, output_limits: OutputLimits) -> Self {
        self.output_limits = output_limits;
        self
    }

    #[must_use]
    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn run(&self, query: &P4Query) -> Result<P4Response, P4Error> {
        let request = P4Request {
            executable: self.executable.clone(),
            cwd: self.cwd.clone(),
            args: query.args(),
            stdin: Vec::new(),
            environment: BTreeMap::new(),
            removed_environment: herdr_control_variable_names(),
            timeout: self.timeout,
            output_limits: self.output_limits,
        };

        let output = self
            .transport
            .execute(&request)
            .map_err(map_transport_error)?;

        if output.elapsed > self.timeout {
            return Err(known_error(P4ErrorKind::TimedOut));
        }
        if output.stdout.len() > self.output_limits.stdout_bytes
            || output.stderr.len() > self.output_limits.stderr_bytes
        {
            return Err(known_error(P4ErrorKind::OutputLimitExceeded));
        }

        let records = match parse_json_records(&output.stdout) {
            Ok(records) => records,
            Err(_) if output.exit_code != 0 => {
                return Err(classify_command_failure(&[], &output.stderr));
            }
            Err(_) => return Err(known_error(P4ErrorKind::MalformedOutput)),
        };

        if output.exit_code != 0
            || records
                .iter()
                .any(|record| matches!(record.code, super::parser::RecordCode::Error))
        {
            return Err(classify_command_failure(&records, &output.stderr));
        }

        Ok(P4Response {
            records,
            elapsed: output.elapsed,
        })
    }
}

fn map_transport_error(error: TransportError) -> P4Error {
    known_error(match error {
        TransportError::ExecutableMissing => P4ErrorKind::ExecutableMissing,
        TransportError::SpawnFailed => P4ErrorKind::CommandFailed,
        TransportError::TimedOut => P4ErrorKind::TimedOut,
        TransportError::Cancelled => P4ErrorKind::Cancelled,
        TransportError::OutputLimitExceeded => P4ErrorKind::OutputLimitExceeded,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::p4::fake::FakeP4Transport;

    fn stat_output() -> RawP4Output {
        RawP4Output {
            stdout: br#"{"code":"stat","clientName":"ExampleClientA"}"#.to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::from_millis(5),
        }
    }

    #[test]
    fn fake_records_exact_argv_and_cwd() {
        let fake = FakeP4Transport::default();
        fake.push_output(stat_output());
        let client = P4Client::new(fake.clone(), "p4", "C:/Example Workspace");

        client
            .run(&P4Query::PendingChanges {
                user: "Example User".into(),
                client: "ExampleClientA".into(),
            })
            .expect("query should succeed");

        let requests = fake.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].cwd, PathBuf::from("C:/Example Workspace"));
        assert_eq!(requests[0].args[6], "Example User");
        assert!(requests[0].stdin.is_empty());
        assert!(requests[0].environment.is_empty());
        assert!(
            requests[0]
                .removed_environment
                .iter()
                .any(|name| name == "HERDR_BIN_PATH")
        );
        assert!(
            !requests[0]
                .removed_environment
                .iter()
                .any(|name| name == "P4PASSWD")
        );
    }

    #[test]
    fn timeout_is_deterministic_without_sleeping() {
        let fake = FakeP4Transport::default();
        let mut output = stat_output();
        output.elapsed = Duration::from_secs(2);
        fake.push_output(output);
        let client = P4Client::new(fake, "p4", ".").with_timeout(Duration::from_secs(1));

        let error = client.run(&P4Query::Info).expect_err("request is late");
        assert_eq!(error.kind, P4ErrorKind::TimedOut);
    }

    #[test]
    fn output_budget_failure_is_not_an_empty_result() {
        let fake = FakeP4Transport::default();
        fake.push_output(stat_output());
        let client = P4Client::new(fake, "p4", ".").with_output_limits(OutputLimits {
            stdout_bytes: 4,
            stderr_bytes: 4,
        });

        let error = client
            .run(&P4Query::Info)
            .expect_err("output should exceed budget");
        assert_eq!(error.kind, P4ErrorKind::OutputLimitExceeded);
    }

    #[test]
    fn empty_success_is_distinct_from_command_failure() {
        let fake = FakeP4Transport::default();
        fake.push_output(RawP4Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::ZERO,
        });
        fake.push_output(RawP4Output {
            stdout: Vec::new(),
            stderr: b"no permission for operation".to_vec(),
            exit_code: 1,
            elapsed: Duration::ZERO,
        });
        let client = P4Client::new(fake, "p4", ".");

        let empty = client.run(&P4Query::Info).expect("empty success is valid");
        assert!(empty.records.is_empty());
        let denied = client
            .run(&P4Query::Info)
            .expect_err("failure must surface");
        assert_eq!(denied.kind, P4ErrorKind::PermissionDenied);
    }
}
