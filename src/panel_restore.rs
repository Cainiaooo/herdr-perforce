//! Remembered Herdr pane lifecycle state.
//!
//! This module deliberately contains no process spawning. It parses the small
//! plugin-owned config/state files and turns Herdr JSON snapshots into pure
//! restore decisions that can be covered without a running Herdr server.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::{Map, Value, json};

use crate::domain::{DEFAULT_FOLD_CONTEXT, MAX_FOLD_CONTEXT};

const CONFIG_FILE: &str = "panel.json";
const STATE_FILE: &str = "remembered-workspaces.json";
const LAYOUT_FILE: &str = "layout.json";
const MAX_CONFIG_BYTES: u64 = 16 * 1024;
const MAX_STATE_BYTES: u64 = 64 * 1024;
const MAX_LAYOUT_BYTES: u64 = 64 * 1024;
const MAX_REMEMBERED_WORKSPACES: usize = 128;
const CONTENT_SOURCE_TOKEN: &str = "herdr-perforce-content";
const CONTENT_CONTROL_TOKEN: &str = "herdr-p4-content-control";
pub const DEFAULT_NAVIGATION_SHARE: f64 = 0.2;
const MIN_NAVIGATION_SHARE: f64 = 0.08;
const MAX_NAVIGATION_SHARE: f64 = 0.32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelOpenMode {
    Manual,
    Remembered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelConfig {
    pub open_mode: PanelOpenMode,
    pub diff_fold_context: usize,
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            open_mode: PanelOpenMode::Remembered,
            diff_fold_context: DEFAULT_FOLD_CONTEXT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RememberedWorkspace {
    pub cwd: PathBuf,
    pub workspace_cwd: PathBuf,
    pub workspace_id: Option<String>,
    pub pane_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrWorkspace {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrPane {
    pub id: String,
    pub workspace_id: String,
    pub cwd: PathBuf,
    pub label: Option<String>,
    pub focused: bool,
    pub content_owned: bool,
}

pub fn load_panel_open_mode(config_dir: Option<&Path>) -> Result<PanelOpenMode, &'static str> {
    Ok(load_panel_config(config_dir)?.open_mode)
}

pub fn load_panel_config(config_dir: Option<&Path>) -> Result<PanelConfig, &'static str> {
    let Some(config_dir) = config_dir else {
        return Ok(PanelConfig::default());
    };
    let path = config_dir.join(CONFIG_FILE);
    if !path.exists() {
        return Ok(PanelConfig::default());
    }
    let value = read_bounded_json(&path, MAX_CONFIG_BYTES, "panel config could not be read")?;
    let object = strict_object(
        &value,
        &["open_mode", "diff_fold_context"],
        "panel config is invalid",
    )?;
    let open_mode = match object.get("open_mode").and_then(Value::as_str) {
        Some("manual") => PanelOpenMode::Manual,
        Some("remembered") => PanelOpenMode::Remembered,
        _ => return Err("panel config open_mode must be manual or remembered"),
    };
    Ok(PanelConfig {
        open_mode,
        diff_fold_context: parse_diff_fold_context(object.get("diff_fold_context"))?,
    })
}

fn parse_diff_fold_context(value: Option<&Value>) -> Result<usize, &'static str> {
    match value {
        None => Ok(DEFAULT_FOLD_CONTEXT),
        Some(Value::Number(number)) => {
            let parsed = number
                .as_u64()
                .ok_or("panel config diff_fold_context must be an integer from 0 to 200")?;
            if parsed > MAX_FOLD_CONTEXT as u64 {
                return Err("panel config diff_fold_context must be an integer from 0 to 200");
            }
            Ok(parsed as usize)
        }
        _ => Err("panel config diff_fold_context must be an integer from 0 to 200"),
    }
}

pub fn load_remembered_workspaces(
    state_dir: Option<&Path>,
) -> Result<Vec<RememberedWorkspace>, &'static str> {
    let Some(state_dir) = state_dir else {
        return Ok(Vec::new());
    };
    let path = state_dir.join(STATE_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let value = read_bounded_json(&path, MAX_STATE_BYTES, "panel state could not be read")?;
    let object = strict_object(&value, &["version", "workspaces"], "panel state is invalid")?;
    if object.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("panel state version is unsupported");
    }
    let workspaces = object
        .get("workspaces")
        .and_then(Value::as_array)
        .ok_or("panel state workspaces are invalid")?;
    if workspaces.len() > MAX_REMEMBERED_WORKSPACES {
        return Err("panel state contains too many workspaces");
    }

    let entries = workspaces
        .iter()
        .map(parse_remembered_workspace)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(coalesce_remembered_workspaces(entries))
}

pub fn coalesce_remembered_workspaces(
    entries: Vec<RememberedWorkspace>,
) -> Vec<RememberedWorkspace> {
    let mut kept = Vec::<RememberedWorkspace>::new();
    for entry in entries {
        if let Some(index) = kept
            .iter()
            .position(|existing| same_remembered_workspace(existing, &entry))
        {
            kept[index] = entry;
        } else {
            kept.push(entry);
        }
    }
    kept
}

fn same_remembered_workspace(left: &RememberedWorkspace, right: &RememberedWorkspace) -> bool {
    remembered_workspace_matches(
        left,
        &right.cwd,
        &right.workspace_cwd,
        right.workspace_id.as_deref(),
    )
}

fn remembered_workspace_matches(
    entry: &RememberedWorkspace,
    cwd: &Path,
    workspace_cwd: &Path,
    workspace_id: Option<&str>,
) -> bool {
    match (entry.workspace_id.as_deref(), workspace_id) {
        (Some(left), Some(right)) if left == right => return true,
        _ => {}
    }
    paths_equal(&entry.workspace_cwd, workspace_cwd) || paths_equal(&entry.cwd, cwd)
}

pub fn remember_workspace(
    state_dir: &Path,
    cwd: &Path,
    workspace_cwd: &Path,
    workspace_id: Option<&str>,
    pane_id: Option<&str>,
) -> Result<(), &'static str> {
    let cwd = strip_verbatim_prefix(cwd);
    let workspace_cwd = strip_verbatim_prefix(workspace_cwd);
    if !cwd.is_absolute() || fs::read_dir(&cwd).is_err() {
        return Err("remembered workspace cwd is not a readable absolute directory");
    }
    if !workspace_cwd.is_absolute() || fs::read_dir(&workspace_cwd).is_err() {
        return Err("Herdr workspace cwd is not a readable absolute directory");
    }
    fs::create_dir_all(state_dir).map_err(|_| "panel state directory could not be created")?;
    let mut entries = load_remembered_workspaces(Some(state_dir))?;
    entries
        .retain(|entry| !remembered_workspace_matches(entry, &cwd, &workspace_cwd, workspace_id));
    entries.push(RememberedWorkspace {
        cwd,
        workspace_cwd,
        workspace_id: nonempty(workspace_id),
        pane_id: nonempty(pane_id),
    });
    if entries.len() > MAX_REMEMBERED_WORKSPACES {
        let excess = entries.len() - MAX_REMEMBERED_WORKSPACES;
        entries.drain(..excess);
    }

    let value = json!({
        "version": 1,
        "workspaces": entries.iter().map(|entry| json!({
            "cwd": entry.cwd.to_string_lossy(),
            "workspace_cwd": entry.workspace_cwd.to_string_lossy(),
            "workspace_id": entry.workspace_id,
            "pane_id": entry.pane_id,
        })).collect::<Vec<_>>(),
    });
    let bytes =
        serde_json::to_vec_pretty(&value).map_err(|_| "panel state could not be encoded")?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("panel state is too large");
    }
    fs::write(state_dir.join(STATE_FILE), bytes).map_err(|_| "panel state could not be written")
}

