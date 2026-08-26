use std::{collections::BTreeMap, error::Error, fmt, path::PathBuf, str};

use serde_json::Value;

use crate::domain::{
    CaseHandling, ChangedFile, Changelist, ChangelistId, ChangelistStatus, FileAction, FileType,
    WorkspaceIdentity,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordCode {
    Stat,
    Info,
    Warning,
    Error,
    Text,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct StructuredRecord {
    pub code: RecordCode,
    fields: BTreeMap<String, Value>,
}

impl StructuredRecord {
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.fields.get(name)
    }

    #[must_use]
    pub fn string(&self, name: &str) -> Option<String> {
        value_as_string(self.field(name)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub kind: ParseErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseErrorKind {
    InvalidUtf8,
    InvalidJson,
    ExpectedObject,
    MissingCode,
    InvalidCode,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "structured P4 parse error on line {}", self.line)
    }
}

impl Error for ParseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainMappingError {
    pub record: usize,
    pub field: String,
    pub reason: &'static str,
}

impl fmt::Display for DomainMappingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "P4 record {} has invalid field {}: {}",
            self.record, self.field, self.reason
        )
    }
}

impl Error for DomainMappingError {}

pub fn parse_json_records(bytes: &[u8]) -> Result<Vec<StructuredRecord>, ParseError> {
    let text = str::from_utf8(bytes).map_err(|error| ParseError {
        line: utf8_error_line(bytes, error.valid_up_to()),
        kind: ParseErrorKind::InvalidUtf8,
    })?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut records = Vec::new();

    for (line_index, line) in text.lines().enumerate() {
        let line_number = line_index + 1;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|_| ParseError {
            line: line_number,
            kind: ParseErrorKind::InvalidJson,
        })?;
        let Value::Object(mut object) = value else {
            return Err(ParseError {
                line: line_number,
                kind: ParseErrorKind::ExpectedObject,
            });
        };
        let code = match object.remove("code") {
            None => inferred_record_code(&object),
            Some(Value::String(code)) => record_code(&code),
            Some(_) => {
                return Err(ParseError {
                    line: line_number,
                    kind: ParseErrorKind::InvalidCode,
                });
            }
        };

        records.push(StructuredRecord {
            code,
            fields: object.into_iter().collect(),
        });
    }

    Ok(records)
}

pub fn workspace_from_info(
    records: &[StructuredRecord],
) -> Result<WorkspaceIdentity, DomainMappingError> {
    let (index, record) = records
        .iter()
        .enumerate()
        .find(|(_, record)| {
            matches!(record.code, RecordCode::Stat) && record.field("clientName").is_some()
        })
        .ok_or_else(|| missing(0, "clientName"))?;

    let server_id = optional_string(record, "serverID")
        .or_else(|| optional_string(record, "serverAddress"))
        .ok_or_else(|| missing(index, "serverID"))?;

    Ok(WorkspaceIdentity {
        server_id,
        user: required_string(record, index, "userName")?,
        client: required_string(record, index, "clientName")?,
        root: PathBuf::from(required_string(record, index, "clientRoot")?),
        stream: optional_string(record, "clientStream"),
        case_handling: CaseHandling::from_p4(&required_string(record, index, "caseHandling")?),
    })
}

pub fn changelists_from_changes(
    records: &[StructuredRecord],
) -> Result<Vec<Changelist>, DomainMappingError> {
    records
        .iter()
        .enumerate()
        .filter(|(_, record)| {
            matches!(record.code, RecordCode::Stat) && record.field("change").is_some()
        })
        .map(|(index, record)| changelist_from_stat(record, index))
        .collect()
}

/// Maps `p4 changes` records and always includes the current client's default
/// pending changelist, even when the server returned no numbered changes.
pub fn pending_changelists_from_changes(
    records: &[StructuredRecord],
    owner: &str,
    client: &str,
) -> Result<Vec<Changelist>, DomainMappingError> {
    let mut changelists = changelists_from_changes(records)?;
    if !changelists
        .iter()
        .any(|changelist| changelist.id == ChangelistId::Default)
    {
        changelists.insert(0, default_pending_changelist(owner, client));
    }
    Ok(changelists)
}

