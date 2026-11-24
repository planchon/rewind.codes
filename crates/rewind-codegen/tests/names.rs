//! Names that become identifiers of `rewind:tools`: a server, a host-tool group and a host tool
//! without a group are declared as `export const`, `export namespace` and `export function`.
//! Every such name an action set accepts must give declarations that parse; the reserved words of
//! JavaScript, which would not, are refused by the action set itself (`rewind_core::RESERVED_WORDS`).

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use deno_ast::{MediaType, ModuleSpecifier, ParseParams};
use rewind_codegen::generate;
use rewind_core::{ActionSet, McpServer, McpTransport, RESERVED_WORDS, Tool, ToolRoute};
use serde_json::json;

/// Names worth trying besides the reserved words: the contextual keywords of JavaScript and
/// TypeScript, the global names, the helper types of the declarations, and ordinary names.
const OTHER_NAMES: &[&str] = &[
    "abstract",
    "accessor",
    "any",
    "as",
    "asserts",
    "async",
    "bigint",
    "boolean",
    "constructor",
    "declare",
    "exports",
    "from",
    "get",
    "global",
    "globalThis",
    "infer",
    "Infinity",
    "is",
    "keyof",
    "module",
    "NaN",
    "namespace",
    "never",
    "number",
    "object",
    "of",
    "out",
    "override",
    "readonly",
    "require",
    "satisfies",
    "set",
    "string",
    "symbol",
    "type",
    "undefined",
    "unique",
    "unknown",
    "using",
    "ContentBlock",
    "Promise",
    "ToolContent",
    "$",
    "_",
    "$default",
    "Delete",
    "class_",
    "github",
];

/// An action set in which `name` is declared as a server, as a host-tool group, or as a host tool
/// without a group; each tool takes a string and returns an object.
fn action_sets(name: &str) -> [Result<ActionSet, rewind_core::ActionSetError>; 3] {
    let input = json!({
        "type": "object",
        "properties": { "q": { "type": "string" } },
        "required": ["q"]
    });
    let output = json!({ "type": "object", "properties": { "id": { "type": "string" } } });
    let tool = |name: String, route: ToolRoute| {
        let mut tool = Tool::new(name, route, input.clone());
        tool.output_schema = Some(output.clone());
        tool.description = Some("Finds things.".into());
        tool
    };
    let server = McpServer::new(
        name,
        McpTransport::Http {
            url: "https://mcp.example.com/mcp".into(),
        },
    );
    [
        ActionSet::new(
            [server],
            [tool(
                format!("{name}.find"),
                ToolRoute::Mcp {
                    server: name.to_owned(),
                },
            )],
        ),
        ActionSet::new(
            [],
            [tool(
                format!("{name}.ask"),
                ToolRoute::Host {
                    group: Some(name.to_owned()),
                },
            )],
        ),
        ActionSet::new([], [tool(name.to_owned(), ToolRoute::Host { group: None })]),
    ]
}

/// Parses TypeScript declarations: the error, if any, with the errors the parser recovered
/// from (strict mode's rules among them).
fn parse(text: &str) -> Result<(), String> {
    let parsed = deno_ast::parse_program(ParseParams {
        specifier: ModuleSpecifier::parse("file:///rewind-tools.d.ts").unwrap(),
        text: text.into(),
        media_type: MediaType::Dts,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .map_err(|error| error.to_string())?;
    match parsed.diagnostics().first() {
        Some(diagnostic) => Err(diagnostic.to_string()),
        None => Ok(()),
    }
}

#[test]
fn the_declarations_of_every_accepted_name_parse() {
    let mut accepted = 0;
    for name in RESERVED_WORDS.iter().chain(OTHER_NAMES) {
        for action_set in action_sets(name) {
            let Ok(action_set) = action_set else { continue };
            let declarations = generate(&action_set, &BTreeMap::new());
            assert!(
                declarations.typescript.contains(&format!(" {name}")),
                "{name} is declared:\n{}",
                declarations.typescript
            );
            if let Err(error) = parse(&declarations.typescript) {
                panic!(
                    "{name}: the declarations do not parse: {error}\n{}",
                    declarations.typescript
                );
            }
            for tool in &declarations.tools {
                if let Err(error) = parse(&tool.typescript) {
                    panic!("{name}: {} does not parse: {error}", tool.name);
                }
            }
            accepted += 1;
        }
    }
    assert_eq!(
        accepted,
        OTHER_NAMES.len() * 3,
        "every other name is accepted"
    );
}

#[test]
fn reserved_words_are_refused_before_they_reach_the_declarations() {
    for word in RESERVED_WORDS {
        for action_set in action_sets(word) {
            assert!(action_set.is_err(), "{word} is refused");
        }
        // Declared by an ES module, as `rewind:tools` declares its exports, the name does not
        // parse. (The parser of `.d.ts` files lets some through, such as `eval` and `static`;
        // tsgo refuses them.)
        let module = format!("export const {word} = {{}};\n");
        let parsed = deno_ast::parse_program(ParseParams {
            specifier: ModuleSpecifier::parse("file:///rewind-tools.mjs").unwrap(),
            text: module.into(),
            media_type: MediaType::Mjs,
            capture_tokens: false,
            scope_analysis: false,
            maybe_syntax: None,
        });
        assert!(
            parsed.map_or(true, |parsed| !parsed.diagnostics().is_empty()),
            "{word} would not parse"
        );
    }
}
