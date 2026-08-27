//! Directory-scoped P4 settings resolved from the pane cwd.
//!
//! Inherited `P4CLIENT`/`P4PORT`/`P4USER` from the Herdr process describe one
//! default client. Multiple mapped workspaces on the same machine each have
//! their own client, typically recorded in a `p4config.txt` (or `P4CONFIG`)
//! file at or above the workspace root. Those values are overlaid on every
//! `p4` child so a pane uses the client that owns its cwd, not the process
//! default.

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs,
    path::{Component, Path, PathBuf},
};

use crate::domain::{CaseHandling, WorkspaceIdentity};

use super::{
    error::{P4Error, P4ErrorKind, known_error},
    parser::{DomainMappingError, StructuredRecord, workspace_from_info},
};

/// Filenames searched when `P4CONFIG` is unset.
///
/// `p4` itself does not search until `P4CONFIG` is set. Game and Windows Helix
/// trees commonly still place `p4config.txt` at the client root, so the pane
/// honors those files for cwd-scoped identity. That compatibility walk never
/// includes the volume root (`D:\`, `/`), which official `p4` would only
/// consult when `P4CONFIG` is explicitly set.
const DEFAULT_P4CONFIG_NAMES: &[&str] = &["p4config.txt", ".p4config", ".p4config.txt"];
const MAX_DIRECTORY_WALK: usize = 64;

#[derive(Debug)]
pub(crate) enum WorkspaceCwdError {
    Mapping(DomainMappingError),
    Query(P4Error),
}

/// Parses `p4 info` and rejects a client whose Root does not contain `cwd`.
///
/// This is a wrong-client guard, not a `p4 where` view test. Unmapped paths
/// under the same Root are still accepted so Review can list pending CLs.
/// Level B does not use this helper: an unmapped cwd is a completed skip,
/// not an identity query failure.
pub(crate) fn workspace_owning_cwd(
    cwd: &Path,
    records: &[StructuredRecord],
) -> Result<WorkspaceIdentity, WorkspaceCwdError> {
    let workspace = workspace_from_info(records).map_err(WorkspaceCwdError::Mapping)?;
    ensure_cwd_in_client_root(cwd, &workspace).map_err(WorkspaceCwdError::Query)?;
    Ok(workspace)
}

#[must_use]
pub(crate) fn discover_directory_p4_settings(cwd: &Path) -> BTreeMap<OsString, OsString> {
    let configured = std::env::var_os("P4CONFIG");
    let include_volume_root = configured.as_ref().is_some_and(|name| !name.is_empty());
    discover_from_ancestors(
        cwd,
        &p4config_file_names_from(configured),
        include_volume_root,
    )
}

#[must_use]
fn p4config_file_names_from(p4config: Option<OsString>) -> Vec<OsString> {
    match p4config {
        Some(name) if !name.is_empty() => vec![name],
        _ => DEFAULT_P4CONFIG_NAMES.iter().map(OsString::from).collect(),
    }
}

#[must_use]
fn discover_from_ancestors(
    cwd: &Path,
    names: &[OsString],
    include_volume_root: bool,
) -> BTreeMap<OsString, OsString> {
    let mut settings = BTreeMap::new();
    if names.is_empty() {
        return settings;
    }

    if names.len() == 1 {
        let configured = Path::new(&names[0]);
        if configured.is_absolute() {
            merge_p4config_file(&mut settings, configured);
            return settings;
        }
    }

    for directory in existing_directories_from(cwd, include_volume_root) {
        for name in names {
            if Path::new(name).is_absolute() {
                continue;
            }
            let candidate = directory.join(name);
            if candidate.is_file() {
                merge_p4config_file(&mut settings, &candidate);
            }
        }
    }
    settings
}

fn cwd_maps_into_client_root(cwd: &Path, identity: &WorkspaceIdentity) -> bool {
    let ignore_case = path_comparison_is_case_insensitive(&identity.case_handling);
    if path_is_within_root(cwd, &identity.root, ignore_case) {
        return true;
    }

    let Ok(canonical_cwd) = fs::canonicalize(cwd) else {
        return false;
    };
    let Ok(canonical_root) = fs::canonicalize(&identity.root) else {
        return false;
    };
    path_is_within_root(&canonical_cwd, &canonical_root, ignore_case)
}

