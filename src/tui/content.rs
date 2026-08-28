//! Standalone File / Diff / Changelist content pane.
//!
//! The navigation pane writes a small request into a private per-user scratch
//! file. The content pane polls that file, so later selections update in place
//! without putting workspace paths through a shell command line.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    domain::{
        ChangedFile, Changelist, ChangelistId, FileAction, FileDiffKind, PreviewContent,
        PreviewTruncation, build_file_diff,
    },
    p4::{
        P4Client, P4Query, StdProcessTransport, changed_files_from_opened,
        changelist_from_describe, load_workspace_diff, read_workspace_preview,
    },
    panel_restore::{self, strip_verbatim_prefix},
};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use serde_json::{Value, json};

use super::{
    diff::{self, DiffToolbarAction, DiffViewState},
    syntax, wrap,
};

pub const CONTROL_ENV: &str = "HERDR_P4_CONTENT_CONTROL";
const CONTENT_SOURCE: &str = "herdr-perforce-content";
const CONTROL_TOKEN: &str = "herdr-p4-content-control";
const POLL: Duration = Duration::from_millis(250);
const HEARTBEAT: Duration = Duration::from_secs(5);
const NAVIGATION_SHARE: f64 = 0.2;
const LAYOUT_EPSILON: i64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaneRect {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
}

impl PaneRect {
    fn right(self) -> i64 {
        self.x.saturating_add(self.width)
    }

    fn bottom(self) -> i64 {
        self.y.saturating_add(self.height)
    }

    fn vertically_overlaps(self, other: Self) -> bool {
        self.y < other.bottom() && other.y < self.bottom()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ContentRequest {
    File {
        path: PathBuf,
    },
    Diff {
        change: ChangelistId,
        path: PathBuf,
        action: Option<FileAction>,
        fold_context: usize,
    },
    Changelist {
        change: ChangelistId,
    },
}

impl ContentRequest {
    fn to_json(&self) -> Result<String, String> {
        let value = match self {
            Self::File { path } => json!({
                "version": 1,
                "kind": "file",
                "path": path_to_text(path)?,
            }),
            Self::Diff {
                change,
                path,
                action,
                fold_context,
            } => {
                let mut value = json!({
                    "version": 1,
                    "kind": "diff",
                    "change": change.as_p4_arg(),
                    "path": path_to_text(path)?,
                    "diff_fold_context": fold_context,
                });
                if let Some(action) = action {
                    value["action"] = json!(action.canonical_name());
                }
                value
            }
            Self::Changelist { change } => json!({
                "version": 1,
                "kind": "changelist",
                "change": change.as_p4_arg(),
            }),
        };
        serde_json::to_string(&value)
            .map_err(|error| format!("could not encode content request: {error}"))
    }

    fn from_json(raw: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(raw)
            .map_err(|error| format!("content request is invalid: {error}"))?;
        if value.get("version").and_then(Value::as_u64) != Some(1) {
            return Err("content request uses an unsupported version".to_owned());
        }
        let change = || {
            value
                .get("change")
                .and_then(Value::as_str)
                .and_then(parse_change)
                .ok_or_else(|| "content request has an invalid changelist".to_owned())
        };
        let path = || {
            value
                .get("path")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .ok_or_else(|| "content request has an invalid path".to_owned())
        };
        match value.get("kind").and_then(Value::as_str) {
            Some("file") => Ok(Self::File { path: path()? }),
            Some("diff") => Ok(Self::Diff {
                change: change()?,
                path: path()?,
                action: value
                    .get("action")
                    .and_then(Value::as_str)
                    .map(FileAction::from_p4),
                fold_context: parse_request_fold_context(&value),
            }),
            Some("changelist") => Ok(Self::Changelist { change: change()? }),
            _ => Err("content request has an unsupported kind".to_owned()),
        }
    }
}

fn parse_change(value: &str) -> Option<ChangelistId> {
    if value.eq_ignore_ascii_case("default") {
        Some(ChangelistId::Default)
    } else {
        value.parse().ok().map(ChangelistId::Numbered)
    }
}

fn path_to_text(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| "content pane cannot represent a non-Unicode workspace path".to_owned())
}

fn filename(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

fn scratch_dir() -> PathBuf {
    let directory = env::temp_dir().join("herdr-perforce-scratch");
    let _ = fs::create_dir_all(&directory);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&directory, fs::Permissions::from_mode(0o700));
    }
    directory
}

fn fresh_control_path() -> PathBuf {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    scratch_dir().join(format!(
        "content-{}-{stamp:x}-{sequence:x}.json",
        std::process::id()
    ))
}

fn write_control(path: &Path, request: &ContentRequest) -> Result<(), String> {
    let expected_parent = scratch_dir();
    if path.parent() != Some(expected_parent.as_path()) {
        return Err("content control path is outside the private scratch directory".to_owned());
    }
    if fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("content control path is a symbolic link".to_owned());
    }
    fs::write(path, request.to_json()?)
        .map_err(|error| format!("could not update content pane: {error}"))
}

#[derive(Debug)]
pub struct ContentPaneClient {
    cwd: PathBuf,
    navigation_pane: Option<String>,
    content_pane: Option<String>,
    control: Option<PathBuf>,
}

impl ContentPaneClient {
    pub fn new(cwd: PathBuf) -> Self {
        let navigation_pane = env::var("HERDR_PANE_ID")
            .ok()
            .or_else(|| env::var("HERDR_ACTIVE_PANE_ID").ok())
            .filter(|value| !value.trim().is_empty());
        Self {
            cwd,
            navigation_pane,
            content_pane: None,
            control: None,
        }
    }

    pub fn show_file(&mut self, path: PathBuf) -> Result<String, String> {
        let name = filename(&path);
        self.open(ContentRequest::File { path })?;
        Ok(format!("Opened file content: {name}"))
    }

    pub fn show_diff(
        &mut self,
        change: ChangelistId,
        path: PathBuf,
        action: Option<FileAction>,
    ) -> Result<String, String> {
        let name = filename(&path);
        self.open(ContentRequest::Diff {
            change,
            path,
            action,
            fold_context: diff_fold_context(),
        })?;
        Ok(format!("Opened CL {change} diff: {name}"))
    }

    pub fn show_changelist(&mut self, change: ChangelistId) -> Result<String, String> {
        self.open(ContentRequest::Changelist { change })?;
        Ok(format!("Opened CL {change} files"))
    }

    #[cfg(test)]
    pub fn disable_host_for_test(&mut self) {
        self.navigation_pane = None;
    }

