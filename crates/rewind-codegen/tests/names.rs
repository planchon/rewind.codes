//! Names that become identifiers of `rewind:actions`: a server, a group of harness actions and a
//! harness action without a group are declared as `export const`, `export namespace` and `export
//! function`. Every such name an action set accepts must give declarations that parse; the reserved
//! words of JavaScript, which would not, are refused by the action set itself
//! (`rewind_core::RESERVED_WORDS`).

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use deno_ast::{MediaType, ModuleSpecifier, ParseParams};
use rewind_codegen::generate;
use rewind_core::{Action, ActionRoute, ActionSet, McpServer, McpTransport, RESERVED_WORDS};
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
    "ActionContent",
    "$",
    "_",
    "$default",
    "Delete",
    "class_",
    "github",
];

/// An action set in which `name` is declared as a server, as a group of harness actions, or as a
/// harness action without a group; each action takes a string and returns an object.
fn action_sets(name: &str) -> [Result<ActionSet, rewind_core::ActionSetError>; 3] {
    let input = json!({
        "type": "object",
        "properties": { "q": { "type": "string" } },
        "required": ["q"]
    });
    let output = json!({ "type": "object", "properties": { "id": { "type": "string" } } });
    let action = |name: String, route: ActionRoute| {
        let mut action = Action::new(name, route, input.clone());
        action.output_schema = Some(output.clone());
        action.description = Some("Finds things.".into());
        action
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
            [action(
                format!("{name}.find"),
                ActionRoute::Mcp {
                    server: name.to_owned(),
                },
            )],
        ),
        ActionSet::new(
            [],
            [action(
                format!("{name}.ask"),
                ActionRoute::Harness {
                    group: Some(name.to_owned()),
                },
            )],
        ),
        ActionSet::new(
            [],
            [action(
                name.to_owned(),
                ActionRoute::Harness { group: None },
            )],
        ),
    ]
}

/// Parses TypeScript declarations: the error, if any, with the errors the parser recovered
/// from (strict mode's rules among them).
fn parse(text: &str) -> Result<(), String> {
    let parsed = deno_ast::parse_program(ParseParams {
        specifier: ModuleSpecifier::parse("file:///rewind-actions.d.ts").unwrap(),
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
            for action in &declarations.actions {
                if let Err(error) = parse(&action.typescript) {
                    panic!("{name}: {} does not parse: {error}", action.name);
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
        // Declared by an ES module, as `rewind:actions` declares its exports, the name does not
        // parse. (The parser of `.d.ts` files lets some through, such as `eval` and `static`;
        // tsgo refuses them.)
        let module = format!("export const {word} = {{}};\n");
        let parsed = deno_ast::parse_program(ParseParams {
            specifier: ModuleSpecifier::parse("file:///rewind-actions.mjs").unwrap(),
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