pub(crate) fn ensure_cwd_in_client_root(
    cwd: &Path,
    identity: &WorkspaceIdentity,
) -> Result<(), P4Error> {
    if cwd_maps_into_client_root(cwd, identity) {
        Ok(())
    } else {
        Err(known_error(P4ErrorKind::NotInClientView))
    }
}

fn path_comparison_is_case_insensitive(case_handling: &CaseHandling) -> bool {
    if cfg!(windows) {
        return true;
    }
    matches!(
        case_handling,
        CaseHandling::Insensitive | CaseHandling::Hybrid
    )
}

fn merge_p4config_file(settings: &mut BTreeMap<OsString, OsString>, path: &Path) {
    let Ok(bytes) = fs::read(path) else {
        return;
    };
    merge_missing(settings, parse_p4config_bytes(&bytes));
}

fn merge_missing(
    settings: &mut BTreeMap<OsString, OsString>,
    parsed: BTreeMap<OsString, OsString>,
) {
    for (key, value) in parsed {
        settings.entry(key).or_insert(value);
    }
}

fn parse_p4config_bytes(bytes: &[u8]) -> BTreeMap<OsString, OsString> {
    let mut settings = BTreeMap::new();
    let Ok(text) = std::str::from_utf8(bytes) else {
        return settings;
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    for line in text.lines() {
        let line = strip_shell_assignment_prefix(line.trim());
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !is_p4_variable(key) || key.eq_ignore_ascii_case("P4CONFIG") {
            continue;
        }
        settings
            .entry(OsString::from(key.to_ascii_uppercase()))
            .or_insert_with(|| OsString::from(unquote_p4_value(value)));
    }
    settings
}

fn strip_shell_assignment_prefix(line: &str) -> &str {
    let Some((word, rest)) = line.split_once(char::is_whitespace) else {
        return line;
    };
    if word.eq_ignore_ascii_case("set") || word.eq_ignore_ascii_case("export") {
        rest.trim()
    } else {
        line
    }
}

fn unquote_p4_value(value: &str) -> String {
    let value = value.trim();
    let bytes = value.as_bytes();
    let last = bytes.last().copied();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && last == Some(b'"')) || (bytes[0] == b'\'' && last == Some(b'\'')))
    {
        value[1..value.len() - 1].to_owned()
    } else {
        value.to_owned()
    }
}