    fn open(&mut self, request: ContentRequest) -> Result<(), String> {
        let navigation = self.navigation_pane.clone().ok_or_else(|| {
            "content pane needs HERDR_PANE_ID; reopen Perforce from a Herdr pane".to_owned()
        })?;

        // A pane already owned by this navigation process is authoritative.
        // Updating content must never replace it merely because process-info
        // is briefly stale while the viewer is starting.
        if let (Some(pane), Some(control)) = (&self.content_pane, &self.control) {
            if pane_is_in_navigation_tab(pane, &navigation)? {
                write_control(control, &request)?;
                ensure_content_layout(&navigation, pane)?;
                focus_left_of(&navigation);
                return Ok(());
            }
            self.content_pane = None;
            self.control = None;
        }

        // Reattach after a navigation process restart. If the viewer process
        // stopped but its pane still exists, restart it in that exact pane.
        if let Some((pane, control)) = discover_content_pane(&navigation) {
            let control = control.ok_or_else(|| {
                "the existing Perforce content pane has no update channel; close it and retry"
                    .to_owned()
            })?;
            write_control(&control, &request)?;
            if !content_process_is_active(&pane) {
                start_viewer_in_pane(&pane)?;
            }
            ensure_content_layout(&navigation, &pane)?;
            focus_left_of(&navigation);
            self.content_pane = Some(pane);
            self.control = Some(control);
            return Ok(());
        }

        let control = fresh_control_path();
        write_control(&control, &request)?;
        match spawn_content_pane(&navigation, &self.cwd, &control) {
            Ok(pane) => {
                self.content_pane = Some(pane);
                self.control = Some(control);
                Ok(())
            }
            Err(error) => {
                let _ = fs::remove_file(control);
                Err(error)
            }
        }
    }
}

fn pane_is_in_navigation_tab(pane_id: &str, navigation_pane: &str) -> Result<bool, String> {
    let response = run_herdr_json(&[OsString::from("pane"), OsString::from("list")])?;
    let panes = response
        .pointer("/result/panes")
        .and_then(Value::as_array)
        .ok_or_else(|| "Herdr pane list did not include panes".to_owned())?;
    let navigation_tab = panes
        .iter()
        .find(|pane| pane.get("pane_id").and_then(Value::as_str) == Some(navigation_pane))
        .and_then(|pane| pane.get("tab_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| "the Perforce navigation pane is no longer in its tab".to_owned())?;
    Ok(panes.iter().any(|pane| {
        pane.get("pane_id").and_then(Value::as_str) == Some(pane_id)
            && pane.get("tab_id").and_then(Value::as_str) == Some(navigation_tab)
    }))
}

fn discover_content_pane(navigation_pane: &str) -> Option<(String, Option<PathBuf>)> {
    let response = run_herdr_json(&[OsString::from("pane"), OsString::from("list")]).ok()?;
    discover_content_pane_in(&response, navigation_pane)
}

fn discover_content_pane_in(
    response: &Value,
    navigation_pane: &str,
) -> Option<(String, Option<PathBuf>)> {
    let panes = response.pointer("/result/panes")?.as_array()?;
    let tab = panes
        .iter()
        .find(|pane| pane.get("pane_id").and_then(Value::as_str) == Some(navigation_pane))?
        .get("tab_id")?
        .as_str()?;
    panes
        .iter()
        .filter(|pane| pane.get("tab_id").and_then(Value::as_str) == Some(tab))
        .find_map(|pane| {
            let pane_id = pane.get("pane_id")?.as_str()?;
            if pane_id == navigation_pane {
                return None;
            }
            let tokens = pane.get("tokens").and_then(Value::as_object);
            let label = pane
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let recognized = tokens.is_some_and(|tokens| tokens.contains_key(CONTENT_SOURCE))
                || label == "Perforce Content"
                || label.ends_with(" · Perforce");
            if !recognized {
                return None;
            }
            let control = tokens
                .and_then(|tokens| tokens.get(CONTROL_TOKEN))
                .and_then(Value::as_str)
                .and_then(control_path_from_token);
            Some((pane_id.to_owned(), control))
        })
}

fn control_path_from_token(token: &str) -> Option<PathBuf> {
    let token_path = Path::new(token);
    let name = token_path.file_name()?.to_str()?;
    (token_path.components().count() == 1
        && name.starts_with("content-")
        && name.ends_with(".json"))
    .then(|| scratch_dir().join(name))
}

fn content_process_is_active(pane_id: &str) -> bool {
    run_herdr_json(&[
        OsString::from("pane"),
        OsString::from("process-info"),
        OsString::from("--pane"),
        OsString::from(pane_id),
    ])
    .map(|response| viewer_process_is_active(&response))
    .unwrap_or(false)
}

fn viewer_process_is_active(response: &Value) -> bool {
    response
        .pointer("/result/process_info/foreground_processes")
        .and_then(Value::as_array)
        .is_some_and(|processes| processes.iter().any(is_viewer_process))
}

fn is_viewer_process(process: &Value) -> bool {
    let executable_is_viewer = process
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| process.get("argv0").and_then(Value::as_str))
        .and_then(|value| Path::new(value).file_name())
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            name.eq_ignore_ascii_case("herdr-p4") || name.eq_ignore_ascii_case("herdr-p4.exe")
        });
    executable_is_viewer
        && process
            .get("argv")
            .and_then(Value::as_array)
            .is_some_and(|arguments| {
                arguments
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|argument| argument == "viewer")
            })
}

fn focus_left_of(navigation_pane: &str) {
    let _ = run_herdr_status(&[
        OsString::from("pane"),
        OsString::from("focus"),
        OsString::from("--direction"),
        OsString::from("left"),
        OsString::from("--pane"),
        OsString::from(navigation_pane),
    ]);
}

fn spawn_content_pane(navigation_pane: &str, cwd: &Path, control: &Path) -> Result<String, String> {
    let layout = run_herdr_json(&[
        OsString::from("pane"),
        OsString::from("layout"),
        OsString::from("--pane"),
        OsString::from(navigation_pane),
    ])?;
    let left_neighbor = horizontal_neighbor(&layout, navigation_pane, true);

    let target = left_neighbor
        .clone()
        .unwrap_or_else(|| navigation_pane.to_owned());
    let executable_dir = env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .ok_or_else(|| "content pane could not resolve the plugin binary directory".to_owned())?;
    let executable_dir = strip_verbatim_prefix(&executable_dir);
    let inherited_path = env::var_os("PATH").unwrap_or_default();
    let mut launch_path = OsString::from(executable_dir.as_os_str());
    launch_path.push(if cfg!(windows) { ";" } else { ":" });
    launch_path.push(inherited_path);

    let args = split_args(
        &target,
        cwd,
        control,
        &launch_path,
        env::var_os("HERDR_PLUGIN_CONFIG_DIR").as_deref(),
    );
    let response = run_herdr_json(&args)?;
    let pane = pane_id_from_response(&response)
        .ok_or_else(|| "Herdr opened a content split without returning its pane id".to_owned())?;

    if left_neighbor.is_none()
        && !run_herdr_status(&[
            OsString::from("pane"),
            OsString::from("swap"),
            OsString::from("--source-pane"),
            OsString::from(&pane),
            OsString::from("--target-pane"),
            OsString::from(navigation_pane),
        ])
    {
        let _ = close_pane(&pane);
        return Err("content pane opened but could not be placed left of navigation".to_owned());
    }

    if let Err(error) = start_viewer_in_pane(&pane) {
        let _ = close_pane(&pane);
        return Err(error);
    }
    let _ = run_herdr_status(&[
        OsString::from("pane"),
        OsString::from("rename"),
        OsString::from(&pane),
        OsString::from("Perforce Content"),
    ]);
    if let Err(error) = ensure_content_layout(navigation_pane, &pane) {
        let _ = close_pane(&pane);
        return Err(error);
    }
    Ok(pane)
}

