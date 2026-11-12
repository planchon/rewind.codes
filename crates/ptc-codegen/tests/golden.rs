//! Golden tests: each directory of `tests/golden/` holds a toolset (`toolset.json`) and what
//! codegen must generate from it, byte for byte: `ptc-tools.d.ts`, `prompt.txt`, and
//! `tools.txt` (each tool on its own). `PTC_CODEGEN_BLESS=1` writes the outputs again.
//!
//! `toolset.json` describes the toolset in a form of this test's own (codegen does not read the
//! stored format):
//!
//! ```json
//! {
//!   "servers": ["github"],
//!   "tools": [
//!     { "name": "github.list_issues", "route": "mcp", "description": "…", "input": {…}, "output": {…} },
//!     { "name": "ask_user", "route": "host", "input": {…} },
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
use ptc_codegen::{Declarations, HTTP_TOOL_REFERENCE, generate};
use ptc_core::{Builtin, McpServer, McpTransport, Shape, Tool, ToolRoute, Toolset};
use serde_json::Value;

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn cases() -> Vec<PathBuf> {
    let mut cases: Vec<PathBuf> = fs::read_dir(golden_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join("toolset.json").exists())
        .collect();
    cases.sort();
    assert!(cases.len() >= 4, "the golden cases are missing");
    cases
}

/// The toolset and the observed shapes of a case.
fn load(case: &Path) -> (Toolset, BTreeMap<String, Shape>) {
    let text = fs::read_to_string(case.join("toolset.json")).unwrap();
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
    let tools = document["tools"].as_array().unwrap().iter().map(|tool| {
        let name = tool["name"].as_str().unwrap();
        let route = match tool["route"].as_str().unwrap() {
            "mcp" => ToolRoute::Mcp {
                server: name.split_once('.').unwrap().0.to_owned(),
            },
            "host" => ToolRoute::Host {
                group: name.split_once('.').map(|(group, _)| group.to_owned()),
            },
            "http" => ToolRoute::Builtin(Builtin::Http {
                allowed_hosts: serde_json::from_value(tool["hosts"].clone()).unwrap(),
            }),
            "webhook" => ToolRoute::Builtin(Builtin::Webhook),
            other => panic!("unknown route {other}"),
        };
        let mut pinned = Tool::new(name, route, tool.get("input").cloned().unwrap_or_default());
        pinned.description = tool["description"].as_str().map(str::to_owned);
        pinned.output_schema = tool.get("output").cloned();
        pinned
    });
    let toolset = Toolset::new(servers, tools).unwrap();
    let observed = document
        .get("observed")
        .and_then(Value::as_object)
        .map(|observed| {
            observed
                .iter()
                .map(|(tool, results)| {
                    let shape = results
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(Shape::of)
                        .fold(Shape::Unknown, Shape::merge);
                    (tool.clone(), shape)
                })
                .collect()
        })
        .unwrap_or_default();
    (toolset, observed)
}

fn outputs(declarations: &Declarations) -> Vec<(&'static str, String)> {
    let tools: Vec<String> = declarations
        .tools
        .iter()
        .map(|tool| format!("== {}\n{}\n\n{}", tool.name, tool.prompt, tool.typescript))
        .collect();
    vec![
        ("ptc-tools.d.ts", declarations.typescript.clone()),
        ("prompt.txt", declarations.prompt.clone()),
        ("tools.txt", tools.join("\n")),
    ]
}

#[test]
fn generated_declarations_match_the_golden_files() {
    let bless = std::env::var_os("PTC_CODEGEN_BLESS").is_some();
    let mut stale = Vec::new();
    for case in cases() {
        let (toolset, observed) = load(&case);
        let declarations = generate(&toolset, &observed);
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
        "generated declarations differ (PTC_CODEGEN_BLESS=1 writes them again):\n{}",
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
        let (toolset, observed) = load(&case);
        let declarations = generate(&toolset, &observed);
        parse("ptc-tools.d.ts", &declarations.typescript);
        for tool in &declarations.tools {
            parse(&format!("{}.d.ts", tool.name), &tool.typescript);
        }
    }
}

#[test]
fn generation_is_deterministic_whatever_the_order_of_keys() {
    for case in cases() {
        let (toolset, observed) = load(&case);
        let first = generate(&toolset, &observed);
        assert_eq!(first, generate(&toolset, &observed));

        // The same toolset, every object's keys reversed.
        let reversed_tools = toolset.tools().iter().rev().map(|tool| {
            let mut tool = tool.clone();
            tool.input_schema = reverse_keys(&tool.input_schema);
            tool.output_schema = tool.output_schema.as_ref().map(reverse_keys);
            tool
        });
        let reversed = Toolset::new(toolset.servers().to_vec(), reversed_tools).unwrap();
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
fn the_declarations_of_a_toolset_with_the_http_tool_start_with_its_reference() {
    for case in cases() {
        let (toolset, observed) = load(&case);
        let typescript = generate(&toolset, &observed).typescript;
        let http_tool = toolset.tool("http").is_some();
        assert_eq!(
            typescript.starts_with(&format!("{HTTP_TOOL_REFERENCE}\n")),
            http_tool,
            "{}",
            case.display()
        );
        assert_eq!(
            typescript.matches(HTTP_TOOL_REFERENCE).count(),
            usize::from(http_tool)
        );
        // Every toolset declares the module, the empty one included.
        assert!(
            typescript.contains("\ndeclare module \"ptc:tools\" {\n"),
            "{}",
            case.display()
        );
    }
}

/// The type-checker learns that a script has `fetch` from this line only; `ptc-check` holds the
/// same literal and records it in the spec, which both crates are checked against.
#[test]
fn the_http_tool_reference_is_the_one_the_spec_records() {
    let spec = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../archi/04-tools.md");
    let spec = fs::read_to_string(spec).unwrap();
    let row = spec
        .lines()
        .find(|line| line.starts_with("| Sandbox globals |"))
        .expect("04 lists what tsgo sees");
    assert!(
        row.contains(&format!(
            "`ptc-tools.d.ts` then starts with `{HTTP_TOOL_REFERENCE}`"
        )),
        "archi/04-tools.md does not record {HTTP_TOOL_REFERENCE:?}: {row}"
    );
    assert_eq!(
        HTTP_TOOL_REFERENCE,
        r#"/// <reference path="fetch.d.ts" />"#
    );
}
