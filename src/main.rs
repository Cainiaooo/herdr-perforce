use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, ExitCode},
};

use herdr_perforce::p4::{P4Client, StdProcessTransport, run_level_b_read_only};
use serde_json::Value;

const HELP: &str = concat!(
    "herdr-p4 - compact Perforce review pane for Herdr\n\n",
    "Usage:\n",
    "  herdr-p4 --version\n",
    "  herdr-p4 --help\n",
    "  herdr-p4 level-b --read-only [--cwd <workspace-path>]\n",
    "  herdr-p4 pane [--cwd <workspace-path>]\n",
    "  herdr-p4 open-pane\n\n",
    "Level B is explicitly opt-in and only runs bounded info, changes, describe, opened,\n",
    "and where queries. It never runs a write command or retries through another config.\n\n",
    "The pane command is the Herdr terminal entrypoint. Submit remains available only\n",
    "through its preflight and explicitly confirmed overlay; there is no CLI submit command."
);

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
        Ok(Command::Pane { cwd }) => run_pane(cwd),
        Ok(Command::OpenPane) => open_herdr_pane(),
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
    Pane { cwd: Option<PathBuf> },
    OpenPane,
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
        [pane] if pane == "pane" => Ok(Command::Pane { cwd: None }),
        [pane, cwd_flag, cwd] if pane == "pane" && cwd_flag == "--cwd" => Ok(Command::Pane {
            cwd: Some(PathBuf::from(cwd)),
        }),
        [pane, ..] if pane == "pane" => Err("pane accepts only [--cwd <workspace-path>]"),
        [open] if open == "open-pane" => Ok(Command::OpenPane),
        [open, ..] if open == "open-pane" => Err("open-pane accepts no arguments"),
        _ => Err("unsupported arguments"),
    }
}

fn run_pane(requested: Option<PathBuf>) -> ExitCode {
    let Some(cwd) = resolve_pane_cwd(requested) else {
        eprintln!(
            "Herdr pane could not resolve a workspace cwd from --cwd or HERDR_PLUGIN_CONTEXT_JSON"
        );
        return ExitCode::from(66);
    };
    match herdr_perforce::tui::run_pane(cwd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Herdr pane failed: {error}");
            ExitCode::from(70)
        }
    }
}

fn resolve_pane_cwd(requested: Option<PathBuf>) -> Option<PathBuf> {
    let current_dir = env::current_dir().ok()?;
    if requested.is_some() {
        return resolve_level_b_cwd(requested, current_dir);
    }
    let plugin_root = env::var_os("HERDR_PLUGIN_ROOT").map(PathBuf::from);
    if let Ok(context_json) = env::var("HERDR_PLUGIN_CONTEXT_JSON") {
        if let Ok(context) = serde_json::from_str::<Value>(&context_json) {
            if let Some(path) = pane_cwd_from_context(&context, plugin_root.as_deref()) {
                return Some(path);
            }
        }
    }
    if is_plugin_directory(&current_dir, plugin_root.as_deref()) {
        return None;
    }
    fs::read_dir(&current_dir).ok()?;
    Some(current_dir)
}

fn pane_entrypoint() -> &'static str {
    if cfg!(windows) {
        "review-windows"
    } else {
        "review"
    }
}

fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    match path.to_str() {
        Some(raw) => PathBuf::from(raw.strip_prefix(r"\\?\").unwrap_or(raw)),
        None => path.to_path_buf(),
    }
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    let left = strip_verbatim_prefix(left);
    let right = strip_verbatim_prefix(right);
    if left == right {
        return true;
    }
    match (left.to_str(), right.to_str()) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

fn is_plugin_directory(path: &Path, plugin_root: Option<&Path>) -> bool {
    plugin_root.is_some_and(|root| paths_equal(path, root))
}

fn readable_absolute_dir(path: &Path) -> Option<PathBuf> {
    let path = strip_verbatim_prefix(path);
    (path.is_absolute() && fs::read_dir(&path).is_ok()).then_some(path)
}

fn context_directory(context: &Value, key: &str) -> Option<PathBuf> {
    let value = if key.starts_with('/') {
        context.pointer(key).and_then(Value::as_str)
    } else {
        context.get(key).and_then(Value::as_str)
    }?;
    readable_absolute_dir(Path::new(value))
}

fn pane_cwd_from_context(context: &Value, plugin_root: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = context_directory(context, "focused_pane_cwd") {
        if !is_plugin_directory(&path, plugin_root) {
            return Some(path);
        }
    }
    context_directory(context, "workspace_cwd")
        .or_else(|| context_directory(context, "/workspace/cwd"))
        .filter(|path| !is_plugin_directory(path, plugin_root))
}

fn herdr_open_pane_args(context: Option<&Value>, plugin_root: Option<&Path>) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("plugin"),
        OsString::from("pane"),
        OsString::from("open"),
        OsString::from("--plugin"),
        OsString::from("herdr.perforce"),
        OsString::from("--entrypoint"),
        OsString::from(pane_entrypoint()),
        OsString::from("--placement"),
        OsString::from("split"),
        OsString::from("--direction"),
        OsString::from("right"),
        OsString::from("--focus"),
    ];
    if let Some(context) = context {
        append_context_argument(&mut args, "--workspace", context, "workspace_id");
        append_context_argument(&mut args, "--target-pane", context, "focused_pane_id");
        if let Some(cwd) = pane_cwd_from_context(context, plugin_root) {
            args.push(OsString::from("--cwd"));
            args.push(cwd.into_os_string());
        }
    }
    args
}

