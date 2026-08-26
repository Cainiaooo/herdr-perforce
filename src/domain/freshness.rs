use std::fmt;

use blake3::Hasher;

use super::{CaseHandling, Changelist, WorkspaceIdentity};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpecToken([u8; 32]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentToken([u8; 32]);

impl SpecToken {
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[cfg(test)]
    pub(crate) const fn from_bytes_for_test(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl ContentToken {
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for SpecToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(formatter, &self.0)
    }
}

impl fmt::Display for ContentToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(formatter, &self.0)
    }
}

fn write_hex(formatter: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    for byte in bytes {
        write!(formatter, "{byte:02x}")?;
    }
    Ok(())
}

/// Computes the ADR-0002 spec token from write-relevant workspace, spec and
/// file-action facts. File order from P4 never affects the result.
#[must_use]
pub fn compute_spec_token(workspace: &WorkspaceIdentity, changelist: &Changelist) -> SpecToken {
    let mut canonical = CanonicalHasher::new(b"herdr-p4/spec-token/v1");
    canonical.field("server_id", workspace.server_id.as_bytes());
    canonical.field("user", workspace.user.as_bytes());
    canonical.field("client", workspace.client.as_bytes());
    canonical.field("change", changelist.id.to_string().as_bytes());
    canonical.field("status", changelist.status.canonical_name().as_bytes());
    canonical.field("owner", changelist.owner.as_bytes());
    canonical.field("change_client", changelist.client.as_bytes());
    canonical.field("description", changelist.description.as_bytes());

    for (name, value) in &changelist.preserved_spec_fields {
        canonical.field("spec_field_name", name.as_bytes());
        canonical.field("spec_field_value", value.as_bytes());
    }

    let mut files = changelist.files.iter().collect::<Vec<_>>();
    files.sort_by(|left, right| {
        path_sort_key(&workspace.case_handling, &left.depot_path)
            .cmp(&path_sort_key(&workspace.case_handling, &right.depot_path))
            .then_with(|| left.depot_path.cmp(&right.depot_path))
    });

    for file in files {
        canonical.field("file", file.depot_path.as_bytes());
        canonical.field("action", file.action.canonical_name().as_bytes());
        canonical.field("type", file.file_type.as_str().as_bytes());
        canonical.optional_u64("base_revision", file.base_revision);
        canonical.optional_field("moved_from", file.moved_from.as_deref());
        canonical.optional_field("moved_to", file.moved_to.as_deref());
    }

    SpecToken(*canonical.finish().as_bytes())
}

fn path_sort_key(case_handling: &CaseHandling, path: &str) -> String {
    case_handling.canonical_path_key(path)
}

struct CanonicalHasher {
    hasher: Hasher,
}

impl CanonicalHasher {
    fn new(domain: &[u8]) -> Self {
        let mut hasher = Hasher::new();
        write_sized(&mut hasher, domain);
        Self { hasher }
    }

    fn field(&mut self, name: &str, value: &[u8]) {
        write_sized(&mut self.hasher, name.as_bytes());
        write_sized(&mut self.hasher, value);
    }

    fn optional_field(&mut self, name: &str, value: Option<&str>) {
        match value {
            Some(value) => {
                self.field(name, b"some");
                self.field("value", value.as_bytes());
            }
            None => self.field(name, b"none"),
        }
    }

    fn optional_u64(&mut self, name: &str, value: Option<u64>) {
        match value {
            Some(value) => {
                self.field(name, b"some");
                self.field("value", &value.to_le_bytes());
            }
            None => self.field(name, b"none"),
        }
    }

    fn finish(self) -> blake3::Hash {
        self.hasher.finalize()
    }
}

fn write_sized(hasher: &mut Hasher, bytes: &[u8]) {
    let length = u64::try_from(bytes.len()).expect("field length must fit in u64");
    hasher.update(&length.to_le_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use super::*;
    use crate::domain::{ChangedFile, ChangelistId, ChangelistStatus, FileAction, FileType};

    fn workspace() -> WorkspaceIdentity {
        WorkspaceIdentity {
            server_id: "example-server".into(),
            user: "ExampleAuthor".into(),
            client: "ExampleClientA".into(),
            root: PathBuf::from("C:/example"),
            stream: None,
            case_handling: CaseHandling::Insensitive,
        }
    }

    fn changelist(files: Vec<ChangedFile>) -> Changelist {
        Changelist {
            id: ChangelistId::Numbered(42),
            status: ChangelistStatus::Pending,
            owner: "ExampleAuthor".into(),
            client: "ExampleClientA".into(),
            description: "A specific change".into(),
            files,
            preserved_spec_fields: BTreeMap::new(),
            spec_token: None,
            content_token: None,
        }
    }

    fn file(path: &str, action: FileAction) -> ChangedFile {
        ChangedFile {
            depot_path: path.into(),
            client_path: None,
            action,
            file_type: FileType::new("text"),
            base_revision: Some(3),
            moved_from: None,
            moved_to: None,
        }
    }

    #[test]
    fn token_is_independent_of_p4_record_order() {
        let first = changelist(vec![
            file("//SampleDepot/B.txt", FileAction::Edit),
            file("//SampleDepot/a.txt", FileAction::Add),
        ]);
        let second = changelist(vec![
            file("//SampleDepot/a.txt", FileAction::Add),
            file("//SampleDepot/B.txt", FileAction::Edit),
        ]);

        assert_eq!(
            compute_spec_token(&workspace(), &first),
            compute_spec_token(&workspace(), &second)
        );
    }

    #[test]
    fn write_relevant_change_invalidates_token() {
        let original = changelist(vec![file("//SampleDepot/a.txt", FileAction::Edit)]);
        let mut changed = original.clone();
        changed.files[0].file_type = FileType::new("binary+l");

        assert_ne!(
            compute_spec_token(&workspace(), &original),
            compute_spec_token(&workspace(), &changed)
        );
    }

    #[test]
    fn description_owner_client_status_and_preserved_fields_invalidate_token() {
        let original = changelist(vec![file("//SampleDepot/a.txt", FileAction::Edit)]);
        let workspace = workspace();
        let original_token = compute_spec_token(&workspace, &original);

        let mut description = original.clone();
        description.description = "Changed purpose".into();
        assert_ne!(original_token, compute_spec_token(&workspace, &description));

        let mut owner = original.clone();
        owner.owner = "ExampleOther".into();
        assert_ne!(original_token, compute_spec_token(&workspace, &owner));

        let mut client = original.clone();
        client.client = "ExampleClientB".into();
        assert_ne!(original_token, compute_spec_token(&workspace, &client));

        let mut status = original.clone();
        status.status = ChangelistStatus::Shelved;
        assert_ne!(original_token, compute_spec_token(&workspace, &status));

        let mut preserved = original.clone();
        preserved
            .preserved_spec_fields
            .insert("Type".into(), "restricted".into());
        assert_ne!(original_token, compute_spec_token(&workspace, &preserved));
    }
}