fn start_viewer_in_pane(pane_id: &str) -> Result<(), String> {
    run_herdr_status(&[
        OsString::from("pane"),
        OsString::from("run"),
        OsString::from(pane_id),
        OsString::from("herdr-p4"),
        OsString::from("viewer"),
    ])
    .then_some(())
    .ok_or_else(|| "content pane exists but the viewer process did not start".to_owned())
}

fn split_args(
    target: &str,
    cwd: &Path,
    control: &Path,
    launch_path: &OsStr,
    config_dir: Option<&OsStr>,
) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("pane"),
        OsString::from("split"),
        OsString::from("--pane"),
        OsString::from(target),
        OsString::from("--direction"),
        OsString::from("right"),
        OsString::from("--ratio"),
        OsString::from("0.5"),
        OsString::from("--cwd"),
        cwd.as_os_str().to_os_string(),
        OsString::from("--env"),
        env_assignment(CONTROL_ENV, control.as_os_str()),
        OsString::from("--env"),
        env_assignment("PATH", launch_path),
    ];
    if let Some(config_dir) = config_dir {
        args.push(OsString::from("--env"));
        args.push(env_assignment("HERDR_PLUGIN_CONFIG_DIR", config_dir));
    }
    args.push(OsString::from("--focus"));
    args
}

fn ensure_content_layout(navigation_pane: &str, content_pane: &str) -> Result<(), String> {
    let mut layout = pane_layout(navigation_pane)?;
    if directly_left_of(&layout, navigation_pane, content_pane) {
        // Already correct.
    } else if directly_left_of(&layout, content_pane, navigation_pane) {
        if !run_herdr_status(&[
            OsString::from("pane"),
            OsString::from("swap"),
            OsString::from("--source-pane"),
            OsString::from(content_pane),
            OsString::from("--target-pane"),
            OsString::from(navigation_pane),
        ]) {
            return Err(
                "content pane is right of navigation and could not be moved left".to_owned(),
            );
        }
        layout = pane_layout(navigation_pane)?;
    }

    if !directly_left_of(&layout, navigation_pane, content_pane)
        || horizontal_neighbor(&layout, navigation_pane, false).is_some()
    {
        return Err(
            "content pane layout is invalid; navigation must stay rightmost with content directly left"
                .to_owned(),
        );
    }

    if let Some(args) = navigation_resize_args_for_layout(&layout, navigation_pane) {
        if !run_herdr_status(&args) {
            return Err("navigation pane could not be resized to its narrow share".to_owned());
        }
        layout = pane_layout(navigation_pane)?;
        if !directly_left_of(&layout, navigation_pane, content_pane)
            || horizontal_neighbor(&layout, navigation_pane, false).is_some()
        {
            return Err("content pane moved out of place while resizing navigation".to_owned());
        }
    }
    Ok(())
}

fn pane_layout(pane_id: &str) -> Result<Value, String> {
    run_herdr_json(&[
        OsString::from("pane"),
        OsString::from("layout"),
        OsString::from("--pane"),
        OsString::from(pane_id),
    ])
}

fn directly_left_of(layout: &Value, right_pane: &str, left_pane: &str) -> bool {
    let Some(right) = pane_rect(layout, right_pane) else {
        return false;
    };
    let Some(left) = pane_rect(layout, left_pane) else {
        return false;
    };
    left.vertically_overlaps(right) && (left.right() - right.x).abs() <= LAYOUT_EPSILON
}

fn horizontal_neighbor(layout: &Value, pane_id: &str, left: bool) -> Option<String> {
    let me = pane_rect(layout, pane_id)?;
    layout
        .pointer("/result/layout/panes")?
        .as_array()?
        .iter()
        .filter_map(|pane| {
            let candidate_id = pane.get("pane_id")?.as_str()?;
            if candidate_id == pane_id {
                return None;
            }
            let rect = rect_from(pane.get("rect")?)?;
            if !rect.vertically_overlaps(me) {
                return None;
            }
            let distance = if left {
                me.x.saturating_sub(rect.right())
            } else {
                rect.x.saturating_sub(me.right())
            };
            let is_on_side = if left {
                rect.right() <= me.x.saturating_add(LAYOUT_EPSILON)
            } else {
                rect.x.saturating_add(LAYOUT_EPSILON) >= me.right()
            };
            is_on_side.then(|| (distance.abs(), candidate_id.to_owned()))
        })
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, pane_id)| pane_id)
}

fn pane_rect(layout: &Value, pane_id: &str) -> Option<PaneRect> {
    layout
        .pointer("/result/layout/panes")?
        .as_array()?
        .iter()
        .find(|pane| pane.get("pane_id").and_then(Value::as_str) == Some(pane_id))
        .and_then(|pane| pane.get("rect"))
        .and_then(rect_from)
}

fn rect_from(value: &Value) -> Option<PaneRect> {
    Some(PaneRect {
        x: value.get("x")?.as_i64()?,
        y: value.get("y")?.as_i64()?,
        width: value.get("width")?.as_i64()?,
        height: value.get("height")?.as_i64()?,
    })
}

/// Calculate the exact resize needed to keep a right-docked navigation pane
/// at 20% of its owning horizontal split. `None` means it is already at the
/// target or the layout does not expose a matching divider.
pub fn navigation_resize_args_for_layout(
    layout: &Value,
    navigation_pane: &str,
) -> Option<Vec<OsString>> {
    let navigation = pane_rect(layout, navigation_pane)?;
    let (_, split_width) = layout
        .pointer("/result/layout/splits")?
        .as_array()?
        .iter()
        .filter(|split| split.get("direction").and_then(Value::as_str) == Some("right"))
        .filter_map(|split| {
            let rect = rect_from(split.get("rect")?)?;
            let ratio = split.get("ratio")?.as_f64()?;
            let divider = rect.x + (rect.width as f64 * ratio).round() as i64;
            ((divider - navigation.x).abs() <= LAYOUT_EPSILON && rect.width > 0)
                .then_some((rect.width, rect.width))
        })
        .min_by_key(|(width, _)| *width)?;
    let target_width = (split_width as f64 * NAVIGATION_SHARE).round() as i64;
    let width_delta = target_width.saturating_sub(navigation.width);
    let amount = width_delta.unsigned_abs() as f64 / split_width as f64;
    if amount < 0.005 {
        return None;
    }
    let direction = if width_delta < 0 { "right" } else { "left" };
    Some(vec![
        OsString::from("pane"),
        OsString::from("resize"),
        OsString::from("--direction"),
        OsString::from(direction),
        OsString::from("--amount"),
        OsString::from(format!("{amount:.6}")),
        OsString::from("--pane"),
        OsString::from(navigation_pane),
    ])
}

