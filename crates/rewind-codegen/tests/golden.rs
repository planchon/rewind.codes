//! Golden tests: each directory of `tests/golden/` holds an action set (`action_set.json`) and what
//! codegen must generate from it, byte for byte: `rewind-actions.d.ts`, `prompt.txt`, and
//! `actions.txt` (each action on its own). `REWIND_CODEGEN_BLESS=1` writes the outputs again.
//!
//! `action_set.json` describes the action set in a form of this test's own (codegen does not read the
//! stored format):
//!
//! ```json
//! {
//!   "servers": ["github"],
//!   "actions": [
//!     { "name": "github.list_issues", "route": "mcp", "description": "…", "input": {…}, "output": {…} },
//!     { "name": "ask_user", "route": "harness", "input": {…} },
//!     { "name": "http", "route": "http", "hosts": ["api.example.com"] },
//!     { "name": "webhook", "route": "webhook" }
//!   ],
//!   "observed": { "ask_user": [<recorded result>, …] }
//! }
//! ```

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use deno_ast::{MediaType, ModuleSpecifier, ParseParams};
use rewind_codegen::{Declarations, HTTP_ACTION_REFERENCE, generate};
use rewind_core::{Action, ActionRoute, ActionSet, Builtin, McpServer, McpTransport, Shape};
use serde_json::Value;

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn cases() -> Vec<PathBuf> {
    let mut cases: Vec<PathBuf> = fs::read_dir(golden_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join("action_set.json").exists())
        .collect();
    cases.sort();
    assert!(cases.len() >= 4, "the golden cases are missing");
    cases
}

/// The action set and the observed shapes of a case.
fn load(case: &Path) -> (ActionSet, BTreeMap<String, Shape>) {
    let text = fs::read_to_string(case.join("action_set.json")).unwrap();
    let document: Value = serde_json::from_str(&text).unwrap();
    let servers = document["servers"].as_array().unwrap().iter().map(|name| {
        let name = name.as_str().unwrap();
        McpServer::new(
            name,
            McpTransport::Http {
                url: format!("https://mcp.example.com/{name}"),
            },
        )
    });
    let actions = document["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|action| {
            let name = action["name"].as_str().unwrap();
            let route = match action["route"].as_str().unwrap() {
                "mcp" => ActionRoute::Mcp {
                    server: name.split_once('.').unwrap().0.to_owned(),
                },
                "harness" => ActionRoute::Harness {
                    group: name.split_once('.').map(|(group, _)| group.to_owned()),
                },
                "http" => ActionRoute::Builtin(Builtin::Http {
                    allowed_hosts: serde_json::from_value(action["hosts"].clone()).unwrap(),
                }),
                "webhook" => ActionRoute::Builtin(Builtin::Webhook),
                other => panic!("unknown route {other}"),
            };
            let mut pinned = Action::new(
                name,
                route,
                action.get("input").cloned().unwrap_or_default(),
            );
            pinned.description = action["description"].as_str().map(str::to_owned);
            pinned.output_schema = action.get("output").cloned();
            pinned
        });
    let action_set = ActionSet::new(servers, actions).unwrap();
    let observed = document
        .get("observed")
        .and_then(Value::as_object)
        .map(|observed| {
            observed
                .iter()
                .map(|(action, results)| {
                    let shape = results
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(Shape::of)
                        .fold(Shape::Unknown, Shape::merge);
                    (action.clone(), shape)
                })
                .collect()
        })
        .unwrap_or_default();
    (action_set, observed)
}

fn outputs(declarations: &Declarations) -> Vec<(&'static str, String)> {
    let actions: Vec<String> = declarations
        .actions
        .iter()
        .map(|action| {
            format!(
                "== {}\n{}\n\n{}",
                action.name, action.prompt, action.typescript
            )
        })
        .collect();
    vec![
        ("rewind-actions.d.ts", declarations.typescript.clone()),
        ("prompt.txt", declarations.prompt.clone()),
        ("actions.txt", actions.join("\n")),
    ]
}