#[must_use]
pub fn default_pending_changelist(owner: &str, client: &str) -> Changelist {
    Changelist {
        id: ChangelistId::Default,
        status: ChangelistStatus::Pending,
        owner: owner.to_owned(),
        client: client.to_owned(),
        description: String::new(),
        files: Vec::new(),
        preserved_spec_fields: BTreeMap::new(),
        spec_token: None,
        content_token: None,
    }
}

/// Maps a `p4 describe -s` record, including indexed `depotFileN` file lists.
pub fn changelist_from_describe(
    records: &[StructuredRecord],
) -> Result<Changelist, DomainMappingError> {
    let mut changelists = changelists_from_changes(records)?;
    match changelists.len() {
        0 => Err(invalid(0, "change", "no describe changelist record")),
        1 => {
            let mut changelist = changelists
                .pop()
                .expect("describe mapping checked for a single changelist");
            if changelist.files.is_empty() {
                changelist.files = changed_files_from_records(records, false)?;
            }
            Ok(changelist)
        }
        _ => Err(invalid(
            0,
            "change",
            "describe output mapped to multiple changelists",
        )),
    }
}

pub fn changed_files_from_opened(
    records: &[StructuredRecord],
) -> Result<Vec<ChangedFile>, DomainMappingError> {
    changed_files_from_records(records, true)
}

fn changelist_from_stat(
    record: &StructuredRecord,
    index: usize,
) -> Result<Changelist, DomainMappingError> {
    let change = required_string(record, index, "change")?;
    Ok(Changelist {
        id: parse_changelist_id(index, &change)?,
        status: ChangelistStatus::from_p4(&required_string(record, index, "status")?),
        owner: required_string(record, index, "user")?,
        client: required_string(record, index, "client")?,
        description: optional_string(record, "desc")
            .or_else(|| optional_string(record, "Description"))
            .ok_or_else(|| missing(index, "desc"))?,
        files: indexed_changed_files(record, index)?,
        preserved_spec_fields: BTreeMap::new(),
        spec_token: None,
        content_token: None,
    })
}

fn changed_files_from_records(
    records: &[StructuredRecord],
    reject_indexed_describe: bool,
) -> Result<Vec<ChangedFile>, DomainMappingError> {
    let has_named_depot_file = records.iter().any(|record| {
        matches!(record.code, RecordCode::Stat) && record.field("depotFile").is_some()
    });
    let has_indexed_depot_file = records.iter().any(|record| {
        matches!(record.code, RecordCode::Stat) && record.field("depotFile0").is_some()
    });
    if reject_indexed_describe && has_indexed_depot_file && !has_named_depot_file {
        return Err(invalid(
            0,
            "depotFile",
            "indexed describe records require changelist_from_describe",
        ));
    }

    records
        .iter()
        .enumerate()
        .filter(|(_, record)| {
            matches!(record.code, RecordCode::Stat) && record.field("depotFile").is_some()
        })
        .map(|(index, record)| {
            changed_file_from_fields(
                record,
                index,
                "depotFile",
                "action",
                "type",
                "clientFile",
                "movedFile",
                "haveRev",
                "rev",
            )
        })
        .collect()
}

fn indexed_changed_files(
    record: &StructuredRecord,
    record_index: usize,
) -> Result<Vec<ChangedFile>, DomainMappingError> {
    let mut files = Vec::new();
    for file_index in 0usize.. {
        let depot_field = format!("depotFile{file_index}");
        if optional_string(record, &depot_field).is_none() {
            break;
        }
        files.push(changed_file_from_fields(
            record,
            record_index,
            &depot_field,
            &format!("action{file_index}"),
            &format!("type{file_index}"),
            &format!("clientFile{file_index}"),
            &format!("movedFile{file_index}"),
            &format!("haveRev{file_index}"),
            &format!("rev{file_index}"),
        )?);
    }
    Ok(files)
}

#[allow(clippy::too_many_arguments)]
fn changed_file_from_fields(
    record: &StructuredRecord,
    index: usize,
    depot_field: &str,
    action_field: &str,
    type_field: &str,
    client_field: &str,
    moved_field: &str,
    have_field: &str,
    rev_field: &str,
) -> Result<ChangedFile, DomainMappingError> {
    let action = FileAction::from_p4(&required_named(record, index, action_field)?);
    let moved_file = optional_string(record, moved_field);
    let (moved_from, moved_to) = match &action {
        FileAction::MoveAdd => (moved_file, None),
        FileAction::MoveDelete => (None, moved_file),
        _ => (None, None),
    };
    let base_revision = optional_revision(record, index, &action, have_field, rev_field)?;
    Ok(ChangedFile {
        depot_path: required_named(record, index, depot_field)?,
        client_path: optional_string(record, client_field).map(PathBuf::from),
        action,
        file_type: FileType::new(required_named(record, index, type_field)?),
        base_revision,
        moved_from,
        moved_to,
    })
}