fn env_assignment(name: &str, value: &OsStr) -> OsString {
    let mut assignment = OsString::from(name);
    assignment.push("=");
    assignment.push(value);
    assignment
}

fn close_pane(pane_id: &str) -> bool {
    run_herdr_status(&[
        OsString::from("pane"),
        OsString::from("close"),
        OsString::from(pane_id),
    ])
}

fn run_herdr_json(args: &[OsString]) -> Result<Value, String> {
    let executable = env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| OsString::from("herdr"));
    let output = Command::new(executable)
        .args(args)
        .output()
        .map_err(|error| format!("could not invoke Herdr: {error}"))?;
    if !output.status.success() {
        return Err("Herdr rejected the content pane request".to_owned());
    }
    let response: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| "Herdr returned an invalid content pane response".to_owned())?;
    if response.get("error").is_some() {
        return Err("Herdr rejected the content pane request".to_owned());
    }
    Ok(response)
}

fn run_herdr_status(args: &[OsString]) -> bool {
    let executable = env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| OsString::from("herdr"));
    Command::new(executable)
        .args(args)
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && serde_json::from_slice::<Value>(&output.stdout)
                    .map(|response| response.get("error").is_none())
                    .unwrap_or_else(|_| output.stdout.is_empty())
        })
}

fn pane_id_from_response(response: &Value) -> Option<String> {
    [
        "/result/pane/pane_id",
        "/result/neighbor/pane_id",
        "/result/pane_id",
        "/result/pane/id",
    ]
    .into_iter()
    .find_map(|pointer| response.pointer(pointer).and_then(Value::as_str))
    .filter(|value| !value.is_empty())
    .map(ToOwned::to_owned)
}

pub fn run_content_pane(cwd: PathBuf) -> Result<(), String> {
    let raw_control =
        env::var_os(CONTROL_ENV).ok_or_else(|| format!("{CONTROL_ENV} is missing"))?;
    let control = PathBuf::from(raw_control);
    if control.parent() != Some(scratch_dir().as_path()) {
        return Err("content control path is outside the private scratch directory".to_owned());
    }
    run_viewer(cwd, control).map_err(|error| error.to_string())
}

#[derive(Debug)]
enum Document {
    Text {
        title: String,
        context: String,
        lines: Vec<Line<'static>>,
        gutter_width: Option<usize>,
        back: Option<ContentRequest>,
    },
    Diff {
        title: String,
        context: String,
        view: DiffViewState,
        back: Option<ContentRequest>,
    },
    Changelist {
        title: String,
        context: String,
        description: Vec<String>,
        files: Vec<ChangedFile>,
        selected: usize,
    },
    Failed {
        title: String,
        message: String,
    },
}

impl Document {
    fn title(&self) -> &str {
        match self {
            Self::Text { title, .. }
            | Self::Diff { title, .. }
            | Self::Changelist { title, .. }
            | Self::Failed { title, .. } => title,
        }
    }

    fn context(&self) -> &str {
        match self {
            Self::Text { context, .. }
            | Self::Diff { context, .. }
            | Self::Changelist { context, .. } => context,
            Self::Failed { message, .. } => message,
        }
    }

    fn back(&self) -> Option<&ContentRequest> {
        match self {
            Self::Text { back, .. } | Self::Diff { back, .. } => back.as_ref(),
            _ => None,
        }
    }

    fn header_height(&self) -> u16 {
        match self {
            Self::Diff { .. } => 3,
            _ => 2,
        }
    }
}

struct ViewerState {
    cwd: PathBuf,
    request: ContentRequest,
    document: Document,
    scroll_y: usize,
    body_width: usize,
    body_height: usize,
    toolbar_hits: Vec<diff::ToolbarHit>,
}

impl ViewerState {
    fn new(cwd: PathBuf, request: ContentRequest) -> Self {
        let document = load_document(&cwd, &request, None);
        Self {
            cwd,
            request,
            document,
            scroll_y: 0,
            body_width: 1,
            body_height: 1,
            toolbar_hits: Vec::new(),
        }
    }

    fn install(&mut self, request: ContentRequest) {
        self.document = load_document(&self.cwd, &request, None);
        self.request = request;
        self.scroll_y = 0;
        self.toolbar_hits.clear();
        rename_own_pane(self.document.title());
    }