#[test]
fn generated_declarations_match_the_golden_files() {
    let bless = std::env::var_os("REWIND_CODEGEN_BLESS").is_some();
    let mut stale = Vec::new();
    for case in cases() {
        let (action_set, observed) = load(&case);
        let declarations = generate(&action_set, &observed);
        for (file, generated) in outputs(&declarations) {
            let path = case.join(file);
            if bless {
                fs::write(&path, &generated).unwrap();
                continue;
            }
            let expected = fs::read_to_string(&path).unwrap_or_default();
            if expected != generated {
                stale.push(format!(
                    "{}:\n--- expected\n{expected}\n--- generated\n{generated}",
                    path.display()
                ));
            }
        }
    }
    assert!(
        stale.is_empty(),
        "generated declarations differ (REWIND_CODEGEN_BLESS=1 writes them again):\n{}",
        stale.join("\n")
    );
}

fn parse(name: &str, text: &str) {
    deno_ast::parse_program(ParseParams {
        specifier: ModuleSpecifier::parse(&format!("file:///{name}")).unwrap(),
        text: text.into(),
        media_type: MediaType::Dts,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .unwrap_or_else(|error| panic!("{name} is not valid TypeScript: {error}\n{text}"));
}

#[test]
fn generated_declarations_are_valid_typescript() {
    for case in cases() {
        let (action_set, observed) = load(&case);
        let declarations = generate(&action_set, &observed);
        parse("rewind-actions.d.ts", &declarations.typescript);
        for action in &declarations.actions {
            parse(&format!("{}.d.ts", action.name), &action.typescript);
        }
    }
}

#[test]
fn generation_is_deterministic_whatever_the_order_of_keys() {
    for case in cases() {
        let (action_set, observed) = load(&case);
        let first = generate(&action_set, &observed);
        assert_eq!(first, generate(&action_set, &observed));

        // The same action set, every object's keys reversed.
        let reversed_actions = action_set.actions().iter().rev().map(|action| {
            let mut action = action.clone();
            action.input_schema = reverse_keys(&action.input_schema);
            action.output_schema = action.output_schema.as_ref().map(reverse_keys);
            action
        });
        let reversed = ActionSet::new(action_set.servers().to_vec(), reversed_actions).unwrap();
        assert_eq!(first, generate(&reversed, &observed), "{}", case.display());
    }
}

fn reverse_keys(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries: Vec<(&String, &Value)> = object.iter().collect();
            entries.reverse();
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), reverse_keys(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(reverse_keys).collect()),
        other => other.clone(),
    }
}

#[test]
fn the_declarations_of_an_action_set_with_the_http_action_start_with_its_reference() {
    for case in cases() {
        let (action_set, observed) = load(&case);
        let typescript = generate(&action_set, &observed).typescript;
        let http_action = action_set.action("http").is_some();
        assert_eq!(
            typescript.starts_with(&format!("{HTTP_ACTION_REFERENCE}\n")),
            http_action,
            "{}",
            case.display()
        );
        assert_eq!(
            typescript.matches(HTTP_ACTION_REFERENCE).count(),
            usize::from(http_action)
        );
        // Every action set declares the module, the empty one included.
        assert!(
            typescript.contains("\ndeclare module \"rewind:actions\" {\n"),
            "{}",
            case.display()
        );
    }
}

/// The type-checker learns that a script has `fetch` from this line only; `rewind-check` holds the
/// same literal and records it in the spec, which both crates are checked against.
#[test]
fn the_http_action_reference_is_the_one_the_spec_records() {
    let spec = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../archi/04-tools.md");
    let spec = fs::read_to_string(spec).unwrap();
    let row = spec
        .lines()
        .find(|line| line.starts_with("| Sandbox globals |"))
        .expect("04 lists what tsgo sees");
    assert!(
        row.contains(&format!(
            "`rewind-actions.d.ts` then starts with `{HTTP_ACTION_REFERENCE}`"
        )),
        "archi/04-tools.md does not record {HTTP_ACTION_REFERENCE:?}: {row}"
    );
    assert_eq!(
        HTTP_ACTION_REFERENCE,
        r#"/// <reference path="fetch.d.ts" />"#
    );
}
