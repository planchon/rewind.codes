//! Codegen on schemas written to make it slow. Schemas come from MCP servers and harnesses
//! ai-ptc does not control, and the server generates declarations inside its request handlers:
//! each case below took hours or more before its bound, and must now finish at once, with valid
//! TypeScript.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::Duration;

use deno_ast::{MediaType, ModuleSpecifier, ParseParams};
use ptc_codegen::{Declarations, generate};
use ptc_core::{McpServer, McpTransport, Tool, ToolRoute, Toolset};
use serde_json::{Map, Value, json};

/// Far more than any case takes, even in a debug build on a busy machine.
const PATIENCE: Duration = Duration::from_secs(60);

/// The declarations of a toolset whose tool `crm.find` takes `input`, generated within
/// [`PATIENCE`], and checked as TypeScript.
fn generate_for(input: Value) -> Declarations {
    generate_for_tools(vec![("find".to_owned(), input)])
}

/// The same, for tools of the server `crm` named and taking what `tools` says.
fn generate_for_tools(tools: Vec<(String, Value)>) -> Declarations {
    let server = McpServer::new(
        "crm",
        McpTransport::Http {
            url: "https://mcp.example.com/crm".into(),
        },
    );
    let tools = tools.into_iter().map(|(name, input)| {
        Tool::new(
            format!("crm.{name}"),
            ToolRoute::Mcp {
                server: "crm".into(),
            },
            input,
        )
    });
    let toolset = Toolset::new([server], tools).unwrap();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(generate(&toolset, &BTreeMap::new()));
    });
    let declarations = receiver
        .recv_timeout(PATIENCE)
        .expect("codegen did not finish in time");
    parse(&declarations.typescript);
    declarations
}

fn parse(text: &str) {
    deno_ast::parse_program(ParseParams {
        specifier: ModuleSpecifier::parse("file:///ptc-tools.d.ts").unwrap(),
        text: text.into(),
        media_type: MediaType::Dts,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .unwrap_or_else(|error| panic!("not valid TypeScript: {error}"));
}

/// An object holding an object, `depth` times: each level too long for one line.
fn nested_objects(depth: usize) -> Value {
    let mut schema = json!({ "type": "string" });
    for level in 0..depth {
        let name = format!("a_property_of_level_{level}");
        schema =
            json!({ "type": "object", "properties": { name.clone(): schema }, "required": [name] });
    }
    schema
}

#[test]
fn deeply_nested_objects_are_written_once() {
    // Each object tried the one-line form of everything it holds before writing it out on
    // several lines: twice the work per level.
    let declarations = generate_for(nested_objects(48));
    assert!(
        declarations
            .typescript
            .contains("{ a_property_of_level_0: string }")
    );
}

#[test]
fn named_types_that_refer_to_each_other_many_times_over_stay_cheap_in_the_prompt() {
    // `A0 = A1 | B1`, `A1 = A2 | B2`… and each `B` refers to the next `A` and `B` too: the
    // prompt writes named types out where they are used, twice the work per level.
    let levels = 48;
    let mut definitions = Map::new();
    for level in 0..levels {
        let next = if level + 1 == levels {
            json!({ "type": "string" })
        } else {
            json!({ "anyOf": [
                { "$ref": format!("#/$defs/a{}", level + 1) },
                { "$ref": format!("#/$defs/b{}", level + 1) }
            ] })
        };
        definitions.insert(
            format!("b{level}"),
            json!({ "anyOf": [next.clone(), { "type": "null" }] }),
        );
        definitions.insert(format!("a{level}"), next);
    }
    let declarations = generate_for(json!({
        "type": "object",
        "properties": { "x": { "$ref": "#/$defs/a0" } },
        "required": ["x"],
        "$defs": definitions
    }));
    assert!(
        declarations
            .prompt
            .starts_with("import { crm } from \"ptc:tools\";\ncrm.find({ x: ")
    );
}

#[test]
fn many_definitions_with_the_same_name_are_named_in_linear_time() {
    // Every definition of every tool of the server wants the name `Item`: each name was looked
    // up through every name taken, and the next free number searched from 2 each time.
    let tools = (0..40)
        .map(|tool| {
            let mut properties = Map::new();
            let mut definitions = Map::new();
            for n in 0..500 {
                properties.insert(
                    format!("p{n}"),
                    json!({ "$ref": format!("#/$defs/d{n}/$defs/item") }),
                );
                definitions.insert(
                    format!("d{n}"),
                    json!({ "$defs": { "item": { "const": tool * 1000 + n } } }),
                );
            }
            let input = json!({ "type": "object", "properties": properties, "$defs": definitions });
            (format!("t{tool}"), input)
        })
        .collect();
    let declarations = generate_for_tools(tools);
    for name in [
        "Item",
        "T0Item",
        "T0Item2",
        "T0Item499",
        "T39Item",
        "T39Item499",
    ] {
        assert!(
            declarations
                .typescript
                .contains(&format!("export type {name} = ")),
            "{name}"
        );
    }
}

#[test]
fn a_huge_enum_is_written_in_linear_time() {
    // Each value was compared with every value before it. 80,000 values hold in 1 MiB.
    let values: Vec<Value> = (0..80_000).map(|n| json!(format!("v{n}"))).collect();
    let declarations = generate_for(json!({
        "type": "object", "properties": { "x": { "enum": values } }, "required": ["x"]
    }));
    assert!(declarations.typescript.contains("\"v0\" | \"v1\""));
    assert!(declarations.typescript.contains("\"v79999\""));
    // The prompt shows a few values.
    assert!(declarations.prompt.contains("\"v7\" | …"));
}

#[test]
fn a_definition_referred_to_many_times_is_read_once() {
    // Every `$ref` read again everything its target refers to: here, the whole document.
    let references: Vec<Value> = (0..20_000).map(|_| json!({ "$ref": "#" })).collect();
    let declarations = generate_for(json!({
        "type": "object",
        "properties": { "next": { "anyOf": references } }
    }));
    assert!(
        declarations
            .typescript
            .contains("export interface FindArgs {")
    );
}

#[test]
fn a_document_designating_too_many_definitions_keeps_its_references_unknown() {
    let schema = |count: usize| {
        let mut properties = Map::new();
        let mut definitions = Map::new();
        for n in 0..count {
            properties.insert(format!("p{n}"), json!({ "$ref": format!("#/$defs/d{n}") }));
            // Each definition refers to all the others through the root.
            definitions.insert(
                format!("d{n}"),
                json!({ "anyOf": [{ "type": "string" }, { "$ref": "#" }] }),
            );
        }
        json!({ "type": "object", "properties": properties, "$defs": definitions })
    };
    // 511 definitions and the root: still named types.
    let declarations = generate_for(schema(511));
    assert!(
        declarations
            .typescript
            .contains("export type D510 = string | FindArgs;")
    );
    // One more: each reference is `unknown`.
    let declarations = generate_for(schema(512));
    assert!(!declarations.typescript.contains("export type D"));
    assert!(declarations.typescript.contains("p511?: unknown;"));
}

#[test]
fn a_document_larger_than_a_mebibyte_is_unknown() {
    let description = "x".repeat(1 << 20);
    let declarations = generate_for(json!({
        "type": "object",
        "properties": { "q": { "type": "string", "description": description } }
    }));
    assert!(
        declarations
            .typescript
            .contains("find(args?: unknown): Promise<ToolContent>;"),
        "{}",
        declarations.typescript
    );
}