    fn render_lines(&self) -> Vec<Line<'static>> {
        match &self.document {
            Document::Text { lines, .. } => lines.clone(),
            Document::Diff { view, .. } => view.body_lines(),
            Document::Failed { message, .. } => vec![Line::styled(
                message.clone(),
                Style::default().fg(Color::Red),
            )],
            Document::Changelist {
                description,
                files,
                selected,
                ..
            } => {
                let mut lines = Vec::new();
                lines.push(Line::styled(
                    "Description",
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                if description.is_empty() {
                    lines.push(Line::styled("(no description)", Color::DarkGray));
                } else {
                    lines.extend(description.iter().cloned().map(Line::raw));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    format!("Files ({})", files.len()),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                for (index, file) in files.iter().enumerate() {
                    let marker = if index == *selected { ">" } else { " " };
                    let path = file
                        .client_path
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| file.depot_path.clone());
                    let style = if index == *selected {
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    lines.push(Line::styled(
                        format!("{marker} {:<11} {path}", file.action.canonical_name()),
                        style,
                    ));
                }
                lines
            }
        }
    }

    fn render_rows(&self) -> Vec<Line<'static>> {
        let gutter_width = self.gutter_width();
        let width = self.body_width.max(1);
        self.render_lines()
            .iter()
            .flat_map(|line| {
                let wrapped = gutter_width.map_or_else(
                    || wrap::wrap_line(line, width),
                    |gutter| wrap::wrap_line_with_gutter(line, width, gutter),
                );
                wrapped
                    .into_iter()
                    .map(move |row| diff::pad_background(row, width))
            })
            .collect()
    }

    fn gutter_width(&self) -> Option<usize> {
        match &self.document {
            Document::Text { gutter_width, .. } => *gutter_width,
            Document::Diff { view, .. } => Some(view.gutter_width),
            _ => None,
        }
    }

    fn visual_row_offset(&self, source_row: usize) -> usize {
        let gutter_width = self.gutter_width();
        self.render_lines()
            .iter()
            .take(source_row)
            .map(|line| {
                gutter_width.map_or_else(
                    || wrap::wrap_line(line, self.body_width.max(1)).len(),
                    |gutter| {
                        wrap::wrap_line_with_gutter(line, self.body_width.max(1), gutter).len()
                    },
                )
            })
            .sum()
    }

    fn clamp_scroll(&mut self) {
        let maximum = self
            .render_rows()
            .len()
            .saturating_sub(self.body_height.max(1));
        self.scroll_y = self.scroll_y.min(maximum);
    }

    fn move_vertical(&mut self, delta: isize) {
        if let Document::Changelist {
            description,
            files,
            selected,
            ..
        } = &mut self.document
        {
            if files.is_empty() {
                return;
            }
            *selected = if delta < 0 {
                selected.saturating_sub(delta.unsigned_abs())
            } else {
                selected.saturating_add(delta as usize).min(files.len() - 1)
            };
            let heading_rows = description.len().max(1).saturating_add(3);
            let selected_source_row = heading_rows.saturating_add(*selected);
            let selected_row = self.visual_row_offset(selected_source_row);
            let selected_height = self
                .render_lines()
                .get(selected_source_row)
                .map(|line| wrap::wrap_line(line, self.body_width.max(1)).len())
                .unwrap_or(1);
            let selected_bottom = selected_row.saturating_add(selected_height);
            if selected_row < self.scroll_y {
                self.scroll_y = selected_row;
            } else if selected_bottom > self.scroll_y.saturating_add(self.body_height) {
                self.scroll_y = selected_bottom.saturating_sub(self.body_height);
            }
            return;
        }
        if delta < 0 {
            self.scroll_y = self.scroll_y.saturating_sub(delta.unsigned_abs());
        } else {
            self.scroll_y = self.scroll_y.saturating_add(delta as usize);
        }
        self.clamp_scroll();
    }

    fn activate(&mut self) {
        let Document::Changelist {
            files, selected, ..
        } = &self.document
        else {
            return;
        };
        let Some(path) = files
            .get(*selected)
            .and_then(|file| file.client_path.clone())
        else {
            return;
        };
        let back = self.request.clone();
        let change = match self.request {
            ContentRequest::Changelist { change } => change,
            _ => return,
        };
        let action = files.get(*selected).map(|file| file.action.clone());
        let request = ContentRequest::Diff {
            change,
            path,
            action,
            fold_context: diff_fold_context(),
        };
        self.document = load_document(&self.cwd, &request, Some(back));
        self.request = request;
        self.scroll_y = 0;
        rename_own_pane(self.document.title());
    }

    fn go_back(&mut self) -> bool {
        let Some(back) = self.document.back().cloned() else {
            return false;
        };
        self.install(back);
        true
    }

    fn handle_diff_key(&mut self, code: KeyCode) -> bool {
        {
            let Document::Diff { view, .. } = &mut self.document else {
                return false;
            };
            match code {
                KeyCode::Char('[') => {
                    view.step_hunk(-1);
                }
                KeyCode::Char(']') => {
                    view.step_hunk(1);
                }
                KeyCode::Char('e') => view.toggle_folds(),
                _ => return false,
            }
        }
        if matches!(code, KeyCode::Char('[') | KeyCode::Char(']')) {
            self.scroll_to_current_hunk();
        } else {
            self.clamp_scroll();
        }
        true
    }

    fn handle_diff_click(&mut self, column: u16, row: u16) -> bool {
        let header = self.document.header_height();
        if row < header {
            if row + 1 != header {
                return false;
            }
            let Some(action) = diff::hit_action(&self.toolbar_hits, column) else {
                return false;
            };
            return self.apply_toolbar(action);
        }
        let Document::Diff { view, .. } = &mut self.document else {
            return false;
        };
        let visual = self
            .scroll_y
            .saturating_add(row.saturating_sub(header) as usize);
        let Some(fold_id) = view.fold_at_visual_row(visual, self.body_width.max(1)) else {
            return false;
        };
        view.expand_fold(fold_id);
        self.clamp_scroll();
        true
    }

    fn apply_toolbar(&mut self, action: DiffToolbarAction) -> bool {
        {
            let Document::Diff { view, .. } = &mut self.document else {
                return false;
            };
            match action {
                DiffToolbarAction::PrevHunk => {
                    view.step_hunk(-1);
                }
                DiffToolbarAction::NextHunk => {
                    view.step_hunk(1);
                }
                DiffToolbarAction::ToggleFolds => view.toggle_folds(),
            }
        }
        match action {
            DiffToolbarAction::PrevHunk | DiffToolbarAction::NextHunk => {
                self.scroll_to_current_hunk();
            }
            DiffToolbarAction::ToggleFolds => self.clamp_scroll(),
        }
        true
    }

    fn scroll_to_current_hunk(&mut self) {
        let Document::Diff { view, .. } = &self.document else {
            return;
        };
        let Some(overlay) = view.current_hunk_overlay_index() else {
            return;
        };
        let visual = view.visual_row_for_overlay(overlay, self.body_width.max(1));
        self.scroll_y = visual.saturating_sub(1);
        self.clamp_scroll();
    }
}

fn load_document(cwd: &Path, request: &ContentRequest, back: Option<ContentRequest>) -> Document {
    match request {
        ContentRequest::File { path } => file_document(path),
        ContentRequest::Diff {
            change,
            path,
            action,
            fold_context,
        } => diff_document(cwd, *change, path, action.as_ref(), *fold_context, back),
        ContentRequest::Changelist { change } => changelist_document(cwd, *change),
    }
}

fn file_document(path: &Path) -> Document {
    let title = filename(path);
    let context = path.display().to_string();
    match read_workspace_preview(path, None, None, None) {
        PreviewContent::Text { lines, truncated } => {
            let (numbered, gutter_width) = numbered_file_lines(&title, &lines, truncated.as_ref());
            Document::Text {
                title,
                context,
                lines: numbered,
                gutter_width: Some(gutter_width),
                back: None,
            }
        }
        preview => {
            let lines = metadata_lines(preview);
            Document::Text {
                title,
                context,
                lines,
                gutter_width: None,
                back: None,
            }
        }
    }
}

fn numbered_file_lines(
    name: &str,
    raw_lines: &[String],
    truncated: Option<&PreviewTruncation>,
) -> (Vec<Line<'static>>, usize) {
    let text = raw_lines.join("\n");
    let highlighted = syntax::highlight(name, &text, raw_lines.len())
        .unwrap_or_else(|| raw_lines.iter().cloned().map(Line::raw).collect());
    let number_width = raw_lines.len().max(1).ilog10() as usize + 1;
    let gutter_width = number_width + 2;
    let mut output = highlighted
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let mut spans = vec![Span::styled(
                format!("{:>number_width$}  ", index + 1),
                Style::default().fg(Color::DarkGray),
            )];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect::<Vec<_>>();
    if output.is_empty() {
        output.push(Line::from(vec![
            Span::styled(" ".repeat(gutter_width), Color::DarkGray),
            Span::styled("(empty file)", Color::DarkGray),
        ]));
    }
    if let Some(reason) = truncated {
        let message = match reason {
            PreviewTruncation::ByteBudget { limit } => {
                format!("truncated: {limit} byte preview budget exceeded")
            }
            PreviewTruncation::LineBudget { limit } => {
                format!("truncated: {limit} line preview budget exceeded")
            }
        };
        output.push(Line::from(vec![
            Span::styled(" ".repeat(gutter_width), Color::DarkGray),
            Span::styled(message, Color::Yellow),
        ]));
    }
    (output, gutter_width)
}

fn metadata_lines(preview: PreviewContent) -> Vec<Line<'static>> {
    match preview {
        PreviewContent::None => vec![Line::raw("Nothing to preview")],
        PreviewContent::Directory { name, child_count } => vec![
            Line::styled(format!("{name}/"), Modifier::BOLD),
            Line::raw(
                child_count
                    .map(|count| format!("{count} item(s)"))
                    .unwrap_or_else(|| "directory".to_owned()),
            ),
        ],
        PreviewContent::Failed { message } => vec![Line::styled(message, Color::Red)],
        PreviewContent::Binary {
            size,
            file_type,
            have_rev,
            head_rev,
        } => vec![
            Line::styled("Binary file", Modifier::BOLD),
            Line::raw("Workspace preview does not parse asset contents."),
            Line::raw(format!(
                "Type: {}",
                file_type.unwrap_or_else(|| "unknown".to_owned())
            )),
            Line::raw(format!(
                "Size: {}",
                size.map(|value| format!("{value} bytes"))
                    .unwrap_or_else(|| "unknown".to_owned())
            )),
            Line::raw(format!(
                "Have: {}  Head: {}",
                revision_label(have_rev),
                revision_label(head_rev)
            )),
        ],
        PreviewContent::Text { .. } => unreachable!("handled by file_document"),
    }
}

