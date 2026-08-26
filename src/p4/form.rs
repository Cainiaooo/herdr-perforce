use std::{collections::BTreeMap, error::Error, fmt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangeForm {
    raw: String,
    fields: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeFormError {
    InvalidUtf8,
    MissingDescription,
    DuplicateDescription,
    InvalidField,
    InvalidDescription,
}

impl fmt::Display for ChangeFormError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidUtf8 => "the changelist form was not valid UTF-8",
            Self::MissingDescription => "the changelist form has no Description field",
            Self::DuplicateDescription => "the changelist form has more than one Description field",
            Self::InvalidField => "the changelist form contains an invalid field layout",
            Self::InvalidDescription => "the proposed description is empty or contains NUL",
        })
    }
}

impl Error for ChangeFormError {}

impl ChangeForm {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, ChangeFormError> {
        let raw = std::str::from_utf8(bytes)
            .map_err(|_| ChangeFormError::InvalidUtf8)?
            .to_owned();
        let mut fields = BTreeMap::<String, Vec<String>>::new();
        let mut active: Option<String> = None;

        for (line_index, raw_line) in raw.lines().enumerate() {
            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            let line = if line_index == 0 {
                line.strip_prefix('\u{feff}').unwrap_or(line)
            } else {
                line
            };
            if let Some(value) = line.strip_prefix('\t') {
                let name = active.as_ref().ok_or(ChangeFormError::InvalidField)?;
                fields
                    .get_mut(name)
                    .expect("active form field must exist")
                    .push(value.to_owned());
                continue;
            }
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((name, value)) = line.split_once(':') else {
                return Err(ChangeFormError::InvalidField);
            };
            if name.is_empty() || name.chars().any(char::is_whitespace) {
                return Err(ChangeFormError::InvalidField);
            }
            if fields.contains_key(name) {
                return Err(if name == "Description" {
                    ChangeFormError::DuplicateDescription
                } else {
                    ChangeFormError::InvalidField
                });
            }
            let initial = value.trim_start_matches([' ', '\t']);
            let mut values = Vec::new();
            if !initial.is_empty() {
                values.push(initial.to_owned());
            }
            fields.insert(name.to_owned(), values);
            active = Some(name.to_owned());
        }

        if !fields.contains_key("Description") {
            return Err(ChangeFormError::MissingDescription);
        }

        Ok(Self { raw, fields })
    }

    pub(crate) fn field(&self, name: &str) -> Option<String> {
        self.fields.get(name).map(|values| values.join("\n"))
    }

    pub(crate) fn preserved_fields(&self) -> BTreeMap<String, String> {
        const MODELED_FIELDS: [&str; 7] = [
            "Change",
            "Date",
            "Client",
            "User",
            "Status",
            "Description",
            "Files",
        ];
        self.fields
            .iter()
            .filter(|(name, _)| !MODELED_FIELDS.contains(&name.as_str()))
            .map(|(name, values)| (name.clone(), encode_values(values)))
            .collect()
    }

    pub(crate) fn replace_description(
        &self,
        description: &str,
    ) -> Result<Vec<u8>, ChangeFormError> {
        if description.trim().is_empty() || description.contains('\0') {
            return Err(ChangeFormError::InvalidDescription);
        }

        let newline = if self.raw.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let lines = self.raw.split_inclusive('\n').collect::<Vec<_>>();
        let description_index = lines
            .iter()
            .position(|line| line_body(line) == "Description:")
            .ok_or(ChangeFormError::MissingDescription)?;
        if lines
            .iter()
            .skip(description_index + 1)
            .any(|line| line_body(line) == "Description:")
        {
            return Err(ChangeFormError::DuplicateDescription);
        }

        let value_start = description_index + 1;
        let mut value_end = value_start;
        while value_end < lines.len() && line_body(lines[value_end]).starts_with('\t') {
            value_end += 1;
        }
        if value_end == value_start {
            return Err(ChangeFormError::InvalidField);
        }

        let mut updated = String::with_capacity(self.raw.len() + description.len());
        for line in &lines[..value_start] {
            updated.push_str(line);
        }
        for line in description
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .split('\n')
        {
            updated.push('\t');
            updated.push_str(line);
            updated.push_str(newline);
        }
        for line in &lines[value_end..] {
            updated.push_str(line);
        }
        Ok(updated.into_bytes())
    }
}

fn line_body(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

fn encode_values(values: &[String]) -> String {
    let mut encoded = String::new();
    for value in values {
        encoded.push_str(&value.len().to_string());
        encoded.push(':');
        encoded.push_str(value);
        encoded.push(';');
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORM: &str = "# comment\r\n\r\nChange:\t42\r\n\r\nClient:\tExampleClientA\r\n\r\nUser:\tExampleAuthor\r\n\r\nStatus:\tpending\r\n\r\nDescription:\r\n\tOld first line\r\n\tOld second line\r\n\r\nJobs:\r\n\tJOB-1\r\n\r\nType:\tpublic\r\n\r\nFiles:\r\n\t//SampleDepot/a.txt\r\n";

    #[test]
    fn replacement_changes_only_description_bytes_and_keeps_crlf() {
        let form = ChangeForm::parse(FORM.as_bytes()).expect("form");
        let updated = form
            .replace_description("New first line\nNew second line")
            .expect("replacement");
        let updated = String::from_utf8(updated).expect("UTF-8");

        assert!(updated.contains("Description:\r\n\tNew first line\r\n\tNew second line\r\n"));
        assert!(updated.contains("Jobs:\r\n\tJOB-1\r\n"));
        assert!(updated.contains("Files:\r\n\t//SampleDepot/a.txt\r\n"));
        assert!(!updated.contains("Old first line"));
    }

    #[test]
    fn semantic_fields_are_extracted_without_date_noise() {
        let form = ChangeForm::parse(FORM.as_bytes()).expect("form");
        assert_eq!(form.field("Change").as_deref(), Some("42"));
        assert_eq!(
            form.field("Description").as_deref(),
            Some("Old first line\nOld second line")
        );
        assert_eq!(
            form.preserved_fields().get("Type").map(String::as_str),
            Some("6:public;")
        );
        assert!(form.preserved_fields().contains_key("Jobs"));
        assert!(!form.preserved_fields().contains_key("Date"));
    }

    #[test]
    fn invalid_or_ambiguous_forms_fail_closed() {
        assert_eq!(
            ChangeForm::parse(b"Change:\t42\n"),
            Err(ChangeFormError::MissingDescription)
        );
        assert_eq!(
            ChangeForm::parse(b"Description:\n\tx\nDescription:\n\ty\n"),
            Err(ChangeFormError::DuplicateDescription)
        );
        let form = ChangeForm::parse(b"Description:\n\tx\n").expect("form");
        assert_eq!(
            form.replace_description(" \n\t"),
            Err(ChangeFormError::InvalidDescription)
        );
    }
}
