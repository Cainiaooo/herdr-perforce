use std::{env, ffi::OsString, fs, path::PathBuf, process::ExitCode};

use herdr_perforce::p4::{P4Client, StdProcessTransport, run_level_b_read_only};

const HELP: &str = "herdr-p4 - compact Perforce review pane for Herdr\n\n\
Usage:\n  herdr-p4 --version\n  herdr-p4 --help\n  herdr-p4 level-b --read-only [--cwd <workspace-path>]\n\n\
Level B is explicitly opt-in and only runs bounded info, changes, describe, opened,\n\
and where queries. It never runs a write command or retries through another config.\n\n\
The interactive Herdr pane entrypoint is not available in this foundation build.";

fn main() -> ExitCode {
    match parse_command(env::args_os().skip(1).collect()) {
        Ok(Command::Version) => {
            println!("herdr-p4 {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Command::Help) => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Ok(Command::LevelB { cwd }) => run_level_b(cwd),
        Err(message) => {
            eprintln!("{message}; run herdr-p4 --help");
            ExitCode::from(64)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Help,
    Version,
    LevelB { cwd: Option<PathBuf> },
}

fn parse_command(args: Vec<OsString>) -> Result<Command, &'static str> {
    match args.as_slice() {
        [] => Ok(Command::Help),
        [arg] if arg == "--help" || arg == "-h" => Ok(Command::Help),
        [arg] if arg == "--version" || arg == "-V" => Ok(Command::Version),
        [level, acknowledgement] if level == "level-b" && acknowledgement == "--read-only" => {
            Ok(Command::LevelB { cwd: None })
        }
        [level, acknowledgement, cwd_flag, cwd]
            if level == "level-b" && acknowledgement == "--read-only" && cwd_flag == "--cwd" =>
        {
            Ok(Command::LevelB {
                cwd: Some(PathBuf::from(cwd)),
            })
        }
        [level, acknowledgement, ..] if level == "level-b" && acknowledgement == "--read-only" => {
            Err("Level B accepts only --read-only [--cwd <workspace-path>]")
        }
        [level, ..] if level == "level-b" => {
            Err("Level B requires the exact --read-only acknowledgement")
        }
        _ => Err("unsupported arguments"),
    }
}

fn run_level_b(cwd: Option<PathBuf>) -> ExitCode {
    let cwd = match env::current_dir()
        .ok()
        .and_then(|current_dir| resolve_level_b_cwd(cwd, current_dir))
    {
        Some(cwd) => cwd,
        None => {
            eprintln!("Level B could not resolve the requested working directory");
            return ExitCode::from(66);
        }
    };
    let client = P4Client::new(StdProcessTransport, "p4", &cwd);
    match run_level_b_read_only(&client, &cwd) {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(69)
        }
    }
}

/// Relative paths are joined, not canonicalized: Windows `\\?\` prefixes break `p4`.
fn resolve_level_b_cwd(requested: Option<PathBuf>, current_dir: PathBuf) -> Option<PathBuf> {
    let resolved = match requested {
        None => current_dir,
        Some(path) if path.is_absolute() => path,
        Some(path) => current_dir.join(path),
    };
    fs::read_dir(&resolved).ok()?;
    Some(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn level_b_requires_explicit_read_only_acknowledgement() {
        assert_eq!(
            parse_command(strings(&["level-b", "--read-only"])),
            Ok(Command::LevelB { cwd: None })
        );
        assert_eq!(
            parse_command(strings(&["level-b"])),
            Err("Level B requires the exact --read-only acknowledgement")
        );
        assert_eq!(
            parse_command(strings(&["level-b", "--write"])),
            Err("Level B requires the exact --read-only acknowledgement")
        );
    }

    #[test]
    fn level_b_accepts_one_explicit_working_directory() {
        assert_eq!(
            parse_command(strings(&[
                "level-b",
                "--read-only",
                "--cwd",
                "C:/Example Workspace"
            ])),
            Ok(Command::LevelB {
                cwd: Some(PathBuf::from("C:/Example Workspace"))
            })
        );
        assert_eq!(
            parse_command(strings(&["level-b", "--read-only", "--cwd"])),
            Err("Level B accepts only --read-only [--cwd <workspace-path>]")
        );
        assert_eq!(
            parse_command(strings(&["level-b", "--read-only", "--help"])),
            Err("Level B accepts only --read-only [--cwd <workspace-path>]")
        );
        assert_eq!(
            parse_command(strings(&[
                "level-b",
                "--read-only",
                "--cwd",
                "C:/Example",
                "extra"
            ])),
            Err("Level B accepts only --read-only [--cwd <workspace-path>]")
        );
    }

    #[test]
    fn relative_cwd_is_joined_to_the_process_directory() {
        let current_dir =
            env::temp_dir().join(format!("herdr-p4-cwd-relative-{}", std::process::id()));
        let requested = PathBuf::from("mapped-ws");
        let workspace = current_dir.join(&requested);
        fs::create_dir_all(&workspace).expect("temp workspace");
        let resolved = resolve_level_b_cwd(Some(requested.clone()), current_dir.clone());
        fs::remove_dir_all(&current_dir).ok();
        assert_eq!(resolved, Some(workspace));
    }

    #[test]
    fn absolute_cwd_is_kept_without_canonicalizing() {
        let current_dir = env::current_dir().expect("current dir");
        let requested = current_dir.join(".");
        let resolved = resolve_level_b_cwd(Some(requested.clone()), current_dir)
            .expect("existing absolute directory");
        assert_eq!(resolved, requested);
        assert!(!resolved.to_string_lossy().contains(r"\\?\"));
    }

    #[test]
    fn missing_or_non_directory_cwd_is_rejected() {
        let current_dir = env::current_dir().expect("current dir");
        assert_eq!(
            resolve_level_b_cwd(
                Some(PathBuf::from("no-such-herdr-p4-workspace")),
                current_dir.clone()
            ),
            None
        );
        assert_eq!(
            resolve_level_b_cwd(Some(current_dir.join("Cargo.toml")), current_dir),
            None
        );
    }
}