fn revision_label(revision: Option<u64>) -> String {
    revision
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn diff_document(
    cwd: &Path,
    change: ChangelistId,
    path: &Path,
    action: Option<&FileAction>,
    fold_context: usize,
    back: Option<ContentRequest>,
) -> Document {
    let title = format!("Diff · {}", filename(path));
    let client = P4Client::new(StdProcessTransport, "p4", cwd);
    let unified = match load_workspace_diff(&client, path) {
        Ok(lines) => lines,
        Err(error) => {
            return Document::Failed {
                title,
                message: error.to_string(),
            };
        }
    };
    match read_workspace_preview(path, None, None, None) {
        PreviewContent::Binary { .. } | PreviewContent::Directory { .. } => {
            let lines = metadata_lines(read_workspace_preview(path, None, None, None));
            Document::Text {
                title,
                context: format!("CL {change} · {}", path.display()),
                lines,
                gutter_width: None,
                back,
            }
        }
        preview => {
            let (new_lines, truncated) = match preview {
                PreviewContent::Text { lines, truncated } => (lines, truncated),
                PreviewContent::Failed { .. } | PreviewContent::None => (Vec::new(), None),
                PreviewContent::Binary { .. } | PreviewContent::Directory { .. } => unreachable!(),
            };
            let kind = action.map(FileDiffKind::from_action);
            let model = build_file_diff(&new_lines, &unified, kind, fold_context);
            let action_label = action.map(FileAction::canonical_name).unwrap_or("diff");
            let context = format!(
                "CL {change} · {action_label} · +{} -{} · {}",
                model.added,
                model.removed,
                path.display()
            );
            let truncated = merge_truncation_notices(
                truncated.map(|reason| match reason {
                    PreviewTruncation::ByteBudget { limit } => {
                        format!("truncated: {limit} byte preview budget exceeded")
                    }
                    PreviewTruncation::LineBudget { limit } => {
                        format!("truncated: {limit} line preview budget exceeded")
                    }
                }),
                model.truncated.clone(),
            );
            Document::Diff {
                title,
                context,
                view: DiffViewState::new(filename(path), model, truncated),
                back,
            }
        }
    }
}

fn diff_fold_context() -> usize {
    panel_restore::load_panel_config(
        env::var_os("HERDR_PLUGIN_CONFIG_DIR")
            .map(PathBuf::from)
            .as_deref(),
    )
    .map(|config| config.diff_fold_context)
    .unwrap_or(crate::domain::DEFAULT_FOLD_CONTEXT)
}

fn parse_request_fold_context(value: &Value) -> usize {
    match value.get("diff_fold_context").and_then(Value::as_u64) {
        Some(parsed) if parsed <= crate::domain::MAX_FOLD_CONTEXT as u64 => parsed as usize,
        Some(_) => crate::domain::MAX_FOLD_CONTEXT,
        None => diff_fold_context(),
    }
}

fn merge_truncation_notices(preview: Option<String>, diff: Option<String>) -> Option<String> {
    match (preview, diff) {
        (Some(left), Some(right)) if left == right => Some(left),
        (Some(left), Some(right)) => Some(format!("{left}; {right}")),
        (left, right) => left.or(right),
    }
}

fn changelist_document(cwd: &Path, change: ChangelistId) -> Document {
    let title = format!("CL {change}");
    match load_changelist(cwd, change) {
        Ok(changelist) => Document::Changelist {
            context: format!(
                "{} · {} · {} file(s)",
                changelist.status.canonical_name(),
                changelist.client,
                changelist.files.len()
            ),
            description: changelist
                .description
                .lines()
                .map(ToOwned::to_owned)
                .collect(),
            files: changelist.files,
            selected: 0,
            title,
        },
        Err(message) => Document::Failed { title, message },
    }
}

fn load_changelist(cwd: &Path, change: ChangelistId) -> Result<Changelist, String> {
    let client = P4Client::new(StdProcessTransport, "p4", cwd);
    match change {
        ChangelistId::Numbered(number) => {
            let response = client
                .run(&P4Query::DescribeSummary { change: number })
                .map_err(|error| error.to_string())?;
            let mut changelist =
                changelist_from_describe(&response.records).map_err(|error| error.to_string())?;
            if let Ok(opened) = client.run(&P4Query::Opened { change })
                && let Ok(files) = changed_files_from_opened(&opened.records)
                && !files.is_empty()
            {
                changelist.files = files;
            }
            Ok(changelist)
        }
        ChangelistId::Default => {
            let response = client
                .run(&P4Query::Opened { change })
                .map_err(|error| error.to_string())?;
            let files =
                changed_files_from_opened(&response.records).map_err(|error| error.to_string())?;
            Ok(Changelist {
                id: change,
                status: crate::domain::ChangelistStatus::Pending,
                owner: String::new(),
                client: String::new(),
                description: String::new(),
                files,
                preserved_spec_fields: Default::default(),
                spec_token: None,
                content_token: None,
            })
        }
    }
}

fn run_viewer(cwd: PathBuf, control: PathBuf) -> io::Result<()> {
    let initial_raw = fs::read_to_string(&control).unwrap_or_default();
    let initial = ContentRequest::from_json(&initial_raw)
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
    let mut state = ViewerState::new(cwd, initial);
    rename_own_pane(state.document.title());
    report_identity(&control);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
        let _ = disable_raw_mode();
        return Err(error);
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
            return Err(error);
        }
    };

    let result = terminal
        .clear()
        .and_then(|()| viewer_loop(&mut terminal, &mut state, &control, initial_raw));
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = terminal.show_cursor();
    let _ = fs::remove_file(&control);
    if result.as_ref().is_ok_and(|close| *close) {
        close_own_pane();
    }
    result.map(|_| ())
}

