//! JSON Schemas compiled once, whose problems are written for the model that wrote the value
//! (`archi/04-tools.md`, "Action sets and code generation").
//!
//! The sandbox checks tool args with it, inside the op, so its messages reach scripts and its
//! version is part of the determinism epoch. The engine checks with it what other runs and the
//! API send to a workflow: inputs and signals.

use serde_json::Value;

/// At most this many problems are listed in one message.
const MAX_PROBLEMS: usize = 5;

/// A JSON Schema, compiled.
pub enum Validator {
    /// `{}` or `true`: any value.
    Any,
    Schema(Box<jsonschema::Validator>),
    /// The schema cannot be used (an invalid schema, a `$ref` outside it): every value is
    /// refused with this message, rather than accepted unchecked.
    Unusable(String),
}

/// Why a value was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The schema cannot be used: why.
    Unusable(String),
    /// The value does not match: every problem, each at its place, sorted, the first five
    /// then how many more.
    Invalid(String),
}

impl std::fmt::Debug for Validator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Validator::Any => f.write_str("Validator::Any"),
            Validator::Schema(_) => f.write_str("Validator::Schema(..)"),
            Validator::Unusable(error) => write!(f, "Validator::Unusable({error:?})"),
        }
    }
}

impl Validator {
    /// Compiles a schema. No `$ref` is fetched from the network or the disk.
    pub fn compile(schema: &Value) -> Self {
        match schema {
            Value::Bool(true) => return Self::Any,
            Value::Object(object) if object.is_empty() => return Self::Any,
            _ => {}
        }
        match jsonschema::validator_for(schema) {
            Ok(validator) => Self::Schema(Box::new(validator)),
            Err(error) => Self::Unusable(error.to_string()),
        }
    }

    /// Why the schema cannot be used, if it cannot.
    pub fn unusable(&self) -> Option<&str> {
        match self {
            Self::Unusable(error) => Some(error),
            _ => None,
        }
    }

    /// Checks `value`. Each problem is at its place written from `root`, as a script writes it:
    /// `args`, `args.repo`, `args.labels[0]`, `args["a-b"]`. Problems are sorted, so that the
    /// message does not depend on the order the validator walks the schema in.
    pub fn check(&self, root: &str, value: &Value) -> Result<(), Refused> {
        let validator = match self {
            Self::Any => return Ok(()),
            Self::Unusable(error) => return Err(Refused::Unusable(error.clone())),
            Self::Schema(validator) => validator,
        };
        let mut problems: Vec<String> = validator
            .iter_errors(value)
            .map(|error| format!("{}: {error}", path(root, value, error.instance_path())))
            .collect();
        if problems.is_empty() {
            return Ok(());
        }
        problems.sort();
        problems.dedup();
        let more = problems.len().saturating_sub(MAX_PROBLEMS);
        problems.truncate(MAX_PROBLEMS);
        let mut message = problems.join("; ");
        if more > 0 {
            message.push_str(&format!(" (and {more} more)"));
        }
        Err(Refused::Invalid(message))
    }
}

/// A place in a value as a script writes it, from `root`.
fn path(root: &str, value: &Value, location: &jsonschema::paths::Location) -> String {
    let mut path = String::from(root);
    let mut value = Some(value);
    for segment in location.iter() {
        let key = segment.to_string();
        match value {
            Some(Value::Array(items)) => {
                path.push_str(&format!("[{key}]"));
                value = key.parse::<usize>().ok().and_then(|index| items.get(index));
            }
            _ => {
                if is_identifier(&key) {
                    path.push('.');
                    path.push_str(&key);
                } else {
                    path.push_str(&format!("[{}]", Value::String(key.clone())));
                }
                value = value.and_then(|value| value.get(&key));
            }
        }
    }
    path
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn problems_are_placed_sorted_and_cut() {
        let validator = Validator::compile(&json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string" },
                "labels": { "type": "array", "items": { "type": "string" } },
                "a-b": { "type": "integer" },
            },
            "required": ["repo"],
        }));
        assert_eq!(validator.check("input", &json!({ "repo": "x" })), Ok(()));
        let Err(Refused::Invalid(message)) =
            validator.check("input", &json!({ "repo": 1, "labels": [2], "a-b": "c" }))
        else {
            panic!("three problems");
        };
        assert_eq!(
            message,
            r#"input.labels[0]: 2 is not of type "string"; input.repo: 1 is not of type "string"; input["a-b"]: "c" is not of type "integer""#
        );
        let many = Validator::compile(&json!({ "type": "array", "items": { "type": "string" } }));
        let Err(Refused::Invalid(message)) = many.check("payload", &json!([1, 2, 3, 4, 5, 6, 7]))
        else {
            panic!("seven problems");
        };
        assert!(message.ends_with(" (and 2 more)"), "{message}");
    }

    #[test]
    fn empty_schemas_accept_anything_and_unusable_ones_nothing() {
        for schema in [json!({}), json!(true)] {
            assert_eq!(Validator::compile(&schema).check("x", &json!([1])), Ok(()));
        }
        let unusable = Validator::compile(&json!({ "$ref": "https://schemas.example.com/a.json" }));
        assert!(unusable.unusable().is_some());
        assert!(matches!(
            unusable.check("x", &json!(1)),
            Err(Refused::Unusable(_))
        ));
    }
}
