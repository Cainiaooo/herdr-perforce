//! Configurable boundary between the reviewed Herdr submit flow and the tool
//! that owns the final submission.

use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    thread,
};

use serde_json::Value;

const CONFIG_FILE_NAME: &str = "submit-provider.json";
const CONFIG_PATH_ENV: &str = "HERDR_P4_SUBMIT_PROVIDER_CONFIG";
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_ARGUMENTS: usize = 64;
const MAX_ARGUMENT_BYTES: usize = 4 * 1024;
const MAX_LABEL_CHARS: usize = 80;

#[derive(Debug, Clone)]
pub enum SubmitProvider {
    Native,
    External(Arc<ExternalSubmitProvider>),
    Invalid(String),
}

#[derive(Debug)]
pub struct ExternalSubmitProvider {
    label: String,
    command: PathBuf,
    arguments: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalLaunchError {
    InvalidConfiguration(String),
    StartFailed,
}

impl SubmitProvider {
    #[must_use]
    pub fn load_from_environment() -> Self {
        let config_path = env::var_os(CONFIG_PATH_ENV).map(PathBuf::from).or_else(|| {
            env::var_os("HERDR_PLUGIN_CONFIG_DIR")
                .map(PathBuf::from)
                .map(|directory| directory.join(CONFIG_FILE_NAME))
        });
        let Some(config_path) = config_path else {
            return Self::Native;
        };
        if !config_path.exists() {
            return Self::Native;
        }
        match Self::load(&config_path) {
            Ok(provider) => provider,
            Err(error) => Self::Invalid(error),
        }
    }

    fn load(path: &Path) -> Result<Self, String> {
        let metadata = fs::metadata(path)
            .map_err(|_| "submit provider config could not be read".to_owned())?;
        if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
            return Err("submit provider config is not a bounded regular file".to_owned());
        }
        let bytes =
            fs::read(path).map_err(|_| "submit provider config could not be read".to_owned())?;
        let document = serde_json::from_slice::<Value>(&bytes)
            .map_err(|_| "submit provider config is not valid JSON".to_owned())?;
        let object = document
            .as_object()
            .ok_or_else(|| "submit provider config must be a JSON object".to_owned())?;
        match object.get("mode").and_then(Value::as_str) {
            Some("native") => Ok(Self::Native),
            Some("external") => {
                let label = required_string(object.get("label"), "label")?;
                if label.chars().count() > MAX_LABEL_CHARS || label.chars().any(char::is_control) {
                    return Err("external submit provider label is invalid".to_owned());
                }
                let command = PathBuf::from(required_string(object.get("command"), "command")?);
                validate_direct_executable(&command)?;
                let arguments = object
                    .get("args")
                    .and_then(Value::as_array)
                    .ok_or_else(|| "external submit provider args must be an array".to_owned())?;
                if arguments.len() > MAX_ARGUMENTS {
                    return Err("external submit provider has too many arguments".to_owned());
                }
                let mut parsed_arguments = Vec::with_capacity(arguments.len());
                let mut has_change_placeholder = false;
                for argument in arguments {
                    let argument = required_string(Some(argument), "args item")?;
                    if argument.len() > MAX_ARGUMENT_BYTES || argument.contains('\0') {
                        return Err("external submit provider argument is invalid".to_owned());
                    }
                    has_change_placeholder |= argument.contains("{change}");
                    parsed_arguments.push(argument);
                }
                if !has_change_placeholder {
                    return Err("external submit provider args must contain {change}".to_owned());
                }
                Ok(Self::External(Arc::new(ExternalSubmitProvider {
                    label,
                    command,
                    arguments: parsed_arguments,
                })))
            }
            _ => Err("submit provider mode must be native or external".to_owned()),
        }
    }

    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Native => "Native p4 submit",
            Self::External(provider) => provider.label(),
            Self::Invalid(_) => "Invalid submit provider configuration",
        }
    }

    #[must_use]
    pub const fn is_native(&self) -> bool {
        matches!(self, Self::Native)
    }

    #[cfg(test)]
    pub(crate) fn external_for_test(
        label: impl Into<String>,
        command: PathBuf,
        arguments: Vec<String>,
    ) -> Self {
        Self::External(Arc::new(ExternalSubmitProvider {
            label: label.into(),
            command,
            arguments,
        }))
    }
}

