//! Process-backed P4 transport.
//!
//! Timeout and stdout/stderr byte budgets are enforced while the child runs:
//! oversized output stops reading and kills the process; a late process is
//! killed instead of returning a complete buffer after the fact.

use std::{
    io::Write,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use super::{
    bounded::{BoundedReadError, read_limited},
    env::environment_keys_to_remove,
    transport::{P4Request, P4Transport, RawP4Output, TransportError},
};

#[derive(Debug, Default, Clone, Copy)]
pub struct StdProcessTransport;

impl P4Transport for StdProcessTransport {
    fn execute(&self, request: &P4Request) -> Result<RawP4Output, TransportError> {
        let mut command = Command::new(&request.executable);
        command
            .args(&request.args)
            .current_dir(&request.cwd)
            .stdin(if request.stdin.is_empty() {
                Stdio::null()
            } else {
                Stdio::piped()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        for key in environment_keys_to_remove(request) {
            command.env_remove(key);
        }
        for (key, value) in &request.environment {
            command.env(key, value);
        }

        let started = Instant::now();
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(TransportError::ExecutableMissing);
            }
            Err(_) => return Err(TransportError::SpawnFailed),
        };

        if !request.stdin.is_empty() {
            let Some(mut stdin) = child.stdin.take() else {
                let _ = child.kill();
                return Err(TransportError::SpawnFailed);
            };
            if stdin.write_all(&request.stdin).is_err() {
                let _ = child.kill();
                return Err(TransportError::SpawnFailed);
            }
        }
        drop(child.stdin.take());

        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            return Err(TransportError::SpawnFailed);
        };
        let Some(stderr) = child.stderr.take() else {
            let _ = child.kill();
            return Err(TransportError::SpawnFailed);
        };

        let stdout_limit = request.output_limits.stdout_bytes;
        let stderr_limit = request.output_limits.stderr_bytes;
        let (limit_tx, limit_rx) = mpsc::channel();
        let stdout_limit_tx = limit_tx.clone();
        let stdout_thread = thread::spawn(move || {
            let result = read_limited(stdout, stdout_limit);
            if matches!(result, Err(BoundedReadError::LimitExceeded)) {
                let _ = stdout_limit_tx.send(());
            }
            result
        });
        let stderr_thread = thread::spawn(move || {
            let result = read_limited(stderr, stderr_limit);
            if matches!(result, Err(BoundedReadError::LimitExceeded)) {
                let _ = limit_tx.send(());
            }
            result
        });

        match wait_for_child(&mut child, request.timeout, &limit_rx, started) {
            ChildWait::Aborted => {
                let _ = child.kill();
                let _ = child.wait();
                let stdout_result = stdout_thread.join();
                let stderr_result = stderr_thread.join();
                if output_hit_limit(&stdout_result) || output_hit_limit(&stderr_result) {
                    Err(TransportError::OutputLimitExceeded)
                } else {
                    Err(TransportError::TimedOut)
                }
            }
            ChildWait::Exited(status) => {
                let stdout = join_output(stdout_thread)?;
                let stderr = join_output(stderr_thread)?;
                Ok(RawP4Output {
                    stdout,
                    stderr,
                    exit_code: status.code().unwrap_or(-1),
                    elapsed: started.elapsed(),
                })
            }
        }
    }
}

enum ChildWait {
    Exited(std::process::ExitStatus),
    Aborted,
}

fn wait_for_child(
    child: &mut Child,
    timeout: Duration,
    limit_rx: &mpsc::Receiver<()>,
    started: Instant,
) -> ChildWait {
    loop {
        if limit_rx.try_recv().is_ok() {
            return ChildWait::Aborted;
        }
        match child.try_wait() {
            Ok(Some(status)) => return ChildWait::Exited(status),
            Ok(None) => {
                if started.elapsed() >= timeout {
                    return ChildWait::Aborted;
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return ChildWait::Aborted,
        }
    }
}

fn join_output(
    handle: thread::JoinHandle<Result<Vec<u8>, BoundedReadError>>,
) -> Result<Vec<u8>, TransportError> {
    match handle.join() {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(BoundedReadError::LimitExceeded)) => Err(TransportError::OutputLimitExceeded),
        Ok(Err(BoundedReadError::Io(_))) | Err(_) => Err(TransportError::SpawnFailed),
    }
}

fn output_hit_limit(result: &thread::Result<Result<Vec<u8>, BoundedReadError>>) -> bool {
    matches!(result, Ok(Err(BoundedReadError::LimitExceeded)))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, time::Duration};

    use super::*;
    use crate::p4::transport::{OutputLimits, P4Request};

    fn request(
        executable: &str,
        args: &[&str],
        timeout: Duration,
        stdout_bytes: usize,
    ) -> P4Request {
        P4Request {
            executable: PathBuf::from(executable),
            cwd: PathBuf::from("."),
            args: args.iter().map(OsString::from).collect(),
            stdin: Vec::new(),
            environment: BTreeMap::new(),
            removed_environment: Vec::new(),
            timeout,
            output_limits: OutputLimits {
                stdout_bytes,
                stderr_bytes: 64 * 1024,
            },
        }
    }

    #[test]
    fn missing_executable_is_classified() {
        let error = StdProcessTransport
            .execute(&request(
                "herdr-p4-missing-executable",
                &[],
                Duration::from_secs(1),
                1024,
            ))
            .expect_err("missing binary");
        assert_eq!(error, TransportError::ExecutableMissing);
    }

    #[test]
    fn host_echo_is_collected_within_budget() {
        let (executable, args) = if cfg!(windows) {
            ("cmd", vec!["/C", "echo", "ok"])
        } else {
            ("echo", vec!["ok"])
        };
        let output = StdProcessTransport
            .execute(&request(
                executable,
                &args,
                Duration::from_secs(5),
                64 * 1024,
            ))
            .expect("host echo should run");
        assert_eq!(output.exit_code, 0);
        let stdout = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
        assert!(stdout.contains("ok"), "stdout was {stdout:?}");
    }

    #[test]
    fn output_budget_kills_oversize_stdout() {
        let (executable, args) = if cfg!(windows) {
            ("cmd", vec!["/C", "echo", "hello-world"])
        } else {
            ("echo", vec!["hello-world"])
        };
        let error = StdProcessTransport
            .execute(&request(executable, &args, Duration::from_secs(5), 2))
            .expect_err("echo exceeds 2 bytes");
        assert_eq!(error, TransportError::OutputLimitExceeded);
    }

    #[test]
    fn timeout_kills_a_long_running_process() {
        let (executable, args) = if cfg!(windows) {
            ("ping", vec!["-n", "20", "127.0.0.1"])
        } else {
            ("sleep", vec!["20"])
        };
        let error = StdProcessTransport
            .execute(&request(
                executable,
                &args,
                Duration::from_millis(200),
                64 * 1024,
            ))
            .expect_err("sleeping process should time out");
        assert_eq!(error, TransportError::TimedOut);
    }
}
