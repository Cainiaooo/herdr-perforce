//! Deterministic, in-memory P4 transport used by Level A contract tests.
//!
//! It records the exact request and returns scripted stdout, stderr, exit code,
//! elapsed time or transport failure without consulting the host P4 setup.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use super::transport::{
    P4Request, P4Transport, RawP4Output, TransportError, enforce_request_bounds,
};

#[derive(Debug, Clone, Default)]
pub struct FakeP4Transport {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    steps: Mutex<VecDeque<Result<RawP4Output, TransportError>>>,
    requests: Mutex<Vec<P4Request>>,
}

impl FakeP4Transport {
    pub fn push_output(&self, output: RawP4Output) {
        self.push_step(Ok(output));
    }

    pub fn push_error(&self, error: TransportError) {
        self.push_step(Err(error));
    }

    pub fn push_step(&self, step: Result<RawP4Output, TransportError>) {
        self.inner
            .steps
            .lock()
            .expect("fake P4 step lock poisoned")
            .push_back(step);
    }

    #[must_use]
    pub fn requests(&self) -> Vec<P4Request> {
        self.inner
            .requests
            .lock()
            .expect("fake P4 request lock poisoned")
            .clone()
    }

    #[must_use]
    pub fn remaining_steps(&self) -> usize {
        self.inner
            .steps
            .lock()
            .expect("fake P4 step lock poisoned")
            .len()
    }
}

impl P4Transport for FakeP4Transport {
    fn execute(&self, request: &P4Request) -> Result<RawP4Output, TransportError> {
        self.inner
            .requests
            .lock()
            .expect("fake P4 request lock poisoned")
            .push(request.clone());
        let step = self
            .inner
            .steps
            .lock()
            .expect("fake P4 step lock poisoned")
            .pop_front()
            .unwrap_or(Err(TransportError::SpawnFailed));
        match step {
            Ok(output) => enforce_request_bounds(request, output),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use super::*;
    use crate::p4::OutputLimits;

    #[test]
    fn unscripted_call_fails_closed_and_is_recorded() {
        let fake = FakeP4Transport::default();
        let request = P4Request {
            executable: PathBuf::from("p4"),
            cwd: PathBuf::from("C:/Example"),
            args: vec!["info".into()],
            stdin: Vec::new(),
            environment: Default::default(),
            removed_environment: Vec::new(),
            timeout: Duration::from_secs(1),
            output_limits: OutputLimits::default(),
        };

        assert_eq!(fake.execute(&request), Err(TransportError::SpawnFailed));
        assert_eq!(fake.requests(), vec![request]);
    }

    #[test]
    fn scripted_oversize_and_late_output_are_rejected_during_execute() {
        let fake = FakeP4Transport::default();
        fake.push_output(RawP4Output {
            stdout: vec![0; 8],
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::ZERO,
        });
        fake.push_output(RawP4Output {
            stdout: b"{}".to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: Duration::from_secs(2),
        });

        let mut request = P4Request {
            executable: PathBuf::from("p4"),
            cwd: PathBuf::from("C:/Example"),
            args: vec!["info".into()],
            stdin: Vec::new(),
            environment: Default::default(),
            removed_environment: Vec::new(),
            timeout: Duration::from_secs(1),
            output_limits: OutputLimits {
                stdout_bytes: 4,
                stderr_bytes: 4,
            },
        };

        assert_eq!(
            fake.execute(&request),
            Err(TransportError::OutputLimitExceeded)
        );

        request.output_limits = OutputLimits::default();
        assert_eq!(fake.execute(&request), Err(TransportError::TimedOut));
    }
}
