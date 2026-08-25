use std::ffi::{OsStr, OsString};

use super::transport::P4Request;

/// Herdr control variables that must not be forwarded to a `p4` child process.
///
/// An empty `P4Request::environment` means "inherit the process environment".
/// These names are always listed in `removed_environment` so a transport can
/// subtract them without clearing `P4PORT`, tickets, or other P4 settings.
/// `P4PASSWD` is intentionally not listed: `p4` itself may need it.
#[must_use]
pub fn herdr_control_variable_names() -> Vec<OsString> {
    [
        "HERDR_BIN_PATH",
        "HERDR_SOCKET",
        "HERDR_SOCKET_PATH",
        "HERDR_PLUGIN_CONFIG_DIR",
        "HERDR_PLUGIN_STATE_DIR",
        "HERDR_PLUGIN_ROOT",
        "HERDR_PLUGIN_CONTEXT",
        "HERDR_WORKSPACE",
        "HERDR_WORKSPACE_ID",
        "HERDR_TAB_ID",
        "HERDR_PANE_ID",
        "HERDR_ACTION",
        "HERDR_ENTRYPOINT",
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

#[must_use]
pub fn is_herdr_control_variable(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| name.starts_with("HERDR_"))
}

/// Names a transport should remove from the inherited environment.
///
/// Starts from `request.removed_environment` and also strips any currently
/// inherited `HERDR_*` variables, so newly introduced Herdr keys cannot leak
/// just because the request list is stale.
#[must_use]
pub fn environment_keys_to_remove(request: &P4Request) -> Vec<OsString> {
    let mut keys = request.removed_environment.clone();
    for (key, _) in std::env::vars_os() {
        if is_herdr_control_variable(&key) && !keys.iter().any(|existing| existing == &key) {
            keys.push(key);
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn herdr_prefix_is_control_and_p4passwd_is_not() {
        assert!(is_herdr_control_variable(OsStr::new("HERDR_BIN_PATH")));
        assert!(is_herdr_control_variable(OsStr::new("HERDR_PANE_ID")));
        assert!(!is_herdr_control_variable(OsStr::new("P4PASSWD")));
        assert!(!is_herdr_control_variable(OsStr::new("P4PORT")));
        assert!(
            !herdr_control_variable_names()
                .iter()
                .any(|name| name == "P4PASSWD")
        );
    }
}