fn optional_revision(
    record: &StructuredRecord,
    index: usize,
    action: &FileAction,
    have_field: &str,
    rev_field: &str,
) -> Result<Option<u64>, DomainMappingError> {
    if let Some(value) = optional_string(record, have_field) {
        return parse_revision_field(index, have_field, &value);
    }
    if matches!(action, FileAction::Add) {
        return Ok(None);
    }
    if let Some(value) = optional_string(record, rev_field) {
        return parse_revision_field(index, have_field, &value);
    }
    Ok(None)
}

fn parse_revision_field(
    index: usize,
    field: &str,
    value: &str,
) -> Result<Option<u64>, DomainMappingError> {
    parse_revision_value(value).map_err(|()| invalid(index, field, "expected an unsigned revision"))
}

pub(crate) fn parse_revision_value(value: &str) -> Result<Option<u64>, ()> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    trimmed.parse::<u64>().map(Some).map_err(|_| ())
}

fn parse_changelist_id(index: usize, value: &str) -> Result<ChangelistId, DomainMappingError> {
    if value.eq_ignore_ascii_case("default") {
        return Ok(ChangelistId::Default);
    }
    value
        .parse::<u64>()
        .map(ChangelistId::Numbered)
        .map_err(|_| {
            invalid(
                index,
                "change",
                "expected an unsigned changelist number or default",
            )
        })
}

