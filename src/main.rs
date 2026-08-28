use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, ExitCode, ExitStatus, Output},
};

use herdr_perforce::p4::{P4Client, StdProcessTransport, run_level_b_read_only};
use herdr_perforce::panel_restore::{
    HerdrPane, PanelOpenMode, RememberedWorkspace, load_navigator_share, load_panel_open_mode,
    load_remembered_workspaces, matching_workspace, opened_pane_id, pane_process_is_active,
    parse_pane_list, perforce_pane_candidates, preferred_perforce_pane, remember_workspace,
    restore_decision,
};
use herdr_perforce::tui::{navigation_resize_args_for_share, rightmost_pane_id};
use serde_json::Value;

const HELP: &str = concat!(
    "herdr-p4 - compact Perforce review pane for Herdr\n\n",
    "Usage:\n",
    "  herdr-p4 --version\n",
    "  herdr-p4 --help\n",
    "  herdr-p4 level-b --read-only [--cwd <workspace-path>]\n",
    "  herdr-p4 pane [--cwd <workspace-path>]\n",
    "  herdr-p4 viewer [--cwd <workspace-path>]\n",
    "  herdr-p4 open-pane\n",
    "  herdr-p4 restore-panes\n\n",
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
        Ok(Command::Viewer { cwd }) => run_viewer(cwd),
        Ok(Command::OpenPane) => open_herdr_pane(),
        Ok(Command::RestorePanes) => restore_herdr_panes(),
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
    Viewer { cwd: Option<PathBuf> },
    OpenPane,
    RestorePanes,
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
        [viewer] if viewer == "viewer" => Ok(Command::Viewer { cwd: None }),
        [viewer, cwd_flag, cwd] if viewer == "viewer" && cwd_flag == "--cwd" => {
            Ok(Command::Viewer {
                cwd: Some(PathBuf::from(cwd)),
            })
        }
        [viewer, ..] if viewer == "viewer" => Err("viewer accepts only [--cwd <workspace-path>]"),
        [open] if open == "open-pane" => Ok(Command::OpenPane),
        [open, ..] if open == "open-pane" => Err("open-pane accepts no arguments"),
        [restore] if restore == "restore-panes" => Ok(Command::RestorePanes),
        [restore, ..] if restore == "restore-panes" => Err("restore-panes accepts no arguments"),
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

fn run_viewer(requested: Option<PathBuf>) -> ExitCode {
    let Some(cwd) = resolve_pane_cwd(requested) else {
        eprintln!(
            "Herdr content pane could not resolve a workspace cwd from --cwd or HERDR_PLUGIN_CONTEXT_JSON"
        );
        return ExitCode::from(66);
    };
    match herdr_perforce::tui::run_content_pane(cwd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Herdr content pane failed: {error}");
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

fn workspace_cwd_from_context(context: &Value, plugin_root: Option<&Path>) -> Option<PathBuf> {
    context_directory(context, "workspace_cwd")
        .or_else(|| context_directory(context, "/workspace/cwd"))
        .filter(|path| !is_plugin_directory(path, plugin_root))
        .or_else(|| pane_cwd_from_context(context, plugin_root))
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
    ];
    if let Some(context) = context {
        if context
            .get("focused_pane_id")
            .and_then(Value::as_str)
            .is_some()
        {
            append_context_argument(&mut args, "--target-pane", context, "focused_pane_id");
        } else {
            append_context_argument(&mut args, "--workspace", context, "workspace_id");
        }
        if let Some(cwd) = pane_cwd_from_context(context, plugin_root) {
            args.push(OsString::from("--cwd"));
            args.push(cwd.into_os_string());
        }
    }
    args.push(OsString::from("--focus"));
    args
}

fn open_herdr_pane() -> ExitCode {
    let executable = env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| OsString::from("herdr"));
    let context = merge_missing_context(
        invocation_context_from_environment(),
        current_pane_context(&executable),
    );
    let plugin_root = env::var_os("HERDR_PLUGIN_ROOT").map(PathBuf::from);
    let mut command = ProcessCommand::new(&executable);
    let args = herdr_open_pane_args(context.as_ref(), plugin_root.as_deref());
    command.args(args);
    match command.output() {
        Ok(output) if output.status.success() => {
            resize_opened_navigation_pane(&executable, &output.stdout);
            remember_opened_pane(context.as_ref(), &output.stdout, plugin_root.as_deref());
            println!("Herdr Perforce pane opened");
            ExitCode::SUCCESS
        }
        Ok(output) => {
            eprintln!("Herdr refused to open the Perforce pane");
            exit_code_from_status(&output.status)
        }
        Err(_) => {
            eprintln!("Could not invoke the Herdr binary from HERDR_BIN_PATH");
            ExitCode::from(69)
        }
    }
}

fn remember_opened_pane(context: Option<&Value>, stdout: &[u8], plugin_root: Option<&Path>) {
    let config_dir = env::var_os("HERDR_PLUGIN_CONFIG_DIR").map(PathBuf::from);
    match load_panel_open_mode(config_dir.as_deref()) {
        Ok(PanelOpenMode::Manual) => return,
        Err(error) => {
            eprintln!("Perforce pane was opened but not remembered: {error}");
            return;
        }
        Ok(PanelOpenMode::Remembered) => {}
    }
    let Some(state_dir) = env::var_os("HERDR_PLUGIN_STATE_DIR").map(PathBuf::from) else {
        eprintln!("Perforce pane was opened but not remembered: HERDR_PLUGIN_STATE_DIR is missing");
        return;
    };
    let Some(context) = context else {
        eprintln!("Perforce pane was opened but not remembered: workspace context is missing");
        return;
    };
    let Some(cwd) = pane_cwd_from_context(context, plugin_root) else {
        eprintln!("Perforce pane was opened but not remembered: workspace cwd is unavailable");
        return;
    };
    let Some(workspace_cwd) = workspace_cwd_from_context(context, plugin_root) else {
        eprintln!(
            "Perforce pane was opened but not remembered: Herdr workspace cwd is unavailable"
        );
        return;
    };
    let response = serde_json::from_slice::<Value>(stdout).ok();
    let pane_id = response.as_ref().and_then(opened_pane_id);
    let workspace_id = context.get("workspace_id").and_then(Value::as_str);
    if let Err(error) = remember_workspace(&state_dir, &cwd, &workspace_cwd, workspace_id, pane_id)
    {
        eprintln!("Perforce pane was opened but not remembered: {error}");
    }
}

fn restore_herdr_panes() -> ExitCode {
    let config_dir = env::var_os("HERDR_PLUGIN_CONFIG_DIR").map(PathBuf::from);
    match load_panel_open_mode(config_dir.as_deref()) {
        Ok(PanelOpenMode::Manual) => {
            println!("Herdr Perforce pane restore: manual mode");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("Herdr Perforce pane restore disabled: {error}");
            return ExitCode::from(78);
        }
        Ok(PanelOpenMode::Remembered) => {}
    }

    let state_dir = env::var_os("HERDR_PLUGIN_STATE_DIR").map(PathBuf::from);
    let remembered = match load_remembered_workspaces(state_dir.as_deref()) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("Herdr Perforce pane restore disabled: {error}");
            return ExitCode::from(65);
        }
    };
    if remembered.is_empty() {
        println!("Herdr Perforce pane restore: no remembered workspaces");
        return ExitCode::SUCCESS;
    }

    let executable = env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| OsString::from("herdr"));
    let pane_args = [OsString::from("pane"), OsString::from("list")];
    let pane_response = match run_herdr_json(&executable, &pane_args) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("Herdr Perforce pane restore failed: {error}");
            return ExitCode::from(69);
        }
    };
    let panes = match parse_pane_list(&pane_response) {
        Ok(panes) => panes,
        Err(error) => {
            eprintln!("Herdr Perforce pane restore failed: {error}");
            return ExitCode::from(65);
        }
    };

    let mut restored = 0usize;
    let mut already_open = 0usize;
    let mut unavailable = 0usize;
    let mut failed = 0usize;
    let mut stale_closed = 0usize;
    let mut duplicates_closed = 0usize;
    for entry in &remembered {
        let Some(workspace) = matching_workspace(entry, &panes) else {
            unavailable += 1;
            continue;
        };
        let candidates = perforce_pane_candidates(entry, &workspace, &panes);
        let mut healthy_ids = Vec::new();
        let mut unknown_ids = Vec::new();
        let mut healthy_panes = Vec::new();
        for pane in &candidates {
            match inspect_perforce_pane(&executable, pane) {
                Ok(true) => {
                    healthy_ids.push(pane.id.clone());
                    healthy_panes.push(*pane);
                }
                Ok(false) => {}
                Err(_) => unknown_ids.push(pane.id.clone()),
            }
        }
        let decision = restore_decision(entry, &workspace, &panes, &healthy_ids, &unknown_ids);
        if !decision.healthy_nav_ids.is_empty() {
            let keep = keep_existing_navigation_pane(&executable, entry, &healthy_panes);
            let extra_healthy = healthy_panes
                .iter()
                .map(|pane| pane.id.clone())
                .filter(|id| id != &keep)
                .collect::<Vec<_>>();
            let leftover_cleanup =
                close_restored_panes(&executable, &decision.leftover_content_ids);
            stale_closed += leftover_cleanup.closed;
            failed += leftover_cleanup.failed;
            let corpse_cleanup = close_restored_panes(&executable, &decision.stale_nav_ids);
            stale_closed += corpse_cleanup.closed;
            failed += corpse_cleanup.failed;
            let duplicate_cleanup = close_restored_panes(&executable, &extra_healthy);
            duplicates_closed += duplicate_cleanup.closed;
            failed += duplicate_cleanup.failed;
            if let Some(state_dir) = state_dir.as_deref() {
                remember_workspace(
                    state_dir,
                    &entry.cwd,
                    &entry.workspace_cwd,
                    Some(&workspace.id),
                    Some(&keep),
                )
                .ok();
            }
            resize_navigation_pane_id(&executable, &keep);
            already_open += 1;
            continue;
        }
        if !decision.unknown_nav_ids.is_empty() {
            failed += 1;
            continue;
        }
        // Session restore leaves a labeled shell. Close that corpse first,
        // then open a real plugin pane from the remaining workspace pane.
        // Opening first and hoping pane close works is what stacked duplicates.
        let leftover_cleanup = close_restored_panes(&executable, &decision.leftover_content_ids);
        stale_closed += leftover_cleanup.closed;
        failed += leftover_cleanup.failed;
        if leftover_cleanup.failed > 0 {
            continue;
        }
        let corpse_cleanup = close_restored_panes(&executable, &decision.stale_nav_ids);
        stale_closed += corpse_cleanup.closed;
        if corpse_cleanup.failed > 0 {
            failed += corpse_cleanup.failed;
            continue;
        }
        let args =
            herdr_restore_pane_args(decision.open_target.as_deref(), &workspace.id, &entry.cwd);
        let output = match ProcessCommand::new(&executable).args(&args).output() {
            Ok(output) if output.status.success() => output,
            _ => {
                failed += 1;
                continue;
            }
        };
        resize_opened_navigation_pane(&executable, &output.stdout);
        restored += 1;
        if let (Some(state_dir), Ok(response)) = (
            state_dir.as_deref(),
            serde_json::from_slice::<Value>(&output.stdout),
        ) {
            let pane_id = opened_pane_id(&response);
            if remember_workspace(
                state_dir,
                &entry.cwd,
                &entry.workspace_cwd,
                Some(&workspace.id),
                pane_id,
            )
            .is_err()
            {
                failed += 1;
            }
        }
    }

    println!(
        "Herdr Perforce pane restore: restored={restored}, already-open={already_open}, stale-closed={stale_closed}, duplicates-closed={duplicates_closed}, unavailable={unavailable}, failed={failed}"
    );
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(70)
    }
}