#[derive(Debug, Clone, PartialEq)]
struct WorkspaceLayoutState {
    workspace_cwd: PathBuf,
    navigator_share: Option<f64>,
    content_request: Option<String>,
    navigation_view: Option<String>,
}

pub fn load_navigator_share(state_dir: Option<&Path>, workspace_cwd: &Path) -> f64 {
    let Some(state_dir) = state_dir else {
        return DEFAULT_NAVIGATION_SHARE;
    };
    load_workspace_layout(state_dir, workspace_cwd)
        .ok()
        .and_then(|entry| entry.navigator_share)
        .unwrap_or(DEFAULT_NAVIGATION_SHARE)
}

pub fn save_navigator_share(
    state_dir: &Path,
    workspace_cwd: &Path,
    share: f64,
) -> Result<(), &'static str> {
    if !share.is_finite() {
        return Err("navigator share is invalid");
    }
    // A just-opened Herdr plugin split is 50/50. Persist only after the user
    // has actually narrowed (or widened slightly past) the default sidebar.
    if (0.45..=0.55).contains(&share) {
        return Err("navigator share looks like the host default split");
    }
    let share = share.clamp(MIN_NAVIGATION_SHARE, MAX_NAVIGATION_SHARE);
    update_workspace_layout(state_dir, workspace_cwd, |entry| {
        entry.navigator_share = Some(share);
    })
}

pub fn workspace_layout_exists(
    state_dir: &Path,
    workspace_cwd: &Path,
) -> Result<bool, &'static str> {
    if !workspace_cwd.is_absolute() {
        return Err("workspace layout cwd is invalid");
    }
    let path = workspace_layout_path(state_dir, workspace_cwd);
    if !path.exists() {
        return Ok(false);
    }
    let state = parse_workspace_layout(&path)?;
    if !paths_equal(&state.workspace_cwd, workspace_cwd) {
        return Err("workspace layout identity does not match its file");
    }
    Ok(true)
}

pub fn load_content_request(state_dir: Option<&Path>, workspace_cwd: &Path) -> Option<String> {
    load_workspace_layout(state_dir?, workspace_cwd)
        .ok()?
        .content_request
}

pub fn save_content_request(
    state_dir: &Path,
    workspace_cwd: &Path,
    request: Option<&str>,
) -> Result<(), &'static str> {
    if request.is_some_and(|request| request.len() > MAX_CONFIG_BYTES as usize) {
        return Err("content request is too large");
    }
    update_workspace_layout(state_dir, workspace_cwd, |entry| {
        entry.content_request = request.map(ToOwned::to_owned);
    })
}

pub fn load_navigation_view(state_dir: Option<&Path>, workspace_cwd: &Path) -> Option<String> {
    load_workspace_layout(state_dir?, workspace_cwd)
        .ok()?
        .navigation_view
}

pub fn save_navigation_view(
    state_dir: &Path,
    workspace_cwd: &Path,
    view: &str,
) -> Result<(), &'static str> {
    if !matches!(view, "explorer" | "review") {
        return Err("navigation view is invalid");
    }
    update_workspace_layout(state_dir, workspace_cwd, |entry| {
        entry.navigation_view = Some(view.to_owned());
    })
}

fn update_workspace_layout(
    state_dir: &Path,
    workspace_cwd: &Path,
    update: impl FnOnce(&mut WorkspaceLayoutState),
) -> Result<(), &'static str> {
    if !workspace_cwd.is_absolute() {
        return Err("workspace layout cwd is invalid");
    }
    fs::create_dir_all(state_dir).map_err(|_| "panel state directory could not be created")?;
    let path = workspace_layout_path(state_dir, workspace_cwd);
    let mut entry = load_workspace_layout(state_dir, workspace_cwd)?;
    update(&mut entry);
    write_layout_state(&path, &entry)
}

fn workspace_layout_path(state_dir: &Path, workspace_cwd: &Path) -> PathBuf {
    let path = strip_verbatim_prefix(workspace_cwd);
    let text = path.to_string_lossy();
    #[cfg(windows)]
    let identity = text.replace('/', "\\").to_ascii_lowercase();
    #[cfg(not(windows))]
    let identity = text.into_owned();
    let digest = blake3::hash(identity.as_bytes()).to_hex();
    state_dir.join(format!("layout-{digest}.json"))
}

fn load_workspace_layout(
    state_dir: &Path,
    workspace_cwd: &Path,
) -> Result<WorkspaceLayoutState, &'static str> {
    if !workspace_cwd.is_absolute() {
        return Err("workspace layout cwd is invalid");
    }
    let path = workspace_layout_path(state_dir, workspace_cwd);
    if path.exists() {
        let state = parse_workspace_layout(&path)?;
        if !paths_equal(&state.workspace_cwd, workspace_cwd) {
            return Err("workspace layout identity does not match its file");
        }
        return Ok(state);
    }
    let legacy_path = state_dir.join(LAYOUT_FILE);
    let navigator_share = legacy_path
        .exists()
        .then(|| parse_legacy_navigator_share(&legacy_path))
        .transpose()?;
    Ok(WorkspaceLayoutState {
        workspace_cwd: strip_verbatim_prefix(workspace_cwd),
        navigator_share,
        content_request: None,
        navigation_view: None,
    })
}

fn write_layout_state(path: &Path, state: &WorkspaceLayoutState) -> Result<(), &'static str> {
    let value = json!({
        "version": 2,
        "workspace_cwd": state.workspace_cwd.to_string_lossy(),
        "navigator_share": state.navigator_share,
        "content_request": state.content_request,
        "navigation_view": state.navigation_view,
    });
    let bytes =
        serde_json::to_vec_pretty(&value).map_err(|_| "panel layout could not be encoded")?;
    if bytes.len() as u64 > MAX_LAYOUT_BYTES {
        return Err("panel layout is too large");
    }
    fs::write(path, bytes).map_err(|_| "panel layout could not be written")
}

fn parse_legacy_navigator_share(path: &Path) -> Result<f64, &'static str> {
    let value = read_bounded_json(path, MAX_LAYOUT_BYTES, "panel layout could not be read")?;
    let object = strict_object(
        &value,
        &["version", "navigator_share"],
        "panel layout is invalid",
    )?;
    if object.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("panel layout version is unsupported");
    }
    parse_layout_share(object.get("navigator_share"))
}

