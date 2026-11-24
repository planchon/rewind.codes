//! Does a call still match its action? (`archi/04-tools.md`, "When an action set changes during a
//! run".) A run keeps the action set it started with, but the server behind it can change during a
//! long pause. Before each call, the dispatcher compares the pinned action with the tool the
//! server lists now.
//!
//! The call no longer matches, and is parked, when:
//!
//! - the server no longer lists the tool (removed or renamed);
//! - its input schema changed, and the call's args do not validate against the new one;
//! - it declared an output schema when it was pinned, and that schema changed or is gone: the
//!   code's code reads the result through it.
//!
//! An input schema that changed without refusing the args (a new optional property, a new
//! description), and an output schema the pinned action did not have, do not stop the call: the
//! code's code still fits. Schemas are compared in their canonical JSON (RFC 8785).

use rewind_core::{Action, canonical_json};
use serde_json::Value;

use crate::failure::bounded;

/// Why the call no longer matches the action, written for the harness developer and the model;
/// `None` when it still does. Cut at 2,000 characters: a refused argument is quoted in it.
pub(crate) fn mismatch(
    pinned: &Action,
    current: Option<&rmcp::model::Tool>,
    args: &Value,
) -> Option<String> {
    changed(pinned, current, args).map(|message| bounded(&message))
}

fn changed(pinned: &Action, current: Option<&rmcp::model::Tool>, args: &Value) -> Option<String> {
    let name = &pinned.name;
    let Some(current) = current else {
        return Some(format!(
            "{name} changed on its server: the server no longer lists the action {:?}",
            pinned.local_name()
        ));
    };

    let input = Value::Object((*current.input_schema).clone());
    if canonical_json(&input) != canonical_json(&pinned.input_schema) {
        match jsonschema::validator_for(&input) {
            Ok(validator) => {
                if let Some(error) = validator.iter_errors(args).next() {
                    let at = error.instance_path().to_string();
                    let at = if at.is_empty() {
                        String::new()
                    } else {
                        format!(" at {at}")
                    };
                    return Some(format!(
                        "{name} changed on its server: the call's arguments no longer match its \
                         input schema{at}: {error}"
                    ));
                }
            }
            Err(error) => {
                return Some(format!(
                    "{name} changed on its server: its new input schema cannot be read: {error}"
                ));
            }
        }
    }

    if let Some(pinned_output) = &pinned.output_schema {
        let output = current
            .output_schema
            .as_ref()
            .map(|schema| Value::Object((**schema).clone()));
        match output {
            None => {
                return Some(format!(
                    "{name} changed on its server: it no longer declares an output schema"
                ));
            }
            Some(output) if canonical_json(&output) != canonical_json(pinned_output) => {
                return Some(format!(
                    "{name} changed on its server: its output schema is not the one the code \
                     was checked against"
                ));
            }
            Some(_) => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rewind_core::ActionRoute;
    use serde_json::{Map, json};

    use super::*;

    fn object(value: Value) -> Arc<Map<String, Value>> {
        match value {
            Value::Object(object) => Arc::new(object),
            _ => unreachable!(),
        }
    }

    fn pinned(input: Value, output: Option<Value>) -> Action {
        let mut action = Action::new(
            "github.list_issues",
            ActionRoute::Mcp {
                server: "github".into(),
            },
            input,
        );
        action.output_schema = output;
        action
    }

    fn current(input: Value, output: Option<Value>) -> rmcp::model::Tool {
        let mut action = rmcp::model::Tool::new("list_issues", "List the issues.", object(input));
        action.output_schema = output.map(object);
        action
    }

    fn schema(state: &[&str]) -> Value {
        json!({
            "type": "object",
            "properties": { "repo": { "type": "string" }, "state": { "enum": state } },
            "required": ["repo"]
        })
    }

    #[test]
    fn an_unchanged_action_matches() {
        let action = pinned(
            schema(&["open", "closed"]),
            Some(json!({ "type": "object" })),
        );
        let same = current(
            schema(&["open", "closed"]),
            Some(json!({ "type": "object" })),
        );
        assert_eq!(
            mismatch(&action, Some(&same), &json!({ "repo": "a/b" })),
            None
        );
    }

    #[test]
    fn a_removed_action_does_not() {
        let action = pinned(schema(&["open"]), None);
        assert_eq!(
            mismatch(&action, None, &json!({})).unwrap(),
            "github.list_issues changed on its server: the server no longer lists the action \"list_issues\""
        );
    }

    #[test]
    fn a_new_input_schema_stops_only_the_calls_it_refuses() {
        let action = pinned(schema(&["open", "closed"]), None);
        let narrower = current(schema(&["open"]), None);
        assert_eq!(
            mismatch(
                &action,
                Some(&narrower),
                &json!({ "repo": "a/b", "state": "open" })
            ),
            None
        );
        let refused = mismatch(
            &action,
            Some(&narrower),
            &json!({ "repo": "a/b", "state": "closed" }),
        )
        .unwrap();
        assert!(
            refused.starts_with(
                "github.list_issues changed on its server: the call's arguments no longer match its input schema at /state:"
            ),
            "{refused}"
        );
    }

    #[test]
    fn the_message_holds_a_bounded_part_of_the_args() {
        let action = pinned(schema(&["open"]), None);
        let mut narrower = schema(&["open"]);
        narrower["properties"]["repo"]["maxLength"] = json!(3);
        let message = mismatch(
            &action,
            Some(&current(narrower, None)),
            &json!({ "repo": "x".repeat(100_000) }),
        )
        .unwrap();
        assert!(message.chars().count() <= 2001, "{}", message.len());
    }

    #[test]
    fn a_changed_or_dropped_output_schema_does_not_match() {
        let action = pinned(schema(&["open"]), Some(json!({ "type": "object" })));
        let changed = current(
            schema(&["open"]),
            Some(json!({ "type": "object", "properties": { "a": {} } })),
        );
        assert!(mismatch(&action, Some(&changed), &json!({ "repo": "a" })).is_some());
        let dropped = current(schema(&["open"]), None);
        assert!(mismatch(&action, Some(&dropped), &json!({ "repo": "a" })).is_some());

        // An action pinned without one returns its content, whatever it declares now.
        let content = pinned(schema(&["open"]), None);
        assert_eq!(
            mismatch(&content, Some(&changed), &json!({ "repo": "a" })),
            None
        );
    }
}