fn keep_existing_navigation_pane(
    executable: &OsStr,
    entry: &RememberedWorkspace,
    active_panes: &[&HerdrPane],
) -> String {
    if active_panes.len() == 1 {
        return active_panes[0].id.clone();
    }
    if let Some(layout) = active_panes
        .first()
        .and_then(|pane| run_herdr_json(executable, &herdr_navigation_layout_args(&pane.id)).ok())
    {
        let ids = active_panes.iter().map(|pane| pane.id.as_str());
        if let Some(pane_id) = rightmost_pane_id(&layout, ids) {
            return pane_id.to_owned();
        }
    }
    preferred_perforce_pane(entry, active_panes)
        .map(|pane| pane.id.clone())
        .unwrap_or_else(|| active_panes[0].id.clone())
}

fn inspect_perforce_pane(executable: &OsStr, pane: &HerdrPane) -> Result<bool, &'static str> {
    let process_args = herdr_pane_process_info_args(&pane.id);
    let attempts = if pane.label.as_deref() == Some("Perforce") {
        5
    } else {
        1
    };
    let mut last = None;
    for attempt in 0..attempts {
        match run_herdr_json(executable, &process_args)
            .and_then(|response| pane_process_is_active(&response))
        {
            Ok(true) => return Ok(true),
            other => {
                last = Some(other);
                if attempt + 1 < attempts {
                    std::thread::sleep(std::time::Duration::from_millis(80));
                }
            }
        }
    }
    last.unwrap_or(Ok(false))
}