fn parse_workspace_layout(path: &Path) -> Result<WorkspaceLayoutState, &'static str> {
    let value = read_bounded_json(path, MAX_LAYOUT_BYTES, "workspace layout could not be read")?;
    let object = strict_object(
        &value,
        &[
            "version",
            "workspace_cwd",
            "navigator_share",
            "content_request",
            "navigation_view",
        ],
        "workspace layout is invalid",
    )?;
    if object.get("version").and_then(Value::as_u64) != Some(2) {
        return Err("workspace layout version is unsupported");
    }
    let workspace_cwd =
        required_absolute_path(&value, "workspace_cwd", "workspace layout cwd is invalid")?;
    let navigator_share = match object.get("navigator_share") {
        Some(Value::Null) | None => None,
        value => Some(parse_layout_share(value)?),
    };
    let content_request = match object.get("content_request") {
        Some(Value::String(request)) if request.len() <= MAX_CONFIG_BYTES as usize => {
            Some(request.clone())
        }
        Some(Value::Null) | None => None,
        _ => return Err("workspace content request is invalid"),
    };
    let navigation_view = match object.get("navigation_view") {
        Some(Value::String(view)) if matches!(view.as_str(), "explorer" | "review") => {
            Some(view.clone())
        }
        Some(Value::Null) | None => None,
        _ => return Err("workspace navigation view is invalid"),
    };
    Ok(WorkspaceLayoutState {
        workspace_cwd,
        navigator_share,
        content_request,
        navigation_view,
    })
}

fn parse_layout_share(value: Option<&Value>) -> Result<f64, &'static str> {
    let share = value
        .and_then(Value::as_f64)
        .ok_or("panel layout navigator_share is invalid")?;
    if !share.is_finite() || !(MIN_NAVIGATION_SHARE..=MAX_NAVIGATION_SHARE).contains(&share) {
        return Err("panel layout navigator_share is out of range");
    }
    Ok(share)
}

pub fn parse_pane_list(response: &Value) -> Result<Vec<HerdrPane>, &'static str> {
    response
        .pointer("/result/panes")
        .and_then(Value::as_array)
        .ok_or("Herdr pane list response is invalid")?
        .iter()
        .map(|value| {
            Ok(HerdrPane {
                id: required_string(value, "pane_id", "Herdr pane id is invalid")?,
                workspace_id: required_string(
                    value,
                    "workspace_id",
                    "Herdr pane workspace id is invalid",
                )?,
                cwd: required_absolute_path(value, "cwd", "Herdr pane cwd is invalid")?,
                label: value
                    .get("label")
                    .and_then(Value::as_str)
                    .and_then(|label| nonempty(Some(label))),
                focused: value
                    .get("focused")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                content_owned: value.get("tokens").and_then(Value::as_object).is_some_and(
                    |tokens| {
                        tokens.contains_key(CONTENT_SOURCE_TOKEN)
                            && tokens.contains_key(CONTENT_CONTROL_TOKEN)
                    },
                ),
            })
        })
        .collect()
}

pub fn opened_pane_id(response: &Value) -> Option<&str> {
    [
        "/result/plugin_pane/pane/pane_id",
        "/result/plugin_pane/pane_id",
        "/result/pane/pane_id",
    ]
    .into_iter()
    .find_map(|pointer| response.pointer(pointer).and_then(Value::as_str))
    .filter(|value| !value.trim().is_empty())
}

pub fn matching_workspace(
    remembered: &RememberedWorkspace,
    panes: &[HerdrPane],
) -> Option<HerdrWorkspace> {
    let pane = remembered
        .workspace_id
        .as_deref()
        .and_then(|id| {
            panes.iter().find(|pane| {
                pane.workspace_id == id && path_is_within(&pane.cwd, &remembered.workspace_cwd)
            })
        })
        .or_else(|| {
            panes
                .iter()
                .find(|pane| path_is_within(&pane.cwd, &remembered.workspace_cwd))
        })?;
    Some(HerdrWorkspace {
        id: pane.workspace_id.clone(),
    })
}

pub fn perforce_pane_candidates<'a>(
    remembered: &RememberedWorkspace,
    workspace: &HerdrWorkspace,
    panes: &'a [HerdrPane],
) -> Vec<&'a HerdrPane> {
    panes
        .iter()
        .filter(|pane| is_perforce_pane(remembered, workspace, pane))
        .collect()
}

pub fn pane_process_is_active(response: &Value) -> Result<bool, &'static str> {
    let process_info = response
        .pointer("/result/process_info")
        .and_then(Value::as_object)
        .ok_or("Herdr pane process response is invalid")?;
    let processes = match process_info.get("foreground_processes") {
        None => return Ok(false),
        Some(processes) => processes
            .as_array()
            .ok_or("Herdr pane process response is invalid")?,
    };
    Ok(processes.iter().any(is_herdr_p4_pane_process))
}

pub fn target_pane_id<'a>(
    remembered: &RememberedWorkspace,
    workspace: &HerdrWorkspace,
    panes: &'a [HerdrPane],
) -> Option<&'a str> {
    panes
        .iter()
        .filter(|pane| pane.workspace_id == workspace.id)
        .filter(|pane| !is_perforce_pane(remembered, workspace, pane))
        .filter(|pane| !is_perforce_content_label(pane.label.as_deref()))
        .min_by_key(|pane| !pane.focused)
        .map(|pane| pane.id.as_str())
}

pub fn preferred_perforce_pane<'a>(
    remembered: &RememberedWorkspace,
    panes: &[&'a HerdrPane],
) -> Option<&'a HerdrPane> {
    remembered
        .pane_id
        .as_deref()
        .and_then(|id| panes.iter().copied().find(|pane| pane.id == id))
        .or_else(|| {
            panes
                .iter()
                .copied()
                .find(|pane| paths_equal(&pane.cwd, &remembered.cwd))
        })
        .or_else(|| panes.first().copied())
}

pub fn paths_equal(left: &Path, right: &Path) -> bool {
    let left = strip_verbatim_prefix(left);
    let right = strip_verbatim_prefix(right);
    if left == right {
        return true;
    }
    path_is_within(&left, &right) && path_is_within(&right, &left)
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    let path = strip_verbatim_prefix(path);
    let root = strip_verbatim_prefix(root);
    let mut path_components = path.components();
    for root_component in root.components() {
        let Some(path_component) = path_components.next() else {
            return false;
        };
        let equal = if cfg!(windows) {
            path_component
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&root_component.as_os_str().to_string_lossy())
        } else {
            path_component == root_component
        };
        if !equal {
            return false;
        }
    }
    true
}

pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    match path.to_str() {
        Some(raw) => PathBuf::from(raw.strip_prefix(r"\\?\").unwrap_or(raw)),
        None => path.to_path_buf(),
    }
}

fn is_perforce_pane(
    _remembered: &RememberedWorkspace,
    workspace: &HerdrWorkspace,
    pane: &HerdrPane,
) -> bool {
    pane.workspace_id == workspace.id && is_perforce_navigation_label(pane.label.as_deref())
}

/// Startup restore plan for one remembered workspace.
///
/// Herdr session restore keeps pane slots and labels, but the process inside
/// is a fresh shell. A `Perforce` title without a live `herdr-p4 pane` process
/// is a corpse: close it, then open a real plugin pane. Never treat the empty
/// shell as already restored — that leaves a Terminal where the TUI should be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreDecision {
    pub healthy_nav_ids: Vec<String>,
    pub unknown_nav_ids: Vec<String>,
    pub stale_nav_ids: Vec<String>,
    pub leftover_content_ids: Vec<String>,
    pub open_target: Option<String>,
}

impl RestoreDecision {
    pub fn should_open_new(&self) -> bool {
        self.healthy_nav_ids.is_empty() && self.unknown_nav_ids.is_empty()
    }
}

pub fn restore_decision(
    remembered: &RememberedWorkspace,
    workspace: &HerdrWorkspace,
    panes: &[HerdrPane],
    healthy_nav_ids: &[String],
    unknown_nav_ids: &[String],
) -> RestoreDecision {
    let nav = perforce_pane_candidates(remembered, workspace, panes);
    let mut leftover_content_ids = Vec::new();
    for pane in perforce_content_panes(workspace, panes) {
        push_unique(&mut leftover_content_ids, pane.id.clone());
    }
    let mut healthy = Vec::new();
    let mut unknown = Vec::new();
    let mut stale = Vec::new();
    for pane in nav {
        if healthy_nav_ids.iter().any(|id| id == &pane.id) {
            push_unique(&mut healthy, pane.id.clone());
            continue;
        }
        if unknown_nav_ids.iter().any(|id| id == &pane.id) {
            push_unique(&mut unknown, pane.id.clone());
            continue;
        }
        push_unique(&mut stale, pane.id.clone());
    }
    RestoreDecision {
        open_target: if healthy.is_empty() && unknown.is_empty() {
            target_pane_id(remembered, workspace, panes).map(ToOwned::to_owned)
        } else {
            None
        },
        healthy_nav_ids: healthy,
        unknown_nav_ids: unknown,
        stale_nav_ids: stale,
        leftover_content_ids,
    }
}

fn push_unique(ids: &mut Vec<String>, id: String) {
    if !ids.iter().any(|existing| existing == &id) {
        ids.push(id);
    }
}

pub fn is_perforce_navigation_label(label: Option<&str>) -> bool {
    label == Some("Perforce")
}

pub fn is_perforce_content_label(label: Option<&str>) -> bool {
    let Some(label) = label else {
        return false;
    };
    label == "Perforce Content" || (label.ends_with(" · Perforce") && label != "Perforce")
}

pub fn perforce_content_panes<'a>(
    workspace: &HerdrWorkspace,
    panes: &'a [HerdrPane],
) -> Vec<&'a HerdrPane> {
    perforce_content_label_panes(workspace, panes)
        .into_iter()
        .filter(|pane| pane.content_owned)
        .collect()
}

pub fn perforce_content_label_panes<'a>(
    workspace: &HerdrWorkspace,
    panes: &'a [HerdrPane],
) -> Vec<&'a HerdrPane> {
    panes
        .iter()
        .filter(|pane| {
            pane.workspace_id == workspace.id && is_perforce_content_label(pane.label.as_deref())
        })
        .collect()
}

fn is_herdr_p4_pane_process(process: &Value) -> bool {
    if is_herdr_p4_binary(process) {
        return process
            .get("argv")
            .and_then(Value::as_array)
            .map(|argv| argv_contains_pane_command(argv))
            .unwrap_or(true);
    }
    powershell_launches_herdr_p4_pane(process)
}

fn is_herdr_p4_binary(process: &Value) -> bool {
    let name = process.get("name").and_then(Value::as_str).unwrap_or("");
    matches_ignore_ascii_case(name, &["herdr-p4", "herdr-p4.exe"])
        || process
            .get("argv0")
            .and_then(Value::as_str)
            .and_then(|argv0| Path::new(argv0).file_name())
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches_ignore_ascii_case(name, &["herdr-p4", "herdr-p4.exe"]))
}

fn argv_contains_pane_command(argv: &[Value]) -> bool {
    argv.iter()
        .filter_map(Value::as_str)
        .any(|argument| argument == "pane")
}

fn powershell_launches_herdr_p4_pane(process: &Value) -> bool {
    if !process_is_powershell(process) {
        return false;
    }
    let mut blobs = Vec::new();
    if let Some(cmdline) = process.get("cmdline").and_then(Value::as_str) {
        blobs.push(cmdline);
    }
    if let Some(argv) = process.get("argv").and_then(Value::as_array) {
        for argument in argv {
            if let Some(value) = argument.as_str() {
                blobs.push(value);
            }
        }
    }
    blobs.iter().any(|blob| command_invokes_herdr_p4_pane(blob))
}

fn process_is_powershell(process: &Value) -> bool {
    let name = process.get("name").and_then(Value::as_str).unwrap_or("");
    let argv0_name = process
        .get("argv0")
        .and_then(Value::as_str)
        .and_then(|argv0| Path::new(argv0).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("");
    matches_ignore_ascii_case(name, &["powershell", "powershell.exe", "pwsh", "pwsh.exe"])
        || matches_ignore_ascii_case(
            argv0_name,
            &["powershell", "powershell.exe", "pwsh", "pwsh.exe"],
        )
}

fn command_invokes_herdr_p4_pane(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    let Some(offset) = lower.find("herdr-p4") else {
        return false;
    };
    let mut rest = &lower[offset + "herdr-p4".len()..];
    if let Some(stripped) = rest.strip_prefix(".exe") {
        rest = stripped;
    }
    rest = rest.trim_start_matches(|character: char| {
        matches!(
            character,
            '"' | '\'' | ')' | '(' | '\\' | ' ' | '\t' | '\r' | '\n'
        )
    });
    rest == "pane"
        || rest.starts_with("pane ")
        || rest.starts_with("pane\"")
        || rest.starts_with("pane'")
        || rest.starts_with("pane\t")
}

fn matches_ignore_ascii_case(value: &str, expected: &[&str]) -> bool {
    expected
        .iter()
        .any(|expected| value.eq_ignore_ascii_case(expected))
}

fn parse_remembered_workspace(value: &Value) -> Result<RememberedWorkspace, &'static str> {
    strict_object(
        value,
        &["cwd", "workspace_cwd", "workspace_id", "pane_id"],
        "remembered workspace is invalid",
    )?;
    let cwd = required_absolute_path(value, "cwd", "remembered workspace cwd is invalid")?;
    Ok(RememberedWorkspace {
        workspace_cwd: match value.get("workspace_cwd") {
            Some(_) => {
                required_absolute_path(value, "workspace_cwd", "Herdr workspace cwd is invalid")?
            }
            None => cwd.clone(),
        },
        cwd,
        workspace_id: optional_string(value, "workspace_id")?,
        pane_id: optional_string(value, "pane_id")?,
    })
}