fn viewer_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut ViewerState,
    control: &Path,
    mut last_raw: String,
) -> io::Result<bool> {
    let mut last_heartbeat = Instant::now();
    loop {
        terminal.draw(|frame| draw_viewer(frame, state))?;
        if event::poll(POLL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') => return Ok(true),
                    KeyCode::Esc if !state.go_back() => return Ok(true),
                    KeyCode::Esc => {}
                    KeyCode::Enter => state.activate(),
                    KeyCode::Up | KeyCode::Char('k') => state.move_vertical(-1),
                    KeyCode::Down | KeyCode::Char('j') => state.move_vertical(1),
                    KeyCode::PageUp => state.move_vertical(-(state.body_height as isize)),
                    KeyCode::PageDown => state.move_vertical(state.body_height as isize),
                    KeyCode::Home => state.scroll_y = 0,
                    KeyCode::End => {
                        state.scroll_y = state.render_rows().len();
                        state.clamp_scroll();
                    }
                    other => {
                        let _ = state.handle_diff_key(other);
                    }
                },
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => state.move_vertical(-3),
                    MouseEventKind::ScrollDown => state.move_vertical(3),
                    MouseEventKind::Down(MouseButton::Left) => {
                        let _ = state.handle_diff_click(mouse.column, mouse.row);
                    }
                    _ => {}
                },
                Event::Resize(_, _)
                | Event::FocusGained
                | Event::FocusLost
                | Event::Paste(_)
                | Event::Key(_) => {}
            }
        }

        if let Ok(raw) = fs::read_to_string(control)
            && raw != last_raw
            && let Ok(request) = ContentRequest::from_json(&raw)
        {
            state.install(request);
            last_raw = raw;
        }
        if last_heartbeat.elapsed() >= HEARTBEAT {
            report_identity(control);
            last_heartbeat = Instant::now();
        }
    }
}