fn resize_opened_navigation_pane(executable: &OsStr, stdout: &[u8]) {
    let Some(pane_id) = serde_json::from_slice::<Value>(stdout)
        .ok()
        .as_ref()
        .and_then(opened_pane_id)
        .map(ToOwned::to_owned)
    else {
        return;
    };
    resize_navigation_pane_id(executable, &pane_id);
}

fn resize_navigation_pane_id(executable: &OsStr, pane_id: &str) {
    let share = load_navigator_share(
        env::var_os("HERDR_PLUGIN_STATE_DIR")
            .map(PathBuf::from)
            .as_deref(),
    );
    // Query actual geometry instead of assuming the opening ratio. This also
    // absorbs terminal chrome and any future Herdr default-ratio change.
    for attempt in 0..6 {
        let layout_args = herdr_navigation_layout_args(pane_id);
        if let Ok(layout) = run_herdr_json(executable, &layout_args) {
            let Some(resize_args) = navigation_resize_args_for_share(&layout, pane_id, share)
            else {
                return;
            };
            if ProcessCommand::new(executable)
                .args(&resize_args)
                .output()
                .is_ok_and(|output| herdr_output_succeeded(&output))
            {
                return;
            }
        }
        if attempt < 5 {
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
    }
    // A just-created plugin pane is a 50/50 split in Herdr 0.8.2. If layout
    // metadata never becomes available, retain the bounded legacy fallback
    // instead of leaving navigation at half the tab.
    let _ = ProcessCommand::new(executable)
        .args(herdr_navigation_resize_fallback_args(pane_id, share))
        .output();
}

fn herdr_navigation_layout_args(pane_id: &str) -> Vec<OsString> {
    vec![
        OsString::from("pane"),
        OsString::from("layout"),
        OsString::from("--pane"),
        OsString::from(pane_id),
    ]
}

fn herdr_navigation_resize_fallback_args(pane_id: &str, share: f64) -> Vec<OsString> {
    let amount = (0.5 - share).abs().max(0.01);
    let direction = if share <= 0.5 { "right" } else { "left" };
    vec![
        OsString::from("pane"),
        OsString::from("resize"),
        OsString::from("--direction"),
        OsString::from(direction),
        OsString::from("--amount"),
        OsString::from(format!("{amount:.6}")),
        OsString::from("--pane"),
        OsString::from(pane_id),
    ]
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct StalePaneCleanup {
    closed: usize,
    failed: usize,
}

fn close_restored_panes(executable: &OsStr, pane_ids: &[String]) -> StalePaneCleanup {
    let mut result = StalePaneCleanup::default();
    for pane_id in pane_ids {
        if close_restored_pane(executable, pane_id) {
            result.closed += 1;
        } else {
            result.failed += 1;
        }
    }
    result
}

fn close_restored_pane(executable: &OsStr, pane_id: &str) -> bool {
    // Session-restored plugin slots often keep plugin ownership after the
    // process dies. Prefer plugin pane close, then fall back to a plain close.
    let plugin_closed = ProcessCommand::new(executable)
        .args(herdr_plugin_pane_close_args(pane_id))
        .output()
        .is_ok_and(|output| herdr_output_succeeded(&output));
    if plugin_closed {
        return true;
    }
    ProcessCommand::new(executable)
        .args(herdr_pane_close_args(pane_id))
        .output()
        .is_ok_and(|output| herdr_output_succeeded(&output))
}

fn herdr_pane_process_info_args(pane_id: &str) -> [OsString; 4] {
    [
        OsString::from("pane"),
        OsString::from("process-info"),
        OsString::from("--pane"),
        OsString::from(pane_id),
    ]
}

fn herdr_pane_close_args(pane_id: &str) -> [OsString; 3] {
    [
        OsString::from("pane"),
        OsString::from("close"),
        OsString::from(pane_id),
    ]
}

fn herdr_plugin_pane_close_args(pane_id: &str) -> [OsString; 4] {
    [
        OsString::from("plugin"),
        OsString::from("pane"),
        OsString::from("close"),
        OsString::from(pane_id),
    ]
}

fn herdr_restore_pane_args(
    target_pane: Option<&str>,
    workspace_id: &str,
    cwd: &Path,
) -> Vec<OsString> {
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
    ];
    if let Some(target_pane) = target_pane {
        args.push(OsString::from("--target-pane"));
        args.push(OsString::from(target_pane));
    } else {
        args.push(OsString::from("--workspace"));
        args.push(OsString::from(workspace_id));
    }
    args.push(OsString::from("--cwd"));
    args.push(cwd.as_os_str().to_os_string());
    args.push(OsString::from("--no-focus"));
    args
}

fn run_herdr_json(executable: &OsStr, args: &[OsString]) -> Result<Value, &'static str> {
    let output = ProcessCommand::new(executable)
        .args(args)
        .output()
        .map_err(|_| "Herdr binary could not be invoked")?;
    if !herdr_output_succeeded(&output) {
        return Err("Herdr command was rejected");
    }
    serde_json::from_slice(&output.stdout).map_err(|_| "Herdr response is invalid")
}