fn read_bounded_json(
    path: &Path,
    max_bytes: u64,
    read_error: &'static str,
) -> Result<Value, &'static str> {
    let metadata = fs::metadata(path).map_err(|_| read_error)?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Err(read_error);
    }
    let bytes = fs::read(path).map_err(|_| read_error)?;
    serde_json::from_slice(&bytes).map_err(|_| read_error)
}

fn strict_object<'a>(
    value: &'a Value,
    allowed: &[&str],
    error: &'static str,
) -> Result<&'a Map<String, Value>, &'static str> {
    let object = value.as_object().ok_or(error)?;
    object
        .keys()
        .all(|key| allowed.contains(&key.as_str()))
        .then_some(object)
        .ok_or(error)
}

fn required_string(value: &Value, key: &str, error: &'static str) -> Result<String, &'static str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .and_then(|value| nonempty(Some(value)))
        .ok_or(error)
}

fn optional_string(value: &Value, key: &str) -> Result<Option<String>, &'static str> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => nonempty(Some(value))
            .map(Some)
            .ok_or("remembered workspace identifier is invalid"),
        _ => Err("remembered workspace identifier is invalid"),
    }
}

fn required_absolute_path(
    value: &Value,
    key: &str,
    error: &'static str,
) -> Result<PathBuf, &'static str> {
    let path = value
        .get(key)
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .map(|path| strip_verbatim_prefix(&path))
        .ok_or(error)?;
    path.is_absolute().then_some(path).ok_or(error)
}