fn draw_viewer(frame: &mut ratatui::Frame<'_>, state: &mut ViewerState) {
    let header_height = state.document.header_height();
    let chunks = Layout::vertical([
        Constraint::Length(header_height),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(frame.area());
    let mut title_spans = vec![
        Span::styled(
            state.document.title().to_owned(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  [auto wrap]"),
    ];
    if let Document::Diff { view, .. } = &state.document {
        title_spans.push(Span::raw("  "));
        title_spans.extend(diff::stats_spans(&view.model));
    }
    let mut header = vec![
        Line::from(title_spans),
        Line::styled(state.document.context().to_owned(), Color::DarkGray),
    ];
    if let Document::Diff { view, .. } = &state.document {
        let (toolbar, hits) = diff::toolbar_line(view, chunks[0].width as usize);
        state.toolbar_hits = hits;
        header.push(toolbar);
    } else {
        state.toolbar_hits.clear();
    }
    frame.render_widget(Paragraph::new(header), chunks[0]);

    state.body_width = chunks[1].width as usize;
    state.body_height = chunks[1].height as usize;
    state.clamp_scroll();
    let paragraph = Paragraph::new(state.render_rows())
        .scroll((state.scroll_y.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(paragraph, chunks[1]);

    let footer = match &state.document {
        Document::Changelist { .. } => "↑↓/wheel: select   Enter: diff   q/Esc: close",
        Document::Diff { back: Some(_), .. } => {
            "[/]: hunk   e: folds   click fold: expand   Esc: back   q: close"
        }
        Document::Diff { .. } => "[/]: hunk   e: folds   click fold: expand   q/Esc: close",
        Document::Text { back: Some(_), .. } => {
            "↑↓/wheel: scroll   PgUp/PgDn: page   Esc: back   q: close"
        }
        _ => "↑↓/wheel: scroll   PgUp/PgDn: page   q/Esc: close",
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
}

fn report_identity(control: &Path) {
    let Some(pane_id) = own_pane_id() else {
        return;
    };
    let token = control
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_owned());
    let _ = run_herdr_status(&[
        OsString::from("pane"),
        OsString::from("report-metadata"),
        OsString::from(&pane_id),
        OsString::from("--source"),
        OsString::from(CONTENT_SOURCE),
        OsString::from("--token"),
        OsString::from(format!("{CONTENT_SOURCE}={stamp}")),
        OsString::from("--token"),
        OsString::from(format!("{CONTROL_TOKEN}={token}")),
    ]);
}

fn rename_own_pane(title: &str) {
    let Some(pane_id) = own_pane_id() else {
        return;
    };
    let label = format!("{} · Perforce", title.chars().take(48).collect::<String>());
    let _ = run_herdr_status(&[
        OsString::from("pane"),
        OsString::from("rename"),
        OsString::from(pane_id),
        OsString::from(label),
    ]);
}

fn close_own_pane() {
    if let Some(pane_id) = own_pane_id() {
        let _ = close_pane(&pane_id);
    }
}

fn own_pane_id() -> Option<String> {
    env::var("HERDR_PANE_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_requests_round_trip_without_shell_quoting() {
        let requests = [
            ContentRequest::File {
                path: PathBuf::from(r"C:\Workspace with spaces\a#b.rs"),
            },
            ContentRequest::Diff {
                change: ChangelistId::Numbered(42),
                path: PathBuf::from(r"C:\Workspace\src\main.rs"),
                action: Some(FileAction::Edit),
                fold_context: 8,
            },
            ContentRequest::Changelist {
                change: ChangelistId::Default,
            },
        ];
        for request in requests {
            let encoded = request.to_json().expect("encode");
            assert_eq!(
                ContentRequest::from_json(&encoded).expect("decode"),
                request
            );
        }
    }

    #[test]
    fn content_split_targets_the_left_region_and_passes_state_as_environment() {
        let args = split_args(
            "workspace:p1",
            Path::new(r"C:\Workspace with spaces"),
            Path::new(r"C:\Temp\content.json"),
            OsStr::new(r"C:\Plugin\bin;C:\Windows"),
            Some(OsStr::new(r"C:\Herdr\plugin-config")),
        );
        assert_eq!(args[3], "workspace:p1");
        assert_eq!(args[7], "0.5");
        assert!(args.iter().any(|arg| arg == "--focus"));
        assert!(args.iter().any(|arg| {
            arg.to_string_lossy()
                .starts_with("HERDR_P4_CONTENT_CONTROL=")
        }));
        assert!(args.iter().any(|arg| {
            arg.to_string_lossy()
                .starts_with(r"HERDR_PLUGIN_CONFIG_DIR=C:\Herdr\plugin-config")
        }));
        let without_config = split_args(
            "workspace:p1",
            Path::new(r"C:\Workspace"),
            Path::new(r"C:\Temp\content.json"),
            OsStr::new(r"C:\Plugin\bin"),
            None,
        );
        assert!(
            without_config
                .iter()
                .all(|arg| !arg.to_string_lossy().contains("HERDR_PLUGIN_CONFIG_DIR="))
        );
    }

    #[test]
    fn layout_geometry_finds_the_pane_immediately_left_of_navigation() {
        let layout = json!({"result":{"layout":{
            "panes":[
                {"pane_id":"w:agent","rect":{"x":0,"y":0,"width":40,"height":50}},
                {"pane_id":"w:content","rect":{"x":40,"y":0,"width":40,"height":50}},
                {"pane_id":"w:nav","rect":{"x":80,"y":0,"width":20,"height":50}}
            ],
            "splits":[
                {"direction":"right","ratio":0.8,"rect":{"x":0,"y":0,"width":100,"height":50}},
                {"direction":"right","ratio":0.5,"rect":{"x":0,"y":0,"width":80,"height":50}}
            ]
        }}});
        assert_eq!(
            horizontal_neighbor(&layout, "w:nav", true).as_deref(),
            Some("w:content")
        );
        assert_eq!(horizontal_neighbor(&layout, "w:nav", false), None);
        assert!(directly_left_of(&layout, "w:nav", "w:content"));
        assert!(navigation_resize_args_for_layout(&layout, "w:nav").is_none());
    }

    #[test]
    fn navigation_resize_plan_targets_twenty_percent_of_actual_split() {
        let layout = json!({"result":{"layout":{
            "panes":[
                {"pane_id":"w:agent","rect":{"x":0,"y":0,"width":50,"height":50}},
                {"pane_id":"w:nav","rect":{"x":50,"y":0,"width":50,"height":50}}
            ],
            "splits":[
                {"direction":"right","ratio":0.5,"rect":{"x":0,"y":0,"width":100,"height":50}}
            ]
        }}});
        let args = navigation_resize_args_for_layout(&layout, "w:nav").expect("resize");
        assert_eq!(
            args,
            [
                "pane",
                "resize",
                "--direction",
                "right",
                "--amount",
                "0.300000",
                "--pane",
                "w:nav"
            ]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn reversed_content_and_navigation_are_detected_before_repair() {
        let layout = json!({"result":{"layout":{"panes":[
            {"pane_id":"w:agent","rect":{"x":0,"y":0,"width":40,"height":50}},
            {"pane_id":"w:nav","rect":{"x":40,"y":0,"width":20,"height":50}},
            {"pane_id":"w:content","rect":{"x":60,"y":0,"width":40,"height":50}}
        ]}}});
        assert!(!directly_left_of(&layout, "w:nav", "w:content"));
        assert!(directly_left_of(&layout, "w:content", "w:nav"));
    }

    #[test]
    fn pane_ids_accept_current_and_legacy_response_shapes() {
        for raw in [
            r#"{"result":{"pane":{"pane_id":"w:p2"}}}"#,
            r#"{"result":{"neighbor":{"pane_id":"w:p1"}}}"#,
            r#"{"result":{"pane_id":"w:p3"}}"#,
        ] {
            let response: Value = serde_json::from_str(raw).expect("JSON");
            assert!(pane_id_from_response(&response).is_some());
        }
    }

    #[test]
    fn viewer_health_accepts_only_the_internal_viewer_subcommand() {
        let viewer = json!({
            "result": { "process_info": { "foreground_processes": [{
                "name": "herdr-p4.exe",
                "argv": ["C:/Plugin/herdr-p4.exe", "viewer"]
            }]}}
        });
        let navigation = json!({
            "result": { "process_info": { "foreground_processes": [{
                "name": "herdr-p4.exe",
                "argv": ["C:/Plugin/herdr-p4.exe", "pane"]
            }]}}
        });
        assert!(viewer_process_is_active(&viewer));
        assert!(!viewer_process_is_active(&navigation));
    }

    #[test]
    fn content_discovery_is_scoped_to_the_navigation_tab() {
        let response = json!({"result":{"panes":[
            {"pane_id":"w:p1","tab_id":"w:t1","label":"Agent"},
            {"pane_id":"w:p2","tab_id":"w:t1","label":"a.rs · Perforce","tokens":{
                "herdr-perforce-content":"1",
                "herdr-p4-content-control":"content-1-a-0.json"
            }},
            {"pane_id":"w:p3","tab_id":"w:t1","label":"Perforce"},
            {"pane_id":"w:p4","tab_id":"w:t2","label":"b.rs · Perforce","tokens":{
                "herdr-perforce-content":"1"
            }}
        ]}});
        let (pane, control) = discover_content_pane_in(&response, "w:p3").expect("content");
        assert_eq!(pane, "w:p2");
        assert_eq!(
            control.and_then(|path| path.file_name().map(|name| name.to_owned())),
            Some(OsString::from("content-1-a-0.json"))
        );
        assert!(control_path_from_token("../outside.json").is_none());
    }

    #[test]
    fn diff_request_round_trips_optional_action() {
        let encoded = ContentRequest::Diff {
            change: ChangelistId::Default,
            path: PathBuf::from(r"C:\ws\a.rs"),
            action: None,
            fold_context: 0,
        }
        .to_json()
        .expect("encode");
        assert!(
            !encoded.contains("action"),
            "absent action must stay omitted: {encoded}"
        );
        assert!(encoded.contains("\"diff_fold_context\":0"));
        assert_eq!(
            ContentRequest::from_json(&encoded).expect("decode"),
            ContentRequest::Diff {
                change: ChangelistId::Default,
                path: PathBuf::from(r"C:\ws\a.rs"),
                action: None,
                fold_context: 0,
            }
        );
        let legacy = r#"{"version":1,"kind":"diff","change":"default","path":"C:\\ws\\a.rs"}"#;
        let ContentRequest::Diff {
            fold_context: recovered,
            ..
        } = ContentRequest::from_json(legacy).expect("legacy")
        else {
            panic!("expected diff");
        };
        assert_eq!(recovered, diff_fold_context());
    }

    #[test]
    fn truncation_notices_from_preview_and_diff_are_both_kept() {
        assert_eq!(
            merge_truncation_notices(
                Some("truncated: 512 byte preview budget exceeded".into()),
                Some("truncated: 4000 line diff budget exceeded".into()),
            )
            .as_deref(),
            Some(
                "truncated: 512 byte preview budget exceeded; truncated: 4000 line diff budget exceeded"
            )
        );
    }
}