fn herdr_output_succeeded(output: &Output) -> bool {
    output.status.success()
        && serde_json::from_slice::<Value>(&output.stdout)
            .map(|response| response.get("error").is_none())
            .unwrap_or_else(|_| output.stdout.is_empty())
}

fn exit_code_from_status(status: &ExitStatus) -> ExitCode {
    ExitCode::from(
        status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .filter(|code| *code != 0)
            .unwrap_or(70),
    )
}

fn current_pane_context(executable: &OsStr) -> Option<Value> {
    let output = ProcessCommand::new(executable)
        .args(["pane", "current"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let response = serde_json::from_slice::<Value>(&output.stdout).ok()?;
    pane_context_from_current_response(&response)
}

fn pane_context_from_current_response(response: &Value) -> Option<Value> {
    let pane = response.pointer("/result/pane")?;
    let mut context = serde_json::Map::new();
    for (target, source) in [
        ("workspace_id", "workspace_id"),
        ("focused_pane_id", "pane_id"),
        ("focused_pane_cwd", "cwd"),
    ] {
        if let Some(value) = pane.get(source).and_then(Value::as_str) {
            context.insert(target.to_owned(), Value::String(value.to_owned()));
        }
    }
    (!context.is_empty()).then_some(Value::Object(context))
}

fn merge_missing_context(primary: Option<Value>, fallback: Option<Value>) -> Option<Value> {
    let mut primary = match primary {
        Some(Value::Object(context)) => context,
        _ => serde_json::Map::new(),
    };
    if let Some(Value::Object(fallback)) = fallback {
        let conflict = workspace_ids_conflict(&primary, &fallback);
        for (key, value) in fallback {
            if conflict && is_pane_scoped_context_key(&key) {
                continue;
            }
            primary.entry(key).or_insert(value);
        }
    }
    (!primary.is_empty()).then_some(Value::Object(primary))
}

fn workspace_ids_conflict(
    primary: &serde_json::Map<String, Value>,
    fallback: &serde_json::Map<String, Value>,
) -> bool {
    match (
        primary.get("workspace_id").and_then(Value::as_str),
        fallback.get("workspace_id").and_then(Value::as_str),
    ) {
        (Some(left), Some(right)) => left != right,
        _ => false,
    }
}

fn is_pane_scoped_context_key(key: &str) -> bool {
    matches!(key, "focused_pane_id" | "focused_pane_cwd")
}

fn invocation_context_from_environment() -> Option<Value> {
    let parsed = env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .and_then(|value| serde_json::from_str::<Value>(&value).ok());
    let mut context = match parsed {
        Some(Value::Object(context)) => context,
        _ => serde_json::Map::new(),
    };

    insert_context_fallback(
        &mut context,
        "workspace_id",
        first_environment_value(&["HERDR_WORKSPACE_ID", "HERDR_ACTIVE_WORKSPACE_ID"]),
    );
    insert_context_fallback(
        &mut context,
        "focused_pane_id",
        first_environment_value(&["HERDR_PANE_ID", "HERDR_ACTIVE_PANE_ID"]),
    );
    insert_context_fallback(
        &mut context,
        "focused_pane_cwd",
        first_environment_value(&["HERDR_ACTIVE_PANE_CWD"]),
    );

    (!context.is_empty()).then_some(Value::Object(context))
}

fn first_environment_value(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| env::var(name).ok().filter(|value| !value.trim().is_empty()))
}

fn insert_context_fallback(
    context: &mut serde_json::Map<String, Value>,
    key: &str,
    fallback: Option<String>,
) {
    if !context.get(key).is_some_and(Value::is_string) {
        if let Some(value) = fallback {
            context.insert(key.to_owned(), Value::String(value));
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
            parse_command(strings(&["viewer", "--cwd", "C:/Example Workspace"])),
            Ok(Command::Viewer {
                cwd: Some(PathBuf::from("C:/Example Workspace"))
            })
        );
        assert_eq!(
            parse_command(strings(&["open-pane"])),
            Ok(Command::OpenPane)
        );
        assert_eq!(
            parse_command(strings(&["restore-panes"])),
            Ok(Command::RestorePanes)
        );
        assert!(parse_command(strings(&["submit", "42"])).is_err());
    }

    #[test]
    fn opened_navigation_pane_layout_is_queried_by_id() {
        assert_eq!(
            herdr_navigation_layout_args("workspace:p2"),
            strings(&["pane", "layout", "--pane", "workspace:p2",])
        );
        assert_eq!(
            herdr_navigation_resize_fallback_args("workspace:p2", 0.2),
            strings(&[
                "pane",
                "resize",
                "--direction",
                "right",
                "--amount",
                "0.300000",
                "--pane",
                "workspace:p2",
            ])
        );
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
        assert!(!args.iter().any(|arg| arg == "--workspace"));
        let target_index = args
            .iter()
            .position(|arg| arg == "--target-pane")
            .expect("--target-pane");
        assert_eq!(args[target_index + 1], "pane-1");
        assert_eq!(args.last().map(String::as_str), Some("--focus"));
    }

    #[test]
    fn open_pane_uses_workspace_only_when_no_target_pane_is_available() {
        let current_dir = env::current_dir().expect("current dir");
        let context = serde_json::json!({
            "workspace_id": "ws-1",
            "workspace_cwd": current_dir,
        });
        let args = herdr_open_pane_args(Some(&context), None)
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let workspace_index = args
            .iter()
            .position(|arg| arg == "--workspace")
            .expect("--workspace");
        assert_eq!(args[workspace_index + 1], "ws-1");
        assert!(!args.iter().any(|arg| arg == "--target-pane"));
    }

    #[test]
    fn startup_restore_targets_a_pane_without_repeating_workspace() {
        let cwd = Path::new(r"C:\ExampleWorkspace");
        let args = herdr_restore_pane_args(Some("w1:p1"), "w1", cwd)
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(!args.iter().any(|arg| arg == "--workspace"));
        assert_eq!(
            args.windows(2)
                .find(|pair| pair[0] == "--target-pane")
                .map(|pair| pair[1].as_str()),
            Some("w1:p1")
        );
        assert_eq!(args.last().map(String::as_str), Some("--no-focus"));
    }

    #[test]
    fn startup_restore_uses_workspace_when_no_target_pane_remains() {
        let cwd = Path::new(r"C:\ExampleWorkspace");
        let args = herdr_restore_pane_args(None, "w1", cwd)
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(!args.iter().any(|arg| arg == "--target-pane"));
        assert_eq!(
            args.windows(2)
                .find(|pair| pair[0] == "--workspace")
                .map(|pair| pair[1].as_str()),
            Some("w1")
        );
    }

    #[test]
    fn stale_session_pane_cleanup_uses_plugin_close_then_plain_close() {
        let process_args = herdr_pane_process_info_args("w1:p7")
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let plugin_close = herdr_plugin_pane_close_args("w1:p7")
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let close_args = herdr_pane_close_args("w1:p7")
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(process_args, ["pane", "process-info", "--pane", "w1:p7"]);
        assert_eq!(plugin_close, ["plugin", "pane", "close", "w1:p7"]);
        assert_eq!(close_args, ["pane", "close", "w1:p7"]);
    }

    #[test]
    fn pane_current_from_another_workspace_is_not_merged() {
        let primary = serde_json::json!({
            "workspace_id": "ws-1",
            "workspace_cwd": "E:/Project/NeonGame"
        });
        let fallback = serde_json::json!({
            "workspace_id": "ws-2",
            "focused_pane_id": "ws-2:p1",
            "focused_pane_cwd": "E:/Other"
        });
        let context = merge_missing_context(Some(primary), Some(fallback)).expect("merged");
        assert_eq!(context["workspace_id"], "ws-1");
        assert_eq!(context["workspace_cwd"], "E:/Project/NeonGame");
        assert!(context.get("focused_pane_id").is_none());
        assert!(context.get("focused_pane_cwd").is_none());
    }

    #[test]
    fn pane_current_from_the_same_workspace_fills_missing_pane_fields() {
        let primary = serde_json::json!({ "workspace_id": "w1" });
        let fallback = serde_json::json!({
            "workspace_id": "w1",
            "focused_pane_id": "w1:p1",
            "focused_pane_cwd": "E:/Project/NeonGame"
        });
        let context = merge_missing_context(Some(primary), Some(fallback)).expect("merged");
        assert_eq!(context["focused_pane_id"], "w1:p1");
        assert_eq!(context["focused_pane_cwd"], "E:/Project/NeonGame");
    }

    #[test]
    fn current_pane_response_supplies_missing_action_context() {
        let response = serde_json::json!({
            "result": {
                "pane": {
                    "workspace_id": "w1",
                    "pane_id": "w1:p1",
                    "cwd": "E:/Project/NeonGame"
                }
            }
        });
        let fallback = pane_context_from_current_response(&response);
        let context = merge_missing_context(None, fallback).expect("pane context");
        assert_eq!(context["workspace_id"], "w1");
        assert_eq!(context["focused_pane_id"], "w1:p1");
        assert_eq!(context["focused_pane_cwd"], "E:/Project/NeonGame");
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