impl ExternalSubmitProvider {
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn launch(&self, change: u64, cwd: &Path) -> Result<(), ExternalLaunchError> {
        if let Err(detail) = validate_direct_executable(&self.command) {
            return Err(ExternalLaunchError::InvalidConfiguration(format!(
                "{detail}. No p4 submit was run."
            )));
        }
        let mut child = self
            .command_for_launch(change, cwd)
            .spawn()
            .map_err(|_| ExternalLaunchError::StartFailed)?;
        thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    pub(crate) fn substituted_args(&self, change: u64) -> Vec<String> {
        let change = change.to_string();
        self.arguments
            .iter()
            .map(|argument| argument.replace("{change}", &change))
            .collect()
    }

    fn command_for_launch(&self, change: u64, cwd: &Path) -> Command {
        let mut command = Command::new(&self.command);
        command.current_dir(cwd);
        command.args(self.substituted_args(change));
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
        for (name, _) in env::vars_os() {
            if is_herdr_control_var(&name) {
                command.env_remove(name);
            }
        }
        detach_from_parent_console(&mut command);
        command
    }
}

fn validate_direct_executable(command: &Path) -> Result<(), String> {
    if !command.is_absolute() || !command.is_file() {
        return Err(
            "external submit provider command must be an existing absolute file".to_owned(),
        );
    }
    if is_shell_wrapper(command) {
        return Err(
            "external submit provider command must be a direct executable, not a shell script"
                .to_owned(),
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(command)
            .map_err(|_| "submit provider config could not be read".to_owned())?
            .permissions()
            .mode();
        if mode & 0o111 == 0 {
            return Err("external submit provider command must be executable".to_owned());
        }
    }
    Ok(())
}

fn is_shell_wrapper(command: &Path) -> bool {
    command
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd")
        })
}

fn is_herdr_control_var(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| name.starts_with("HERDR_"))
}

fn detach_from_parent_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