fn open_herdr_pane() -> ExitCode {
    let executable = env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| OsString::from("herdr"));
    let context = env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .and_then(|value| serde_json::from_str::<Value>(&value).ok());
    let plugin_root = env::var_os("HERDR_PLUGIN_ROOT").map(PathBuf::from);
    let mut command = ProcessCommand::new(executable);
    command.args(herdr_open_pane_args(
        context.as_ref(),
        plugin_root.as_deref(),
    ));
    match command.status() {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => ExitCode::from(
            status
                .code()
                .and_then(|code| u8::try_from(code).ok())
                .filter(|code| *code != 0)
                .unwrap_or(70),
        ),
        Err(_) => {
            eprintln!("Could not invoke the Herdr binary from HERDR_BIN_PATH");
            ExitCode::from(69)
        }
    }
}

fn append_context_argument(args: &mut Vec<OsString>, flag: &str, context: &Value, key: &str) {
    if let Some(value) = context.get(key).and_then(Value::as_str) {
        args.push(OsString::from(flag));
        args.push(OsStr::new(value).to_os_string());
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
    fn pane_and_open_pane_are_explicit_non_submit_entrypoints() {
        assert_eq!(
            parse_command(strings(&["pane"])),
            Ok(Command::Pane { cwd: None })
        );
        assert_eq!(
            parse_command(strings(&["pane", "--cwd", "C:/Example Workspace"])),
            Ok(Command::Pane {
                cwd: Some(PathBuf::from("C:/Example Workspace"))
            })
        );
        assert_eq!(
            parse_command(strings(&["open-pane"])),
            Ok(Command::OpenPane)
        );
        assert!(parse_command(strings(&["submit", "42"])).is_err());
    }

    #[test]
    fn herdr_context_prefers_the_focused_pane_workspace() {
        let current_dir = env::current_dir().expect("current dir");
        let fallback = current_dir.parent().unwrap_or(&current_dir);
        let context = serde_json::json!({
            "focused_pane_cwd": current_dir,
            "workspace_cwd": fallback,
        });
        assert_eq!(
            pane_cwd_from_context(&context, Some(fallback)),
            Some(env::current_dir().expect("current dir"))
        );
    }

    #[test]
    fn focused_plugin_root_is_not_used_as_the_p4_workspace() {
        let current_dir = env::current_dir().expect("current dir");
        let workspace = current_dir.parent().unwrap_or(&current_dir);
        let context = serde_json::json!({
            "focused_pane_cwd": current_dir,
            "workspace_cwd": workspace,
        });
        assert_eq!(
            pane_cwd_from_context(&context, Some(&current_dir)),
            Some(workspace.to_path_buf())
        );
    }

    #[test]
    fn verbatim_plugin_root_prefix_is_ignored_when_matching_cwd() {
        let current_dir = env::current_dir().expect("current dir");
        let workspace = current_dir.parent().unwrap_or(&current_dir);
        let prefixed = PathBuf::from(format!(r"\\?\{}", current_dir.display()));
        let context = serde_json::json!({
            "focused_pane_cwd": current_dir,
            "workspace_cwd": workspace,
        });
        assert_eq!(
            pane_cwd_from_context(&context, Some(&prefixed)),
            Some(workspace.to_path_buf())
        );
    }

    #[test]
    fn open_pane_forwards_workspace_cwd_and_platform_entrypoint() {
        let current_dir = env::current_dir().expect("current dir");
        let plugin_root = current_dir.parent().unwrap_or(&current_dir);
        let context = serde_json::json!({
            "workspace_id": "ws-1",
            "focused_pane_id": "pane-1",
            "focused_pane_cwd": current_dir,
            "workspace_cwd": plugin_root,
        });
        let args = herdr_open_pane_args(Some(&context), Some(plugin_root));
        let args = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.contains(&pane_entrypoint().to_owned()));
        let cwd_index = args.iter().position(|arg| arg == "--cwd").expect("--cwd");
        assert_eq!(args[cwd_index + 1], current_dir.to_string_lossy());
        assert!(args.contains(&"--workspace".to_owned()));
        assert!(args.contains(&"ws-1".to_owned()));
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
