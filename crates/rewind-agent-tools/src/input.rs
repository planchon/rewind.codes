//! The arguments of a tool call, read as the SDK reads them: what is wrong is said to the model
//! as `{tool}: {message}`.

use serde_json::{Map, Value};

/// A mistake in the arguments: `{tool}: {message}`.
#[derive(Debug)]
pub(crate) struct InputError(pub(crate) String);

impl InputError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

pub(crate) type Values = Map<String, Value>;

/// The arguments: an object, or its JSON text; nothing is an empty object.
pub(crate) fn parse(name: &str, input: &Value) -> Result<Values, InputError> {
    let values = match input {
        Value::String(text) if text.trim().is_empty() => return Ok(Map::new()),
        Value::String(text) => serde_json::from_str(text)
            .map_err(|_| InputError::new(format!("the arguments of {name} are not valid JSON.")))?,
        other => other.clone(),
    };
    match values {
        Value::Null => Ok(Map::new()),
        Value::Object(values) => Ok(values),
        _ => Err(InputError::new(format!(
            "the arguments of {name} must be an object."
        ))),
    }
}

pub(crate) fn require_string(values: &Values, key: &str) -> Result<String, InputError> {
    match values.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(InputError::new(format!(
            "`{key}` must be a non-empty string."
        ))),
    }
}

pub(crate) fn optional_string(values: &Values, key: &str) -> Result<Option<String>, InputError> {
    match values.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        _ => Err(InputError::new(format!(
            "`{key}` must be a non-empty string."
        ))),
    }
}

/// `secrets`: the secrets a run reads, by MCP server or API, each `org/<name>` or `ns/<name>`.
/// The API checks the references; this checks their shape.
pub(crate) fn optional_attachments(
    values: &Values,
) -> Result<Option<std::collections::BTreeMap<String, String>>, InputError> {
    let invalid = || {
        InputError::new(
            "`secrets` must be an object of secrets by MCP server or API, each \"org/<name>\" or \"ns/<name>\", such as {\"github\": \"ns/github\"}.",
        )
    };
    match values.get("secrets") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(secrets)) => secrets
            .iter()
            .map(|(server, secret)| match secret {
                Value::String(secret) if !secret.is_empty() => Ok((server.clone(), secret.clone())),
                _ => Err(invalid()),
            })
            .collect::<Result<_, _>>()
            .map(Some),
        Some(_) => Err(invalid()),
    }
}

pub(crate) fn optional_strings(
    values: &Values,
    key: &str,
) -> Result<Option<Vec<String>>, InputError> {
    let invalid = || InputError::new(format!("`{key}` must be an array of non-empty strings."));
    match values.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(text) if !text.is_empty() => Ok(text.clone()),
                _ => Err(invalid()),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(invalid()),
    }
}

pub(crate) fn require_object<'v>(values: &'v Values, key: &str) -> Result<&'v Value, InputError> {
    match values.get(key) {
        Some(value @ Value::Object(_)) => Ok(value),
        _ => Err(InputError::new(format!(
            "`{key}` must be a JSON Schema: an object."
        ))),
    }
}

pub(crate) fn optional_index(values: &Values, key: &str) -> Result<Option<u64>, InputError> {
    match values.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| InputError::new(format!("`{key}` must be an integer, 0 or more."))),
    }
}

/// Tests as the model gave them: each an object (in full, or `{ from, expect }`) or a digest.
/// The server checks them.
pub(crate) fn optional_tests(values: &Values, key: &str) -> Result<Option<Vec<Value>>, InputError> {
    match values.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) if items.iter().all(is_test_input) => Ok(Some(items.clone())),
        Some(_) => Err(InputError::new(format!(
            "`{key}` must be an array of tests: each one an object, or the digest of a recorded test as a string."
        ))),
    }
}

fn is_test_input(value: &Value) -> bool {
    matches!(value, Value::String(text) if !text.is_empty()) || value.is_object()
}

pub(crate) fn optional_updates(values: &Values) -> Result<Option<Vec<Value>>, InputError> {
    let invalid = || {
        InputError::new(
            "`update` must be an array of { name, reason, test?, expect? }: the name of an inherited test, why it changes, and the test that replaces it or its new expectation.",
        )
    };
    let items = match values.get("update") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(invalid()),
    };
    let valid = items.iter().all(|item| {
        item.is_object()
            && item["name"].is_string()
            && item["reason"]
                .as_str()
                .is_some_and(|reason| !reason.trim().is_empty())
            && item.get("test").is_none_or(is_test_input)
            && item.get("expect").is_none_or(Value::is_object)
    });
    if valid {
        Ok(Some(items.clone()))
    } else {
        Err(invalid())
    }
}

pub(crate) fn optional_retirements(values: &Values) -> Result<Option<Vec<Value>>, InputError> {
    let invalid = || {
        InputError::new(
            "`retire` must be an array of { names, reason }: the names of inherited tests, and why they no longer hold.",
        )
    };
    let items = match values.get("retire") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(invalid()),
    };
    let valid = items.iter().all(|item| {
        item.is_object()
            && item["names"]
                .as_array()
                .is_some_and(|names| names.iter().all(Value::is_string))
            && item["reason"]
                .as_str()
                .is_some_and(|reason| !reason.trim().is_empty())
    });
    if valid {
        Ok(Some(items.clone()))
    } else {
        Err(invalid())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn inputs_are_objects_or_their_json() {
        assert!(parse("get_run", &json!("{\"run_id\":\"r\"}")).is_ok());
        assert!(parse("get_run", &json!("")).is_ok_and(|values| values.is_empty()));
        assert!(parse("get_run", &Value::Null).is_ok_and(|values| values.is_empty()));
        assert_eq!(
            parse("get_run", &json!([1])).err().map(|error| error.0),
            Some("the arguments of get_run must be an object.".to_owned())
        );
        assert_eq!(
            parse("get_run", &json!("{")).err().map(|error| error.0),
            Some("the arguments of get_run are not valid JSON.".to_owned())
        );
    }

    #[test]
    fn each_kind_of_argument_says_what_it_must_be() {
        let values = parse(
            "x",
            &json!({ "s": "", "n": -1, "a": [""], "o": "x", "t": [3], "u": [{ "name": "a" }], "r": 1 }),
        )
        .unwrap_or_default();
        let message = |error: InputError| error.0;
        assert_eq!(
            require_string(&values, "s").map_err(message).unwrap_err(),
            "`s` must be a non-empty string."
        );
        assert_eq!(
            optional_index(&values, "n").map_err(message).unwrap_err(),
            "`n` must be an integer, 0 or more."
        );
        assert_eq!(
            optional_strings(&values, "a").map_err(message).unwrap_err(),
            "`a` must be an array of non-empty strings."
        );
        assert_eq!(
            require_object(&values, "o").map_err(message).unwrap_err(),
            "`o` must be a JSON Schema: an object."
        );
        assert!(optional_tests(&values, "t").is_err());
        assert!(optional_string(&values, "missing").is_ok_and(|value| value.is_none()));
    }
}
