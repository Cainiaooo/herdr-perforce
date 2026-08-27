use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
};

use crate::domain::ChangelistId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum P4Query {
    Info,
    PendingChanges {
        user: String,
        client: String,
    },
    PendingChangesLimited {
        user: String,
        client: String,
        max_results: u16,
    },
    Opened {
        change: ChangelistId,
    },
    DescribeSummary {
        change: u64,
    },
    Where {
        path: PathBuf,
    },
    WhereMany {
        paths: Vec<PathBuf>,
    },
    Fstat {
        paths: Vec<PathBuf>,
    },
    OpenedOnClient,
}

impl P4Query {
    #[must_use]
    pub fn args(&self) -> Vec<OsString> {
        let mut args = vec![OsString::from("-ztag"), OsString::from("-Mj")];
        match self {
            Self::Info => args.push(OsString::from("info")),
            Self::PendingChanges { user, client } => {
                args.extend([
                    OsString::from("changes"),
                    OsString::from("-s"),
                    OsString::from("pending"),
                    OsString::from("-u"),
                    OsString::from(user),
                    OsString::from("-c"),
                    OsString::from(client),
                ]);
            }
            Self::PendingChangesLimited {
                user,
                client,
                max_results,
            } => {
                args.extend([
                    OsString::from("changes"),
                    OsString::from("-m"),
                    OsString::from(max_results.to_string()),
                    OsString::from("-s"),
                    OsString::from("pending"),
                    OsString::from("-u"),
                    OsString::from(user),
                    OsString::from("-c"),
                    OsString::from(client),
                ]);
            }
            Self::Opened { change } => {
                args.extend([
                    OsString::from("opened"),
                    OsString::from("-c"),
                    OsString::from(change.as_p4_arg()),
                ]);
            }
            Self::DescribeSummary { change } => {
                args.extend([
                    OsString::from("describe"),
                    OsString::from("-s"),
                    OsString::from(change.to_string()),
                ]);
            }
            Self::Where { path } => {
                args.push(OsString::from("where"));
                args.push(escape_p4_file_arg(path.as_os_str()));
            }
            Self::WhereMany { paths } => {
                args.push(OsString::from("where"));
                args.extend(
                    paths
                        .iter()
                        .map(|path| escape_p4_file_arg(path.as_os_str())),
                );
            }
            Self::Fstat { paths } => {
                args.extend([OsString::from("fstat"), OsString::from("-Ol")]);
                args.extend(
                    paths
                        .iter()
                        .map(|path| escape_p4_file_arg(path.as_os_str())),
                );
            }
            Self::OpenedOnClient => args.push(OsString::from("opened")),
        }
        args
    }
}

/// Percent-encodes Perforce revision/wildcard metacharacters in a file argument.
///
/// Helix treats `#`, `@`, `*`, and `%` in file args as revision or wildcard
/// syntax. Encoding keeps a literal filename as a single argv element *and* as
/// a literal path once `p4` parses it.
#[must_use]
pub fn escape_p4_file_arg(path: &OsStr) -> OsString {
    let Some(text) = path.to_str() else {
        return path.to_os_string();
    };
    OsString::from(escape_p4_file_arg_str(text))
}

fn escape_p4_file_arg_str(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '%' => escaped.push_str("%25"),
            '#' => escaped.push_str("%23"),
            '@' => escaped.push_str("%40"),
            '*' => escaped.push_str("%2A"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_values_remain_single_argv_elements() {
        let args = P4Query::PendingChanges {
            user: "Example User; submit".into(),
            client: "Example Client & more".into(),
        }
        .args();

        assert_eq!(args[6], "Example User; submit");
        assert_eq!(args[8], "Example Client & more");
        assert_eq!(args.len(), 9);
    }

    #[test]
    fn bounded_pending_query_adds_a_numeric_server_limit() {
        let args = P4Query::PendingChangesLimited {
            user: "Example User".into(),
            client: "ExampleClientA".into(),
            max_results: 8,
        }
        .args();

        assert_eq!(
            args,
            [
                "-ztag",
                "-Mj",
                "changes",
                "-m",
                "8",
                "-s",
                "pending",
                "-u",
                "Example User",
                "-c",
                "ExampleClientA",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn where_path_escapes_revision_and_wildcard_syntax() {
        let args = P4Query::Where {
            path: PathBuf::from(r"C:\Example Workspace\file#name@rev%.txt"),
        }
        .args();

        assert_eq!(args.len(), 4);
        assert_eq!(args[3], r"C:\Example Workspace\file%23name%40rev%25.txt");
    }

    #[test]
    fn fstat_and_where_many_keep_each_path_as_one_argv_element() {
        let paths = vec![
            PathBuf::from(r"C:\Example Workspace\a.txt"),
            PathBuf::from(r"C:\Example Workspace\file#name.txt"),
        ];
        let fstat = P4Query::Fstat {
            paths: paths.clone(),
        }
        .args();
        assert_eq!(
            fstat,
            [
                "-ztag",
                "-Mj",
                "fstat",
                "-Ol",
                r"C:\Example Workspace\a.txt",
                r"C:\Example Workspace\file%23name.txt",
            ]
            .map(OsString::from)
        );

        let where_many = P4Query::WhereMany { paths }.args();
        assert_eq!(where_many[2], "where");
        assert_eq!(where_many[3], r"C:\Example Workspace\a.txt");
        assert_eq!(where_many[4], r"C:\Example Workspace\file%23name.txt");
    }

    #[test]
    fn opened_on_client_does_not_pass_a_changelist_filter() {
        let args = P4Query::OpenedOnClient.args();
        assert_eq!(args, ["-ztag", "-Mj", "opened"].map(OsString::from));
    }

    #[test]
    fn percent_is_encoded_before_other_metacharacters() {
        assert_eq!(escape_p4_file_arg_str("a%#@*"), "a%25%23%40%2A");
        assert_eq!(escape_p4_file_arg_str(r"C:\plain.txt"), r"C:\plain.txt");
        assert_eq!(
            escape_p4_file_arg_str(r"C:\Example Workspace\..."),
            r"C:\Example Workspace\..."
        );
    }
}
