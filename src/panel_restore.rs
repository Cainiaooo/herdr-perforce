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
const MAX_CONFIG_BYTES: u64 = 16 * 1024;
const MAX_STATE_BYTES: u64 = 64 * 1024;
const MAX_REMEMBERED_WORKSPACES: usize = 128;

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

    workspaces.iter().map(parse_remembered_workspace).collect()
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
    entries.retain(|entry| !paths_equal(&entry.cwd, &cwd));
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
        .min_by_key(|pane| !pane.focused)
        .map(|pane| pane.id.as_str())
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
    remembered: &RememberedWorkspace,
    workspace: &HerdrWorkspace,
    pane: &HerdrPane,
) -> bool {
    pane.workspace_id == workspace.id
        && paths_equal(&pane.cwd, &remembered.cwd)
        && (remembered.pane_id.as_deref() == Some(pane.id.as_str())
            || pane.label.as_deref() == Some("Perforce"))
}

fn is_herdr_p4_pane_process(process: &Value) -> bool {
    let Some(name) = process.get("name").and_then(Value::as_str) else {
        return false;
    };
    let is_binary = matches_ignore_ascii_case(name, &["herdr-p4", "herdr-p4.exe"])
        || process
            .get("argv0")
            .and_then(Value::as_str)
            .and_then(|argv0| Path::new(argv0).file_name())
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches_ignore_ascii_case(name, &["herdr-p4", "herdr-p4.exe"]));
    if !is_binary {
        return false;
    }
    process
        .get("argv")
        .and_then(Value::as_array)
        .map(|argv| {
            argv.iter()
                .filter_map(Value::as_str)
                .any(|arg| arg == "pane")
        })
        .unwrap_or(true)
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
            },
            HerdrPane {
                id: "w1:p2".to_owned(),
                workspace_id: "w1".to_owned(),
                cwd: remembered.cwd.clone(),
                label: Some("Agent".to_owned()),
                focused: true,
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
}
