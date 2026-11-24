//! Reading what the user gives: files, JSON values, labels, action set references.

use std::path::{Path, PathBuf};

use rewind_api::common::Labels;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::CliError;

/// A text file.
pub(crate) fn read_text(path: &Path) -> Result<String, CliError> {
    std::fs::read_to_string(path).map_err(|source| CliError::file(path, source))
}

/// A JSON file, read into `T`.
pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, CliError> {
    let text = read_text(path)?;
    serde_json::from_str(&text)
        .map_err(|error| CliError::usage(format!("{}: {error}", path.display())))
}

/// A JSON value given on the command line: the JSON itself, or `@file.json`.
pub(crate) fn json_value(what: &str, text: &str) -> Result<Value, CliError> {
    match text.strip_prefix('@') {
        Some(path) => read_json(Path::new(path)),
        None => serde_json::from_str(text)
            .map_err(|error| CliError::usage(format!("{what} is not JSON ({error}): {text}"))),
    }
}

/// Labels given as `key=value`, or `key:value`.
pub(crate) fn labels(pairs: &[String]) -> Result<Labels, CliError> {
    let mut labels = Labels::new();
    for pair in pairs {
        let (key, value) = split_label(pair)?;
        labels.insert(key.to_owned(), value.to_owned());
    }
    Ok(labels)
}

/// A label filter as the API takes it: `key:value`.
pub(crate) fn label_filter(pair: &str) -> Result<String, CliError> {
    let (key, value) = split_label(pair)?;
    Ok(format!("{key}:{value}"))
}

fn split_label(pair: &str) -> Result<(&str, &str), CliError> {
    pair.split_once('=')
        .or_else(|| pair.split_once(':'))
        .filter(|(key, _)| !key.is_empty())
        .ok_or_else(|| CliError::usage(format!("a label is key=value, not {pair:?}")))
}

/// What `--action-set` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActionSetRef {
    /// A registered action set: its digest, 64 lowercase hex digits.
    Id(String),
    /// An action set file: `POST /v1/action-sets`'s body.
    File(PathBuf),
}

impl ActionSetRef {
    /// A digest is an id; anything else is a file, which must exist.
    pub(crate) fn parse(text: &str) -> Result<Self, CliError> {
        let path = Path::new(text);
        if path.exists() {
            return Ok(ActionSetRef::File(path.to_owned()));
        }
        if text.len() == 64
            && text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Ok(ActionSetRef::Id(text.to_owned()));
        }
        Err(CliError::usage(format!(
            "--action-set {text}: no such file, and not the id of an action set (64 hex digits)"
        )))
    }
}

/// `--secret server=scope/name`, repeated: the secrets a run reads, by server. The server checks
/// the references.
pub(crate) fn attachments(pairs: &[String]) -> Result<rewind_api::secrets::Attachments, CliError> {
    pairs
        .iter()
        .map(|pair| match pair.split_once('=') {
            Some((server, secret)) if !server.is_empty() && !secret.is_empty() => {
                Ok((server.to_owned(), secret.to_owned()))
            }
            _ => Err(CliError::Usage(format!(
                "--secret {pair:?}: expected <server>=<scope>/<name>, such as github=ns/github or \
                 zendesk=org/zendesk"
            ))),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn json_values_are_inline_or_in_a_file() {
        assert_eq!(
            json_value("--input", r#"{"repo":"acme/api"}"#).unwrap(),
            json!({"repo": "acme/api"})
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.json");
        std::fs::write(&path, "[1, 2]").unwrap();
        assert_eq!(
            json_value("--input", &format!("@{}", path.display())).unwrap(),
            json!([1, 2])
        );
        let error = json_value("--input", "{oops").unwrap_err();
        assert!(error.to_string().contains("--input is not JSON"), "{error}");
        assert!(json_value("--input", "@/nonexistent.json").is_err());
    }

    #[test]
    fn labels_take_both_separators() {
        let parsed = labels(&["agent=triage".into(), "conversation:c-42".into()]).unwrap();
        assert_eq!(parsed.get("agent").unwrap(), "triage");
        assert_eq!(parsed.get("conversation").unwrap(), "c-42");
        assert_eq!(label_filter("agent=triage").unwrap(), "agent:triage");
        assert!(labels(&["nothing".into()]).is_err());
        assert!(labels(&["=x".into()]).is_err());
    }

    #[test]
    fn an_action_set_is_an_id_or_a_file() {
        let id = "9f2c4e1a07b3d5f8a1c2e3b4d5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4";
        assert_eq!(
            ActionSetRef::parse(id).unwrap(),
            ActionSetRef::Id(id.into())
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("action_set.json");
        std::fs::write(&path, "{}").unwrap();
        assert_eq!(
            ActionSetRef::parse(path.to_str().unwrap()).unwrap(),
            ActionSetRef::File(path)
        );
        assert!(ActionSetRef::parse("missing.json").is_err());
    }
}