fn utf8_error_line(bytes: &[u8], valid_up_to: usize) -> usize {
    bytes[..valid_up_to.min(bytes.len())]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

fn record_code(code: &str) -> RecordCode {
    match code {
        "stat" => RecordCode::Stat,
        "info" => RecordCode::Info,
        "warning" => RecordCode::Warning,
        "error" => RecordCode::Error,
        "text" => RecordCode::Text,
        _ => RecordCode::Unknown(code.to_owned()),
    }
}

fn inferred_record_code(object: &serde_json::Map<String, Value>) -> RecordCode {
    if let Some(severity) = object.get("severity") {
        return match value_as_string(severity).and_then(|value| value.parse::<u8>().ok()) {
            Some(0) => RecordCode::Text,
            Some(1) => RecordCode::Info,
            Some(2) => RecordCode::Warning,
            Some(_) | None => RecordCode::Error,
        };
    }

    if object.contains_key("data") || object.contains_key("level") {
        RecordCode::Info
    } else {
        RecordCode::Stat
    }
}

fn required_string(
    record: &StructuredRecord,
    index: usize,
    field: &'static str,
) -> Result<String, DomainMappingError> {
    required_named(record, index, field)
}

fn required_named(
    record: &StructuredRecord,
    index: usize,
    field: &str,
) -> Result<String, DomainMappingError> {
    optional_string(record, field).ok_or_else(|| missing(index, field))
}

fn optional_string(record: &StructuredRecord, field: &str) -> Option<String> {
    record.field(field).and_then(value_as_string)
}

fn value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn missing(index: usize, field: impl Into<String>) -> DomainMappingError {
    invalid(index, field, "required field is missing or not scalar")
}

fn invalid(index: usize, field: impl Into<String>, reason: &'static str) -> DomainMappingError {
    DomainMappingError {
        record: index,
        field: field.into(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Map;

    use super::*;

    #[test]
    fn parses_line_delimited_records_and_preserves_unknown_fields() {
        let records = parse_json_records(
            b"{\"code\":\"stat\",\"change\":\"42\",\"futureField\":\"kept\"}\n\
              {\"code\":\"future\",\"value\":7}\n",
        )
        .expect("fixture should parse");

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].string("futureField").as_deref(), Some("kept"));
        assert_eq!(records[1].code, RecordCode::Unknown("future".into()));
        assert_eq!(records[1].string("value").as_deref(), Some("7"));
    }

    #[test]
    fn real_mj_stat_record_without_code_is_accepted() {
        let records = parse_json_records(
            br#"{"serverAddress":"ExampleHost:1666","clientName":"ExampleClient","caseHandling":"insensitive"}"#,
        )
        .expect("successful -Mj records omit code");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].code, RecordCode::Stat);
        assert_eq!(
            records[0].string("serverAddress").as_deref(),
            Some("ExampleHost:1666")
        );
    }

    #[test]
    fn real_mj_diagnostics_without_code_are_classified_by_severity() {
        let records = parse_json_records(
            b"{\"data\":\"can't edit exclusive file\",\"generic\":0,\"severity\":2}\n\
              {\"data\":\"also opened by another client\",\"level\":1}",
        )
        .expect("real -Mj diagnostics should parse");

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].code, RecordCode::Warning);
        assert_eq!(records[1].code, RecordCode::Info);
    }

    #[test]
    fn non_string_explicit_code_is_rejected() {
        let error = parse_json_records(br#"{"code":7,"change":"42"}"#)
            .expect_err("explicit code must remain a string");

        assert_eq!(error.kind, ParseErrorKind::InvalidCode);
    }

    #[test]
    fn malformed_record_has_structured_location_without_echoing_input() {
        let error = parse_json_records(b"{\"code\":\"stat\"}\nnot-json")
            .expect_err("second line should fail");

        assert_eq!(error.line, 2);
        assert_eq!(error.kind, ParseErrorKind::InvalidJson);
        assert!(!error.to_string().contains("not-json"));
    }

    #[test]
    fn invalid_utf8_reports_the_affected_line() {
        let error = parse_json_records(b"{\"code\":\"stat\"}\n\xFF")
            .expect_err("second line should fail utf8");
        assert_eq!(error.line, 2);
        assert_eq!(error.kind, ParseErrorKind::InvalidUtf8);
    }

    #[test]
    fn utf8_bom_is_ignored() {
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend_from_slice(br#"{"code":"stat","change":"1"}"#);
        let records = parse_json_records(&bytes).expect("BOM should be stripped");
        assert_eq!(records[0].string("change").as_deref(), Some("1"));
    }

    #[test]
    fn maps_workspace_identity() {
        let records = parse_json_records(
            br#"{"code":"stat","serverID":"sample-server","userName":"ExampleAuthor","clientName":"ExampleClientA","clientRoot":"C:/Example","clientStream":"//SampleDepot/main","caseHandling":"insensitive","unknown":"ignored"}"#,
        )
        .expect("fixture should parse");

        let identity = workspace_from_info(&records).expect("identity should map");
        assert_eq!(identity.server_id, "sample-server");
        assert_eq!(identity.client, "ExampleClientA");
        assert_eq!(identity.case_handling, CaseHandling::Insensitive);
    }

    #[test]
    fn maps_move_and_binary_lock_metadata() {
        let records = parse_json_records(
            br#"{"code":"stat","depotFile":"//SampleDepot/new.uasset","clientFile":"C:/Example/new.uasset","action":"move/add","type":"binary+l","haveRev":"4","movedFile":"//SampleDepot/old.uasset"}"#,
        )
        .expect("fixture should parse");

        let files = changed_files_from_opened(&records).expect("file should map");
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].moved_from.as_deref(),
            Some("//SampleDepot/old.uasset")
        );
        assert_eq!(files[0].base_revision, Some(4));
        assert!(files[0].file_type.is_binary());
        assert!(files[0].file_type.is_exclusive_open());
    }

    #[test]
    fn add_with_have_rev_none_has_no_base_revision() {
        let records = parse_json_records(
            br#"{"code":"stat","depotFile":"//SampleDepot/new.txt","action":"add","type":"text","haveRev":"none","rev":"1"}"#,
        )
        .expect("fixture should parse");

        let files = changed_files_from_opened(&records).expect("add should map");
        assert_eq!(files[0].action, FileAction::Add);
        assert_eq!(files[0].base_revision, None);
    }

    #[test]
    fn add_without_have_rev_does_not_use_pending_rev_as_base() {
        let records = parse_json_records(
            br#"{"code":"stat","depotFile":"//SampleDepot/new.txt","action":"add","type":"text","rev":"1"}"#,
        )
        .expect("fixture should parse");

        let files = changed_files_from_opened(&records).expect("add should map");
        assert_eq!(files[0].base_revision, None);
    }

    #[test]
    fn invalid_have_rev_fails_closed() {
        let records = parse_json_records(
            br#"{"code":"stat","depotFile":"//SampleDepot/a.txt","action":"edit","type":"text","haveRev":"head"}"#,
        )
        .expect("fixture should parse");

        let error = changed_files_from_opened(&records).expect_err("head is not a revision");
        assert_eq!(error.field, "haveRev");
    }

    #[test]
    fn missing_required_field_fails_closed() {
        let records = parse_json_records(
            br#"{"code":"stat","change":"42","status":"pending","user":"ExampleAuthor","client":"ExampleClientA"}"#,
        )
        .expect("fixture should parse");

        let error = changelists_from_changes(&records).expect_err("desc is required");
        assert_eq!(error.field, "desc");
    }

    #[test]
    fn empty_pending_changes_still_include_default() {
        let changelists = pending_changelists_from_changes(&[], "ExampleAuthor", "ExampleClientA")
            .expect("default changelist is local");
        assert_eq!(changelists.len(), 1);
        assert_eq!(changelists[0].id, ChangelistId::Default);
        assert_eq!(changelists[0].status, ChangelistStatus::Pending);
        assert_eq!(changelists[0].owner, "ExampleAuthor");
        assert_eq!(changelists[0].client, "ExampleClientA");
    }

    #[test]
    fn change_field_default_maps_to_default_id() {
        let records = parse_json_records(
            br#"{"code":"stat","change":"default","status":"pending","user":"ExampleAuthor","client":"ExampleClientA","desc":""}"#,
        )
        .expect("fixture should parse");

        let changelists = changelists_from_changes(&records).expect("default id should map");
        assert_eq!(changelists[0].id, ChangelistId::Default);
    }

    #[test]
    fn numbered_pending_changes_are_preceded_by_default() {
        let records = parse_json_records(
            br#"{"code":"stat","change":"42","status":"pending","user":"ExampleAuthor","client":"ExampleClientA","desc":"Work"}"#,
        )
        .expect("fixture should parse");

        let changelists =
            pending_changelists_from_changes(&records, "ExampleAuthor", "ExampleClientA")
                .expect("pending list should map");
        assert_eq!(changelists[0].id, ChangelistId::Default);
        assert_eq!(changelists[1].id, ChangelistId::Numbered(42));
        assert_eq!(changelists.len(), 2);
    }

    #[test]
    fn describe_indexed_files_map_to_changelist() {
        let records = parse_json_records(
            br#"{"code":"stat","change":"42","status":"submitted","user":"ExampleAuthor","client":"ExampleClientA","desc":"Shipped","depotFile0":"//SampleDepot/a.txt","action0":"edit","type0":"text","rev0":"3","depotFile1":"//SampleDepot/b.txt","action1":"add","type1":"text","haveRev1":"none","rev1":"1"}"#,
        )
        .expect("fixture should parse");

        let changelist = changelist_from_describe(&records).expect("describe should map");
        assert_eq!(changelist.id, ChangelistId::Numbered(42));
        assert_eq!(changelist.files.len(), 2);
        assert_eq!(changelist.files[0].depot_path, "//SampleDepot/a.txt");
        assert_eq!(changelist.files[0].base_revision, Some(3));
        assert_eq!(changelist.files[1].action, FileAction::Add);
        assert_eq!(changelist.files[1].base_revision, None);
    }

    #[test]
    fn opened_mapper_rejects_describe_indexed_records() {
        let records = parse_json_records(
            br#"{"code":"stat","change":"42","status":"pending","user":"ExampleAuthor","client":"ExampleClientA","desc":"x","depotFile0":"//SampleDepot/a.txt","action0":"edit","type0":"text","rev0":"1"}"#,
        )
        .expect("fixture should parse");

        let error = changed_files_from_opened(&records).expect_err("must not look empty");
        assert_eq!(error.field, "depotFile");
    }

    #[test]
    fn object_helper_accepts_numeric_scalars() {
        let mut object = Map::new();
        object.insert("code".into(), Value::String("stat".into()));
        object.insert("change".into(), Value::from(42));
        let bytes = serde_json::to_vec(&Value::Object(object)).expect("serialize fixture");
        let records = parse_json_records(&bytes).expect("numeric scalar should parse");
        assert_eq!(records[0].string("change").as_deref(), Some("42"));
    }
}