fn is_p4_variable(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(
        (characters.next(), characters.next()),
        (Some('P') | Some('p'), Some('4'))
    ) && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn existing_directories_from(cwd: &Path, include_volume_root: bool) -> Vec<PathBuf> {
    let cwd = strip_verbatim_prefix(cwd);
    if !cwd.is_dir() {
        return Vec::new();
    }

    let mut directories = Vec::new();
    let mut current = cwd;
    loop {
        let volume_root = is_volume_root(&current);
        if volume_root && !include_volume_root && !directories.is_empty() {
            break;
        }
        directories.push(current.clone());
        if directories.len() >= MAX_DIRECTORY_WALK || volume_root {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent.as_os_str().is_empty() || parent == current {
            break;
        }
        if !include_volume_root && is_volume_root(parent) {
            break;
        }
        current = parent.to_path_buf();
        if !current.is_dir() {
            break;
        }
    }
    directories
}

fn is_volume_root(path: &Path) -> bool {
    let components: Vec<_> = path.components().collect();
    matches!(
        components.as_slice(),
        [Component::RootDir] | [Component::Prefix(_)] | [Component::Prefix(_), Component::RootDir]
    )
}

fn path_is_within_root(path: &Path, root: &Path, ignore_case: bool) -> bool {
    let path = strip_verbatim_prefix(path);
    let root = strip_verbatim_prefix(root);
    if root.as_os_str().is_empty() {
        return false;
    }

    let mut path_components = path.components();
    for root_component in root.components() {
        let Some(path_component) = path_components.next() else {
            return false;
        };
        if !components_equal(
            path_component.as_os_str(),
            root_component.as_os_str(),
            ignore_case,
        ) {
            return false;
        }
    }
    true
}

fn components_equal(left: &OsStr, right: &OsStr, ignore_case: bool) -> bool {
    if left == right {
        return true;
    }
    if ignore_case {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        false
    }
}

fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let Some(raw) = path.to_str() else {
        return path.to_path_buf();
    };
    const VERBATIM: &str = r"\\?\";
    const VERBATIM_UNC: &str = r"\\?\UNC\";
    if let Some(rest) = raw.strip_prefix(VERBATIM_UNC) {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    PathBuf::from(raw.strip_prefix(VERBATIM).unwrap_or(raw))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::p4::{P4Query, fake::FakeP4Transport, transport::P4Client};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "herdr-p4-config-{tag}-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).expect("temp tree");
            Self { root }
        }

        fn mkdir(&self, relative: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(&path).expect("nested dir");
            path
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("config parent");
            }
            fs::write(&path, contents).expect("write p4config");
            path
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn names(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn setting<'a>(settings: &'a BTreeMap<OsString, OsString>, key: &str) -> Option<&'a OsStr> {
        settings.get(OsStr::new(key)).map(OsString::as_os_str)
    }

    fn discover_with_names(cwd: &Path, names: &[OsString]) -> BTreeMap<OsString, OsString> {
        discover_from_ancestors(cwd, names, false)
    }

    fn identity_at(root: &str) -> WorkspaceIdentity {
        WorkspaceIdentity {
            server_id: "example-server".into(),
            user: "ExampleUser".into(),
            client: "ExampleClientA".into(),
            root: PathBuf::from(root),
            stream: None,
            case_handling: CaseHandling::Insensitive,
        }
    }

    #[test]
    fn unset_p4config_searches_common_filenames() {
        let names = p4config_file_names_from(None);
        assert_eq!(
            names,
            vec![
                OsString::from("p4config.txt"),
                OsString::from(".p4config"),
                OsString::from(".p4config.txt"),
            ]
        );
    }

    #[test]
    fn configured_p4config_name_is_the_only_search_target() {
        let names = p4config_file_names_from(Some(OsString::from(".p4config")));
        assert_eq!(names, vec![OsString::from(".p4config")]);
        assert!(p4config_file_names_from(Some(OsString::from(""))).len() > 1);
    }

    #[test]
    fn parent_p4config_selects_the_client_for_a_nested_workspace() {
        let tree = TempTree::new("parent");
        tree.write(
            "p4config.txt",
            "P4PORT=ssl:p4.example:1666\nP4USER=ExampleUser\nP4CLIENT=ExampleClientA\n",
        );
        let cwd = tree.mkdir("NeonGame");

        let settings = discover_with_names(&cwd, &names(&["p4config.txt"]));
        assert_eq!(
            setting(&settings, "P4CLIENT"),
            Some(OsStr::new("ExampleClientA"))
        );
        assert_eq!(
            setting(&settings, "P4PORT"),
            Some(OsStr::new("ssl:p4.example:1666"))
        );
        assert_eq!(
            setting(&settings, "P4USER"),
            Some(OsStr::new("ExampleUser"))
        );
    }

    #[test]
    fn sibling_workspace_roots_keep_distinct_clients() {
        let tree = TempTree::new("siblings");
        tree.write(
            "ws-e/p4config.txt",
            "P4CLIENT=ExampleClientA\nP4USER=ExampleUser\n",
        );
        tree.write(
            "ws-g/p4config.txt",
            "P4CLIENT=ExampleClientB\nP4USER=ExampleUser\n",
        );
        let left = tree.mkdir("ws-e/NeonGame");
        let right = tree.mkdir("ws-g/NeonGame");

        let names = names(&["p4config.txt"]);
        assert_eq!(
            setting(&discover_with_names(&left, &names), "P4CLIENT"),
            Some(OsStr::new("ExampleClientA"))
        );
        assert_eq!(
            setting(&discover_with_names(&right, &names), "P4CLIENT"),
            Some(OsStr::new("ExampleClientB"))
        );
    }

    #[test]
    fn closer_p4config_overrides_parent_keys_and_inherits_the_rest() {
        let tree = TempTree::new("merge");
        tree.write(
            "p4config.txt",
            "P4CLIENT=ExampleParent\nP4PORT=ssl:parent.example:1666\nP4USER=ExampleUser\n",
        );
        tree.write("NeonGame/p4config.txt", "P4CLIENT=ExampleChild\n");
        let cwd = tree.mkdir("NeonGame/Source");

        let settings = discover_with_names(&cwd, &names(&["p4config.txt"]));
        assert_eq!(
            setting(&settings, "P4CLIENT"),
            Some(OsStr::new("ExampleChild"))
        );
        assert_eq!(
            setting(&settings, "P4PORT"),
            Some(OsStr::new("ssl:parent.example:1666"))
        );
        assert_eq!(
            setting(&settings, "P4USER"),
            Some(OsStr::new("ExampleUser"))
        );
    }

    #[test]
    fn same_directory_candidate_names_fill_missing_keys() {
        let tree = TempTree::new("same-dir");
        tree.write("p4config.txt", "P4CLIENT=FromTxt\n");
        tree.write(".p4config", "P4CLIENT=FromDot\nP4PORT=ssl:example:1666\n");
        let cwd = tree.mkdir("Source");

        let settings = discover_with_names(&cwd, &names(&["p4config.txt", ".p4config"]));
        assert_eq!(setting(&settings, "P4CLIENT"), Some(OsStr::new("FromTxt")));
        assert_eq!(
            setting(&settings, "P4PORT"),
            Some(OsStr::new("ssl:example:1666"))
        );
    }

    #[test]
    fn non_p4_keys_and_comments_are_ignored() {
        let parsed = parse_p4config_bytes(
            b"# comment\nPATH=C:\\\\Windows\nHERDR_SOCKET=secret\nP4CLIENT=ExampleClientA\n\nP4CONFIG=other\n",
        );
        assert_eq!(
            setting(&parsed, "P4CLIENT"),
            Some(OsStr::new("ExampleClientA"))
        );
        assert!(setting(&parsed, "PATH").is_none());
        assert!(setting(&parsed, "HERDR_SOCKET").is_none());
        assert!(setting(&parsed, "P4CONFIG").is_none());
    }

    #[test]
    fn set_export_and_quoted_values_are_accepted() {
        let parsed = parse_p4config_bytes(
            b"set P4CLIENT=\"Example Client\"\nexport P4USER=ExampleUser\nP4PORT=ssl:example:1666\n",
        );
        assert_eq!(
            setting(&parsed, "P4CLIENT"),
            Some(OsStr::new("Example Client"))
        );
        assert_eq!(setting(&parsed, "P4USER"), Some(OsStr::new("ExampleUser")));
        assert_eq!(
            setting(&parsed, "P4PORT"),
            Some(OsStr::new("ssl:example:1666"))
        );
    }

    #[test]
    fn missing_cwd_does_not_walk_to_a_drive_root_config() {
        let settings = discover_with_names(
            Path::new(r"C:\Example Workspace\missing"),
            &names(&["p4config.txt"]),
        );
        assert!(settings.is_empty());
    }

    #[test]
    fn default_search_skips_volume_root() {
        let tree = TempTree::new("volume");
        let cwd = tree.mkdir("ws/game");
        let dirs = existing_directories_from(&cwd, false);
        assert!(dirs.iter().any(|directory| directory == &cwd));
        assert!(!dirs.iter().any(|directory| is_volume_root(directory)));
    }

    #[test]
    fn explicit_search_includes_volume_root() {
        let tree = TempTree::new("volume-include");
        let cwd = tree.mkdir("ws/game");
        let dirs = existing_directories_from(&cwd, true);
        assert!(
            dirs.iter().any(|directory| is_volume_root(directory)),
            "explicit P4CONFIG walks should still reach the volume root like p4"
        );
    }

    #[test]
    fn nested_cwd_maps_into_client_root_and_foreign_roots_do_not() {
        let identity = identity_at(if cfg!(windows) {
            r"E:\Project"
        } else {
            "/example/project"
        });
        assert!(cwd_maps_into_client_root(
            Path::new(if cfg!(windows) {
                r"E:\Project\NeonGame"
            } else {
                "/example/project/game"
            }),
            &identity
        ));
        assert!(cwd_maps_into_client_root(
            identity.root.as_path(),
            &identity
        ));
        assert!(!cwd_maps_into_client_root(
            Path::new(if cfg!(windows) {
                r"G:\Projects\Neon\NeonGame"
            } else {
                "/other/project/game"
            }),
            &identity
        ));
        assert!(!cwd_maps_into_client_root(
            Path::new(if cfg!(windows) {
                r"E:\ProjectFoo"
            } else {
                "/example/projectfoo"
            }),
            &identity
        ));
        if cfg!(windows) {
            assert!(cwd_maps_into_client_root(
                Path::new(r"e:\project\neongame"),
                &identity
            ));
        } else {
            assert!(cwd_maps_into_client_root(
                Path::new("/Example/Project/game"),
                &identity
            ));
            let mut sensitive = identity_at("/example/project");
            sensitive.case_handling = CaseHandling::Sensitive;
            assert!(!cwd_maps_into_client_root(
                Path::new("/Example/Project/game"),
                &sensitive
            ));
        }
    }

    #[test]
    fn slash_style_client_root_still_contains_the_cwd() {
        let identity = identity_at(if cfg!(windows) {
            "G:/Projects/Neon"
        } else {
            "/example/project"
        });
        assert!(cwd_maps_into_client_root(
            Path::new(if cfg!(windows) {
                r"G:\Projects\Neon\NeonGame"
            } else {
                "/example/project/game"
            }),
            &identity
        ));
    }

    #[test]
    fn existing_workspace_paths_match_through_canonicalize() {
        let tree = TempTree::new("canon");
        let nested = tree.mkdir("NeonGame");
        let mut identity = identity_at("/unused");
        identity.root = tree.root.clone();
        assert!(cwd_maps_into_client_root(&nested, &identity));
    }

    #[test]
    fn foreign_client_root_is_a_classified_error() {
        let error = ensure_cwd_in_client_root(
            Path::new(if cfg!(windows) {
                r"G:\Projects\Neon\NeonGame"
            } else {
                "/other/project/game"
            }),
            &identity_at(if cfg!(windows) {
                r"E:\Project"
            } else {
                "/example/project"
            }),
        )
        .expect_err("foreign root");
        assert_eq!(error.kind, P4ErrorKind::NotInClientView);
        assert!(!error.to_string().contains("/other/"));
        assert!(!error.to_string().contains(r"G:\"));
        assert!(!error.to_string().contains(r"E:\"));
    }

    #[test]
    fn isolated_client_does_not_read_host_p4config() {
        let tree = TempTree::new("isolated");
        tree.write("p4config.txt", "P4CLIENT=ShouldNotApply\n");
        let cwd = tree.mkdir("NeonGame");
        let fake = FakeP4Transport::default();
        fake.push_output(crate::p4::transport::RawP4Output {
            stdout: br#"{"code":"stat","clientName":"ExampleClientA"}"#.to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: std::time::Duration::from_millis(1),
        });
        let client =
            P4Client::new_with_directory_environment(fake.clone(), "p4", cwd, BTreeMap::new());
        client.run(&P4Query::Info).expect("query should succeed");
        assert!(fake.requests()[0].environment.is_empty());
    }

    #[test]
    fn client_new_overlays_p4config_discovered_from_cwd() {
        let tree = TempTree::new("discover-new");
        tree.write("p4config.txt", "P4CLIENT=ExampleFromFile\n");
        let cwd = tree.mkdir("NeonGame");
        let fake = FakeP4Transport::default();
        fake.push_output(crate::p4::transport::RawP4Output {
            stdout: br#"{"code":"stat","clientName":"ExampleFromFile"}"#.to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
            elapsed: std::time::Duration::from_millis(1),
        });
        let discovered = discover_from_ancestors(&cwd, &names(&["p4config.txt"]), false);
        let client = P4Client::new_with_directory_environment(fake.clone(), "p4", cwd, discovered);
        client.run(&P4Query::Info).expect("query should succeed");
        assert_eq!(
            fake.requests()[0]
                .environment
                .get(OsStr::new("P4CLIENT"))
                .map(OsString::as_os_str),
            Some(OsStr::new("ExampleFromFile"))
        );
    }
}