fn required_string(value: Option<&Value>, field: &str) -> Result<String, String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("external submit provider {field} must be a non-empty string"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempConfig {
        directory: PathBuf,
    }

    impl TempConfig {
        fn new(tag: &str) -> Self {
            let directory = env::temp_dir().join(format!(
                "herdr-p4-submit-provider-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            fs::create_dir_all(&directory).expect("temp directory");
            Self { directory }
        }

        fn path(&self) -> PathBuf {
            self.directory.join(CONFIG_FILE_NAME)
        }

        fn write(&self, value: &Value) {
            fs::write(self.path(), value.to_string()).expect("write config");
        }
    }

    impl Drop for TempConfig {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn existing_executable() -> PathBuf {
        env::current_exe().expect("current executable")
    }

    fn load_external(config: &TempConfig, args: Vec<&str>) -> Arc<ExternalSubmitProvider> {
        config.write(&serde_json::json!({
            "mode": "external",
            "label": "Example review tool",
            "command": existing_executable(),
            "args": args
        }));
        match SubmitProvider::load(&config.path()).expect("provider config") {
            SubmitProvider::External(provider) => provider,
            other => panic!("expected external provider, got {other:?}"),
        }
    }

    fn harmless_executable() -> Option<PathBuf> {
        #[cfg(windows)]
        {
            let mut path = PathBuf::from(
                env::var_os("SystemRoot")
                    .unwrap_or_else(|| OsStr::new(r"C:\Windows").to_os_string()),
            );
            path.push("System32");
            path.push("where.exe");
            path.is_file().then_some(path)
        }
        #[cfg(unix)]
        {
            ["/bin/true", "/usr/bin/true"]
                .into_iter()
                .map(PathBuf::from)
                .find(|path| path.is_file())
        }
    }

    #[test]
    fn missing_configuration_defaults_to_native() {
        assert!(SubmitProvider::Native.is_native());
        assert_eq!(SubmitProvider::Native.label(), "Native p4 submit");
        assert!(!SubmitProvider::Invalid("broken".into()).is_native());
    }

    #[test]
    fn required_string_rejects_empty_values() {
        assert!(required_string(Some(&Value::String("  ".to_owned())), "label").is_err());
    }

    #[test]
    fn native_mode_file_selects_native_provider() {
        let config = TempConfig::new("native-mode");
        config.write(&serde_json::json!({ "mode": "native" }));
        let provider = SubmitProvider::load(&config.path()).expect("native config");
        assert!(provider.is_native());
    }

    #[test]
    fn external_config_substitutes_only_the_change_placeholder() {
        let config = TempConfig::new("placeholder");
        let provider = load_external(&config, vec!["--change", "{change}", "--keep", "{mode}"]);
        assert_eq!(provider.label(), "Example review tool");
        assert_eq!(
            provider.substituted_args(42),
            vec!["--change", "42", "--keep", "{mode}"]
        );
        assert!(!provider.command.as_os_str().is_empty());
    }

    #[test]
    fn invalid_json_non_absolute_command_and_missing_placeholder_are_fail_closed() {
        let config = TempConfig::new("fail-closed");
        fs::write(config.path(), "{not json").expect("invalid json");
        let invalid_json = SubmitProvider::load(&config.path()).expect_err("invalid json");
        assert!(invalid_json.contains("valid JSON"));

        config.write(&serde_json::json!({
            "mode": "external",
            "label": "Example review tool",
            "command": "submit-tool.exe",
            "args": ["{change}"]
        }));
        let relative = SubmitProvider::load(&config.path()).expect_err("relative command");
        assert!(relative.contains("absolute file"));

        config.write(&serde_json::json!({
            "mode": "external",
            "label": "Example review tool",
            "command": existing_executable(),
            "args": ["--review"]
        }));
        let missing = SubmitProvider::load(&config.path()).expect_err("missing placeholder");
        assert!(missing.contains("{change}"));
    }

    #[test]
    fn shell_wrappers_are_rejected_at_load() {
        let config = TempConfig::new("shell-wrapper");
        let command = config.directory.join("submit-tool.BAT");
        fs::write(&command, b"@echo off\r\n").expect("batch file");
        config.write(&serde_json::json!({
            "mode": "external",
            "label": "Example review tool",
            "command": command,
            "args": ["{change}"]
        }));
        let error = SubmitProvider::load(&config.path()).expect_err("shell wrapper");
        assert!(error.contains("direct executable"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_non_executable_command_is_rejected() {
        let config = TempConfig::new("unix-mode");
        use std::os::unix::fs::PermissionsExt;
        let command = config.directory.join("submit-tool");
        fs::write(&command, b"#!/bin/sh\n").expect("script");
        let mut permissions = fs::metadata(&command).expect("metadata").permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&command, permissions).expect("chmod");
        config.write(&serde_json::json!({
            "mode": "external",
            "label": "Example review tool",
            "command": command,
            "args": ["{change}"]
        }));
        let error = SubmitProvider::load(&config.path()).expect_err("not executable");
        assert!(error.contains("executable"));
    }

    #[test]
    fn herdr_control_variables_are_identified_for_stripping() {
        assert!(is_herdr_control_var(OsStr::new("HERDR_PLUGIN_ROOT")));
        assert!(is_herdr_control_var(OsStr::new(
            "HERDR_P4_SUBMIT_PROVIDER_CONFIG"
        )));
        assert!(!is_herdr_control_var(OsStr::new("PATH")));
        assert!(!is_herdr_control_var(OsStr::new("P4CLIENT")));
    }

    #[test]
    fn vanished_command_is_invalid_configuration_not_a_start_failure() {
        let provider = ExternalSubmitProvider {
            label: "Example review tool".into(),
            command: env::temp_dir().join("herdr-p4-missing-submit-tool.exe"),
            arguments: vec!["{change}".into()],
        };
        assert!(matches!(
            provider.launch(42, &env::temp_dir()),
            Err(ExternalLaunchError::InvalidConfiguration(detail))
                if detail.contains("No p4 submit was run")
        ));
    }

    #[test]
    fn launch_starts_a_detached_direct_argv_process() {
        let command = harmless_executable().expect("direct executable fixture");
        let cwd = env::temp_dir();
        let provider = ExternalSubmitProvider {
            label: "Example review tool".into(),
            command,
            arguments: vec!["{change}".into()],
        };
        provider
            .launch(42, &cwd)
            .expect("direct argv spawn should succeed");
    }
}