fn nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("herdr-p4-panel-{name}-{}", std::process::id()));
        fs::remove_dir_all(&path).ok();
        fs::create_dir_all(&path).expect("temp dir");
        path
    }

    #[test]
    fn absent_config_defaults_to_remembered_and_manual_is_explicit() {
        let root = temp_dir("config");
        assert_eq!(
            load_panel_open_mode(Some(&root)),
            Ok(PanelOpenMode::Remembered)
        );
        fs::write(root.join(CONFIG_FILE), br#"{"open_mode":"manual"}"#).expect("config");
        assert_eq!(load_panel_open_mode(Some(&root)), Ok(PanelOpenMode::Manual));
        assert_eq!(
            load_panel_config(Some(&root))
                .expect("config")
                .diff_fold_context,
            DEFAULT_FOLD_CONTEXT
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn config_accepts_diff_fold_context_and_rejects_out_of_range() {
        let root = temp_dir("fold-config");
        fs::write(
            root.join(CONFIG_FILE),
            br#"{"open_mode":"remembered","diff_fold_context":8}"#,
        )
        .expect("config");
        let config = load_panel_config(Some(&root)).expect("fold");
        assert_eq!(config.open_mode, PanelOpenMode::Remembered);
        assert_eq!(config.diff_fold_context, 8);
        fs::write(
            root.join(CONFIG_FILE),
            br#"{"open_mode":"remembered","diff_fold_context":0}"#,
        )
        .expect("config");
        assert_eq!(
            load_panel_config(Some(&root))
                .expect("zero")
                .diff_fold_context,
            0
        );
        fs::write(
            root.join(CONFIG_FILE),
            br#"{"open_mode":"remembered","diff_fold_context":201}"#,
        )
        .expect("config");
        assert!(load_panel_config(Some(&root)).is_err());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn config_rejects_unknown_modes_and_fields() {
        let root = temp_dir("bad-config");
        fs::write(root.join(CONFIG_FILE), br#"{"open_mode":"detected"}"#).expect("config");
        assert!(load_panel_open_mode(Some(&root)).is_err());
        fs::write(
            root.join(CONFIG_FILE),
            br#"{"open_mode":"remembered","command":"ignored.exe"}"#,
        )
        .expect("config");
        assert!(load_panel_open_mode(Some(&root)).is_err());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn state_upserts_by_path_and_keeps_only_plugin_owned_data() {
        let root = temp_dir("state");
        let workspace = root.join("ExampleWorkspace");
        fs::create_dir_all(&workspace).expect("workspace");
        remember_workspace(&root, &workspace, &workspace, Some("w1"), Some("w1:p2"))
            .expect("remember");
        remember_workspace(&root, &workspace, &workspace, Some("w2"), Some("w2:p3"))
            .expect("update");
        let entries = load_remembered_workspaces(Some(&root)).expect("state");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].workspace_id.as_deref(), Some("w2"));
        assert_eq!(entries[0].pane_id.as_deref(), Some("w2:p3"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn corrupt_state_fails_closed_instead_of_being_overwritten() {
        let root = temp_dir("corrupt-state");
        let workspace = root.join("ExampleWorkspace");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::write(root.join(STATE_FILE), b"not-json").expect("bad state");
        assert!(remember_workspace(&root, &workspace, &workspace, Some("w1"), None).is_err());
        assert_eq!(fs::read(root.join(STATE_FILE)).expect("state"), b"not-json");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn restore_decision_matches_cwd_and_skips_an_existing_perforce_pane() {
        let remembered = RememberedWorkspace {
            cwd: PathBuf::from(r"C:\ExampleWorkspace"),
            workspace_cwd: PathBuf::from(r"C:\ExampleWorkspace"),
            workspace_id: Some("old-id".to_owned()),
            pane_id: Some("old-pane".to_owned()),
        };
        let panes = vec![HerdrPane {
            id: "w1:p4".to_owned(),
            workspace_id: "w1".to_owned(),
            cwd: PathBuf::from(r"C:\ExampleWorkspace"),
            label: Some("Perforce".to_owned()),
            focused: false,
            content_owned: false,
        }];
        let workspace = matching_workspace(&remembered, &panes).expect("cwd match");
        assert_eq!(
            perforce_pane_candidates(&remembered, &workspace, &panes).len(),
            1
        );
        assert_eq!(target_pane_id(&remembered, &workspace, &panes), None);
    }

    #[test]
    fn restore_targets_the_focused_non_plugin_pane() {
        let remembered = RememberedWorkspace {
            cwd: PathBuf::from(r"C:\ExampleWorkspace"),
            workspace_cwd: PathBuf::from(r"C:\ExampleWorkspace"),
            workspace_id: Some("w1".to_owned()),
            pane_id: None,
        };
        let workspace = HerdrWorkspace {
            id: "w1".to_owned(),
        };
        let panes = vec![
            HerdrPane {
                id: "w1:p1".to_owned(),
                workspace_id: "w1".to_owned(),
                cwd: remembered.cwd.clone(),
                label: Some("1".to_owned()),
                focused: false,
                content_owned: false,
            },
            HerdrPane {
                id: "w1:p2".to_owned(),
                workspace_id: "w1".to_owned(),
                cwd: remembered.cwd.clone(),
                label: Some("Agent".to_owned()),
                focused: true,
                content_owned: false,
            },
        ];
        assert_eq!(
            target_pane_id(&remembered, &workspace, &panes),
            Some("w1:p2")
        );
    }

    #[test]
    fn nested_p4_cwd_uses_the_separate_herdr_workspace_boundary() {
        let workspace_root = if cfg!(windows) {
            PathBuf::from(r"C:\ExampleWorkspace")
        } else {
            PathBuf::from("/ExampleWorkspace")
        };
        let remembered = RememberedWorkspace {
            cwd: workspace_root.join("Source"),
            workspace_cwd: workspace_root.clone(),
            workspace_id: Some("w1".to_owned()),
            pane_id: None,
        };
        let panes = vec![HerdrPane {
            id: "w1:p1".to_owned(),
            workspace_id: "w1".to_owned(),
            cwd: workspace_root.join("Build"),
            label: Some("Agent".to_owned()),
            focused: true,
            content_owned: false,
        }];
        let workspace = matching_workspace(&remembered, &panes).expect("workspace root match");
        assert_eq!(workspace.id, "w1");
        assert_eq!(
            target_pane_id(&remembered, &workspace, &panes),
            Some("w1:p1")
        );
        assert!(perforce_pane_candidates(&remembered, &workspace, &panes).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_matching_ignores_case_verbatim_prefix_and_trailing_separator() {
        assert!(paths_equal(
            Path::new(r"\\?\C:\ExampleWorkspace\"),
            Path::new(r"c:\exampleworkspace")
        ));
        assert!(path_is_within(
            Path::new(r"C:\EXAMPLEWORKSPACE\Source"),
            Path::new(r"c:\exampleworkspace")
        ));
        assert!(!path_is_within(
            Path::new(r"C:\ExampleWorkspace2\Source"),
            Path::new(r"C:\ExampleWorkspace")
        ));
    }

    #[test]
    fn parses_current_herdr_pane_shape_and_derives_the_workspace() {
        let pane_response = json!({
            "result": { "panes": [{
                "pane_id": "w1:p1",
                "workspace_id": "w1",
                "cwd": r"C:\ExampleWorkspace",
                "label": "Agent",
                "focused": true
            }]}
        });
        let panes = parse_pane_list(&pane_response).unwrap();
        assert!(!panes[0].content_owned);
        let remembered = RememberedWorkspace {
            cwd: PathBuf::from(r"C:\ExampleWorkspace"),
            workspace_cwd: PathBuf::from(r"C:\ExampleWorkspace"),
            workspace_id: Some("w1".to_owned()),
            pane_id: None,
        };
        assert_eq!(
            matching_workspace(&remembered, &panes),
            Some(HerdrWorkspace {
                id: "w1".to_owned(),
            })
        );
    }

    #[test]
    fn content_ownership_requires_both_plugin_metadata_tokens() {
        let response = json!({
            "result": { "panes": [
                {
                    "pane_id": "w1:p1",
                    "workspace_id": "w1",
                    "cwd": r"C:\ExampleWorkspace",
                    "label": "Diff · file.rs · Perforce",
                    "tokens": {
                        CONTENT_SOURCE_TOKEN: "alive",
                        CONTENT_CONTROL_TOKEN: "control.json"
                    }
                },
                {
                    "pane_id": "w1:p2",
                    "workspace_id": "w1",
                    "cwd": r"C:\ExampleWorkspace",
                    "label": "Diff · unrelated Agent work · Perforce"
                }
            ]}
        });
        let panes = parse_pane_list(&response).expect("panes");
        assert!(panes[0].content_owned);
        assert!(!panes[1].content_owned);
    }

    #[test]
    fn reads_opened_pane_id_from_supported_response_shapes() {
        let response = json!({
            "result": { "plugin_pane": { "pane": { "pane_id": "w1:p7" } } }
        });
        assert_eq!(opened_pane_id(&response), Some("w1:p7"));
    }

    #[test]
    fn pane_health_requires_a_running_plugin_pane_process() {
        let active = json!({
            "result": { "process_info": { "foreground_processes": [
                { "pid": 10, "name": "powershell.exe" },
                {
                    "pid": 11,
                    "name": "herdr-p4.exe",
                    "argv": [r"C:\Plugin\herdr-p4.exe", "pane"]
                }
            ]}}
        });
        let stale_shell = json!({
            "result": { "process_info": { "foreground_processes": [
                { "pid": 10, "name": "powershell.exe" }
            ]}}
        });
        assert_eq!(pane_process_is_active(&active), Ok(true));
        assert_eq!(pane_process_is_active(&stale_shell), Ok(false));
        assert_eq!(
            pane_process_is_active(&json!({
                "result": { "process_info": { "pane_id": "w1:p1" } }
            })),
            Ok(false)
        );
        assert!(pane_process_is_active(&json!({ "result": {} })).is_err());
    }

    #[test]
    fn unrelated_herdr_p4_subcommand_is_not_a_healthy_pane() {
        let response = json!({
            "result": { "process_info": { "foreground_processes": [{
                "pid": 11,
                "name": "herdr-p4.exe",
                "argv": [r"C:\Plugin\herdr-p4.exe", "level-b", "--read-only"]
            }]}}
        });
        assert_eq!(pane_process_is_active(&response), Ok(false));
    }

    #[test]
    fn windows_powershell_wrapper_running_pane_is_healthy() {
        let command = r#"$root = $env:HERDR_PLUGIN_ROOT; if (-not $root) { Write-Error "HERDR_PLUGIN_ROOT is missing"; exit 69 }; if ($root.StartsWith('\\?\')) { $root = $root.Substring(4) }; & (Join-Path $root "target\release\herdr-p4.exe") pane"#;
        let response = json!({
            "result": { "process_info": { "foreground_processes": [{
                "pid": 620,
                "name": "powershell.exe",
                "argv0": r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.EXE",
                "argv": [
                    r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.EXE",
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    command
                ],
                "cmdline": format!("powershell.EXE -NoProfile -ExecutionPolicy Bypass -Command \"{command}\"")
            }]}}
        });
        assert_eq!(pane_process_is_active(&response), Ok(true));
    }

    #[test]
    fn windows_powershell_restore_hook_is_not_a_healthy_pane() {
        let command = r#"$root = $env:HERDR_PLUGIN_ROOT; & (Join-Path $root "target\release\herdr-p4.exe") restore-panes"#;
        let response = json!({
            "result": { "process_info": { "foreground_processes": [{
                "pid": 8,
                "name": "powershell.exe",
                "argv": ["powershell.exe", "-Command", command]
            }]}}
        });
        assert_eq!(pane_process_is_active(&response), Ok(false));
    }

    #[test]
    fn load_coalesces_duplicate_workspace_ids_from_disk() {
        let root = temp_dir("coalesce-disk");
        let parent = root.join("Neon");
        let child = parent.join("NeonGame");
        fs::create_dir_all(&child).expect("workspace");
        let value = json!({
            "version": 1,
            "workspaces": [
                {
                    "cwd": parent,
                    "workspace_cwd": parent,
                    "workspace_id": "w6",
                    "pane_id": "w6:p8"
                },
                {
                    "cwd": child,
                    "workspace_cwd": child,
                    "workspace_id": "w6",
                    "pane_id": "w6:p9"
                }
            ]
        });
        fs::write(
            root.join(STATE_FILE),
            serde_json::to_vec_pretty(&value).expect("encode"),
        )
        .expect("state");
        let entries = load_remembered_workspaces(Some(&root)).expect("state");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pane_id.as_deref(), Some("w6:p9"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn same_workspace_id_with_nested_cwds_is_one_remembered_entry() {
        let root = temp_dir("same-workspace");
        let parent = root.join("Neon");
        let child = parent.join("NeonGame");
        fs::create_dir_all(&child).expect("workspace");
        remember_workspace(&root, &parent, &parent, Some("w6"), Some("w6:p8")).expect("parent");
        remember_workspace(&root, &child, &child, Some("w6"), Some("w6:p9")).expect("child");
        let entries = load_remembered_workspaces(Some(&root)).expect("state");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].cwd, child);
        assert_eq!(entries[0].pane_id.as_deref(), Some("w6:p9"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn content_panes_are_not_navigation_candidates() {
        assert!(is_perforce_navigation_label(Some("Perforce")));
        assert!(!is_perforce_content_label(Some("Perforce")));
        assert!(is_perforce_content_label(Some("Perforce Content")));
        assert!(is_perforce_content_label(Some(
            "Diff · NeonPhysicsQueryWorldSubsystem.cpp · Perforce"
        )));
        assert!(!is_perforce_content_label(Some(
            "Diff · unrelated Agent work"
        )));
        let remembered = RememberedWorkspace {
            cwd: PathBuf::from(r"E:\Project\NeonGame"),
            workspace_cwd: PathBuf::from(r"E:\Project\NeonGame"),
            workspace_id: Some("w1".to_owned()),
            pane_id: Some("w1:p1V".to_owned()),
        };
        let workspace = HerdrWorkspace {
            id: "w1".to_owned(),
        };
        let panes = vec![
            HerdrPane {
                id: "w1:p1".to_owned(),
                workspace_id: "w1".to_owned(),
                cwd: PathBuf::from(r"E:\Project\NeonGame"),
                label: Some("Agent".to_owned()),
                focused: true,
                content_owned: false,
            },
            HerdrPane {
                id: "w1:p1W".to_owned(),
                workspace_id: "w1".to_owned(),
                cwd: PathBuf::from(r"E:\Project\NeonGame"),
                label: Some("Diff · NeonPhysicsQueryWorldSubsystem.cpp · Perforce".to_owned()),
                focused: false,
                content_owned: true,
            },
            HerdrPane {
                id: "w1:p1V".to_owned(),
                workspace_id: "w1".to_owned(),
                cwd: PathBuf::from(r"E:\Project\NeonGame\"),
                label: Some("Perforce".to_owned()),
                focused: false,
                content_owned: false,
            },
        ];
        let nav = perforce_pane_candidates(&remembered, &workspace, &panes);
        assert_eq!(nav.len(), 1);
        assert_eq!(nav[0].id, "w1:p1V");
        let content = perforce_content_panes(&workspace, &panes);
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].id, "w1:p1W");

        let mut spoofed = panes[0].clone();
        spoofed.label = Some("Diff · unrelated Agent work · Perforce".to_owned());
        spoofed.content_owned = false;
        assert!(perforce_content_panes(&workspace, &[spoofed.clone()]).is_empty());
        assert_eq!(
            perforce_content_label_panes(&workspace, &[spoofed]).len(),
            1
        );
        assert_eq!(
            target_pane_id(&remembered, &workspace, &panes),
            Some("w1:p1")
        );
    }

    #[test]
    fn perforce_candidates_match_by_workspace_and_label_not_exact_cwd() {
        let remembered = RememberedWorkspace {
            cwd: PathBuf::from(r"G:\Projects\Neon\NeonGame"),
            workspace_cwd: PathBuf::from(r"G:\Projects\Neon\NeonGame"),
            workspace_id: Some("w6".to_owned()),
            pane_id: Some("w6:p9".to_owned()),
        };
        let workspace = HerdrWorkspace {
            id: "w6".to_owned(),
        };
        let panes = vec![
            HerdrPane {
                id: "w6:p8".to_owned(),
                workspace_id: "w6".to_owned(),
                cwd: PathBuf::from(r"G:\Projects\Neon\"),
                label: Some("Perforce".to_owned()),
                focused: false,
                content_owned: false,
            },
            HerdrPane {
                id: "w6:p9".to_owned(),
                workspace_id: "w6".to_owned(),
                cwd: PathBuf::from(r"G:\Projects\Neon\NeonGame\"),
                label: Some("Perforce".to_owned()),
                focused: false,
                content_owned: false,
            },
            HerdrPane {
                id: "w6:p1".to_owned(),
                workspace_id: "w6".to_owned(),
                cwd: PathBuf::from(r"G:\Projects\Neon\NeonGame"),
                label: Some("Agent".to_owned()),
                focused: true,
                content_owned: false,
            },
        ];
        let candidates = perforce_pane_candidates(&remembered, &workspace, &panes);
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            preferred_perforce_pane(&remembered, &candidates).map(|pane| pane.id.as_str()),
            Some("w6:p9")
        );
        assert_eq!(
            target_pane_id(&remembered, &workspace, &panes),
            Some("w6:p1")
        );
    }

    #[test]
    fn navigator_share_round_trips_and_rejects_out_of_range() {
        let root = temp_dir("layout-share");
        let neon = PathBuf::from(r"E:\Project\NeonGame");
        let quill = PathBuf::from(r"D:\Projects\Quill");
        assert_eq!(
            load_navigator_share(Some(&root), &neon),
            DEFAULT_NAVIGATION_SHARE
        );
        assert_eq!(workspace_layout_exists(&root, &neon), Ok(false));
        save_navigator_share(&root, &neon, 0.12).expect("save");
        save_navigator_share(&root, &quill, 0.27).expect("save");
        assert_eq!(workspace_layout_exists(&root, &neon), Ok(true));
        assert!((load_navigator_share(Some(&root), &neon) - 0.12).abs() < f64::EPSILON);
        assert!((load_navigator_share(Some(&root), &quill) - 0.27).abs() < f64::EPSILON);
        assert!(save_navigator_share(&root, &neon, 0.5).is_err());
        assert!(save_navigator_share(&root, &neon, 0.01).is_ok());
        assert!((load_navigator_share(Some(&root), &neon) - 0.08).abs() < f64::EPSILON);
        save_content_request(
            &root,
            &neon,
            Some(r#"{"version":1,"kind":"file","path":"E:\\Project\\NeonGame\\Neon.uproject"}"#),
        )
        .expect("content request");
        assert!(load_content_request(Some(&root), &neon).is_some());
        assert_eq!(load_content_request(Some(&root), &quill), None);
        save_navigation_view(&root, &neon, "review").expect("navigation view");
        save_navigation_view(&root, &quill, "explorer").expect("navigation view");
        assert_eq!(
            load_navigation_view(Some(&root), &neon).as_deref(),
            Some("review")
        );
        assert_eq!(
            load_navigation_view(Some(&root), &quill).as_deref(),
            Some("explorer")
        );
        fs::write(
            root.join(LAYOUT_FILE),
            br#"{"version":1,"navigator_share":0.9}"#,
        )
        .expect("layout");
        let other = PathBuf::from(r"G:\Projects\Other");
        assert_eq!(
            load_navigator_share(Some(&root), &other),
            DEFAULT_NAVIGATION_SHARE
        );
        fs::remove_dir_all(root).ok();
    }

    fn neon_game_panes() -> (RememberedWorkspace, HerdrWorkspace, Vec<HerdrPane>) {
        let remembered = RememberedWorkspace {
            cwd: PathBuf::from(r"E:\Project\NeonGame"),
            workspace_cwd: PathBuf::from(r"E:\Project\NeonGame"),
            workspace_id: Some("w1".to_owned()),
            pane_id: Some("w1:p1X".to_owned()),
        };
        let workspace = HerdrWorkspace {
            id: "w1".to_owned(),
        };
        let panes = vec![
            HerdrPane {
                id: "w1:p1".to_owned(),
                workspace_id: "w1".to_owned(),
                cwd: PathBuf::from(r"E:\Project\NeonGame"),
                label: Some("Agent".to_owned()),
                focused: true,
                content_owned: false,
            },
            HerdrPane {
                id: "w1:p1X".to_owned(),
                workspace_id: "w1".to_owned(),
                cwd: PathBuf::from(r"E:\Project\NeonGame"),
                label: Some("Perforce".to_owned()),
                focused: false,
                content_owned: false,
            },
            HerdrPane {
                id: "w1:p1W".to_owned(),
                workspace_id: "w1".to_owned(),
                cwd: PathBuf::from(r"E:\Project\NeonGame"),
                label: Some("Diff · NeonPhysicsQueryWorldSubsystem.cpp · Perforce".to_owned()),
                focused: false,
                content_owned: true,
            },
        ];
        (remembered, workspace, panes)
    }

    #[test]
    fn label_only_shell_is_a_corpse_and_must_be_replaced() {
        let (remembered, workspace, panes) = neon_game_panes();
        let decision = restore_decision(&remembered, &workspace, &panes, &[], &[]);
        assert!(decision.should_open_new());
        assert_eq!(decision.open_target.as_deref(), Some("w1:p1"));
        assert_eq!(decision.stale_nav_ids, ["w1:p1X"]);
        assert_eq!(decision.leftover_content_ids, ["w1:p1W"]);
        assert!(!decision.stale_nav_ids.iter().any(|id| id == "w1:p1"));
        assert!(decision.healthy_nav_ids.is_empty());
    }

    #[test]
    fn live_plugin_process_is_kept_and_corpses_are_closed() {
        let (remembered, workspace, panes) = neon_game_panes();
        let decision =
            restore_decision(&remembered, &workspace, &panes, &["w1:p1X".to_owned()], &[]);
        assert!(!decision.should_open_new());
        assert_eq!(decision.healthy_nav_ids, ["w1:p1X"]);
        assert_eq!(decision.leftover_content_ids, ["w1:p1W"]);
        assert!(decision.stale_nav_ids.is_empty());
        assert_eq!(decision.open_target, None);
    }

    #[test]
    fn remembered_pane_id_does_not_mark_an_agent_pane_as_perforce() {
        let remembered = RememberedWorkspace {
            cwd: PathBuf::from(r"E:\Project\NeonGame"),
            workspace_cwd: PathBuf::from(r"E:\Project\NeonGame"),
            workspace_id: Some("w1".to_owned()),
            pane_id: Some("w1:p1".to_owned()),
        };
        let workspace = HerdrWorkspace {
            id: "w1".to_owned(),
        };
        let panes = vec![HerdrPane {
            id: "w1:p1".to_owned(),
            workspace_id: "w1".to_owned(),
            cwd: PathBuf::from(r"E:\Project\NeonGame"),
            label: Some("Agent".to_owned()),
            focused: true,
            content_owned: false,
        }];
        assert!(perforce_pane_candidates(&remembered, &workspace, &panes).is_empty());
        let decision = restore_decision(&remembered, &workspace, &panes, &[], &[]);
        assert!(decision.should_open_new());
        assert_eq!(decision.open_target.as_deref(), Some("w1:p1"));
        assert!(decision.stale_nav_ids.is_empty());
        assert!(decision.leftover_content_ids.is_empty());
    }

    #[test]
    fn two_shell_corpses_are_both_closed_before_opening() {
        let remembered = RememberedWorkspace {
            cwd: PathBuf::from(r"E:\Project\NeonGame"),
            workspace_cwd: PathBuf::from(r"E:\Project\NeonGame"),
            workspace_id: Some("w1".to_owned()),
            pane_id: Some("w1:p1X".to_owned()),
        };
        let workspace = HerdrWorkspace {
            id: "w1".to_owned(),
        };
        let panes = vec![
            HerdrPane {
                id: "w1:p1".to_owned(),
                workspace_id: "w1".to_owned(),
                cwd: PathBuf::from(r"E:\Project\NeonGame"),
                label: Some("Agent".to_owned()),
                focused: true,
                content_owned: false,
            },
            HerdrPane {
                id: "w1:p1X".to_owned(),
                workspace_id: "w1".to_owned(),
                cwd: PathBuf::from(r"E:\Project\NeonGame"),
                label: Some("Perforce".to_owned()),
                focused: false,
                content_owned: false,
            },
            HerdrPane {
                id: "w1:p1Y".to_owned(),
                workspace_id: "w1".to_owned(),
                cwd: PathBuf::from(r"E:\Project\NeonGame"),
                label: Some("Perforce".to_owned()),
                focused: false,
                content_owned: false,
            },
        ];
        let decision = restore_decision(&remembered, &workspace, &panes, &[], &[]);
        assert!(decision.should_open_new());
        assert_eq!(decision.open_target.as_deref(), Some("w1:p1"));
        assert_eq!(decision.stale_nav_ids.len(), 2);
        assert!(decision.stale_nav_ids.contains(&"w1:p1X".to_owned()));
        assert!(decision.stale_nav_ids.contains(&"w1:p1Y".to_owned()));
    }

    #[test]
    fn unknown_health_does_not_close_or_replace_the_pane() {
        let (remembered, workspace, panes) = neon_game_panes();
        let decision =
            restore_decision(&remembered, &workspace, &panes, &[], &["w1:p1X".to_owned()]);
        assert!(!decision.should_open_new());
        assert_eq!(decision.unknown_nav_ids, ["w1:p1X"]);
        assert!(decision.stale_nav_ids.is_empty());
        assert_eq!(decision.leftover_content_ids, ["w1:p1W"]);
        assert_eq!(decision.open_target, None);
    }
}
