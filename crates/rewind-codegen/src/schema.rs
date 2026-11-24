//! Our JSON Schema → TypeScript converter (`archi/04-tools.md`, "Action sets and code
//! generation"): no maintained crate does it.
//!
//! It covers what tool schemas use: `type` (one or several), objects with `properties`,
//! `required`, `additionalProperties` and `patternProperties`, arrays with `items` and
//! `prefixItems` (or the older `items` list with `additionalItems`), `enum` and `const`,
//! `anyOf`, `oneOf` and `allOf`, `nullable`, and `$ref` into the same document (`#`, JSON
//! pointers, `$anchor`), with `$defs` and `definitions`. Every other keyword says nothing a
//! TypeScript type can express, and is ignored; the constraints worth knowing (`format`,
//! bounds, `pattern`, `default`) go to the JSDoc. A `$ref` the converter cannot resolve, such as
//! a remote document, is `unknown`.
//!
//! Choices:
//!
//! - Each `$ref` target becomes a named type of the tool's namespace, named after its last
//!   meaningful pointer segment (`#/$defs/issue-comment` → `IssueComment`). Two targets with
//!   the same definition, down to everything they refer to, share one name; a different type
//!   that wants a taken name gets the tool's name in front of it, then a number.
//! - An object whose `additionalProperties` is absent or `false` lists its properties and
//!   nothing else, so that a misspelled argument is caught at check time; with `true` or a
//!   schema, it gets an index signature. An object schema that lists no property at all (no
//!   `properties`, `patternProperties` nor `required`) is a map: `{ [key: string]: unknown }`.
//! - Properties are listed required first, each group by name; definitions and anchors are
//!   read in key order. The output does not depend on how the schema's JSON was parsed.
//! - Schemas come from servers and harnesses Rewind does not control, so the work is bounded: a
//!   document larger than [`MAX_DOCUMENT`] bytes is `unknown`, and one whose `$ref`s designate
//!   more than [`MAX_DEFINITIONS`] definitions has its references `unknown`. Each definition's
//!   identity is computed once, from a digest of each definition it reaches.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;

use rewind_core::{Digest, canonical_json};
use serde_json::{Map, Value, json};

/// The largest schema document converted, in bytes of JSON: 1 MiB. Its declarations would not
/// fit a model's context anyway. A larger document is `unknown`.
pub(crate) const MAX_DOCUMENT: usize = 1 << 20;

/// The definitions one schema document may designate with its `$ref`s. Beyond, its references
/// are `unknown`: comparing every definition with every other would take too long.
pub(crate) const MAX_DEFINITIONS: usize = 512;

use crate::ty::{Doc, Object, Property, Tuple, Ty};

/// The named types of one namespace of `rewind:tools`: an MCP server, a host-tool group, or a
/// bare host tool. Its tools' schemas share them.
#[derive(Debug, Default)]
pub(crate) struct Scope {
    pub(crate) declarations: Vec<Declaration>,
    /// By name: the index of the declaration.
    names: HashMap<String, usize>,
    /// By the identity of the definition: its pointer and everything it refers to.
    identities: BTreeMap<String, usize>,
    /// The number to try next after a prefixed name that is taken: `ListIssuesItem3`…
    numbers: HashMap<String, usize>,
}

/// `export interface Issue { … }` or `export type State = "open" | "closed"`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Declaration {
    pub(crate) name: String,
    pub(crate) ty: Ty,
    pub(crate) doc: Doc,
}

impl Scope {
    pub(crate) fn get(&self, name: &str) -> Option<&Declaration> {
        self.names.get(name).map(|&index| &self.declarations[index])
    }

    fn is_taken(&self, name: &str) -> bool {
        self.names.contains_key(name)
    }

    /// A free name: `wanted`, else `{prefix}{wanted}`, else `{prefix}{wanted}2`, `3`…: the
    /// smallest number free. Names are never freed, so the search starts where the last one
    /// ended.
    fn free_name(&mut self, wanted: &str, prefix: &str) -> String {
        if !self.is_taken(wanted) {
            return wanted.to_owned();
        }
        let prefixed = format!("{prefix}{wanted}");
        if !self.is_taken(&prefixed) {
            return prefixed;
        }
        let mut number = self.numbers.get(&prefixed).copied().unwrap_or(2);
        while self.is_taken(&format!("{prefixed}{number}")) {
            number += 1;
        }
        self.numbers.insert(prefixed.clone(), number + 1);
        format!("{prefixed}{number}")
    }

    /// Adds a declaration, under a free name; its index.
    fn declare(&mut self, declaration: Declaration) -> usize {
        let index = self.declarations.len();
        self.names.insert(declaration.name.clone(), index);
        self.declarations.push(declaration);
        index
    }
}

/// Which schema of a tool a document is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Args,
    Result,
}

/// Converts one schema document (a tool's input or output schema) to a type, adding the
/// named types it refers to to `scope`. `tool` is the tool's name inside its namespace, which
/// tells types of different tools apart when their names clash.
pub(crate) fn convert(scope: &mut Scope, document: &Value, tool: &str, role: Role) -> Ty {
    if !fits(document, MAX_DOCUMENT) {
        return Ty::Unknown;
    }
    let anchors = anchors(document);
    let definitions = Definitions::new(document, anchors);
    let mut converter = Converter {
        scope,
        document,
        prefix: pascal_case(tool),
        role,
        references: definitions.count() <= MAX_DEFINITIONS,
        definitions,
        declared: HashMap::new(),
        pending: Vec::new(),
        root: None,
    };
    let mut ty = converter.schema(document);
    // A document that refers to itself is its named type.
    if let Some(name) = converter.root.clone() {
        ty = Ty::Named(name);
    }
    // Definitions are converted after the type that found them, in the order they were found.
    let mut next = 0;
    while next < converter.pending.len() {
        let (index, pointer) = converter.pending[next].clone();
        next += 1;
        if let Some(target) = document.pointer(&pointer) {
            let body = converter.schema(target);
            let doc = doc_of(target);
            let declaration = &mut converter.scope.declarations[index];
            declaration.ty = body;
            declaration.doc = doc;
        }
    }
    ty
}

struct Converter<'a> {
    scope: &'a mut Scope,
    document: &'a Value,
    /// The tool's name in PascalCase, put in front of a type name that clashes.
    prefix: String,
    role: Role,
    /// Whether `$ref`s become named types: the document designates few enough definitions.
    references: bool,
    /// The definitions the document's `$ref`s designate.
    definitions: Definitions<'a>,
    /// The declaration of each definition of this document met so far, by pointer.
    declared: HashMap<String, usize>,
    /// Declarations whose body is still to convert: (index, pointer).
    pending: Vec<(usize, String)>,
    /// The name of the document itself, when it refers to itself (`#`).
    root: Option<String>,
}

impl Converter<'_> {
    fn schema(&mut self, schema: &Value) -> Ty {
        match schema {
            Value::Bool(true) => Ty::Unknown,
            Value::Bool(false) => Ty::Never,
            Value::Object(object) => self.object_schema(object),
            _ => Ty::Unknown,
        }
    }

    fn object_schema(&mut self, schema: &Map<String, Value>) -> Ty {
        let mut parts = Vec::new();
        if let Some(Value::String(reference)) = schema.get("$ref") {
            parts.push(self.reference(reference));
        }
        if let Some(Value::String(reference)) = schema
            .get("$dynamicRef")
            .or_else(|| schema.get("$recursiveRef"))
        {
            // Without dynamic scopes, a dynamic reference resolves like a static one.
            parts.push(self.reference(reference));
        }

        if let Some(value) = schema.get("const") {
            parts.push(Ty::Literal(value.clone()));
        } else if let Some(Value::Array(values)) = schema.get("enum") {
            parts.push(Ty::union(values.iter().cloned().map(Ty::Literal)));
        } else if let Some(base) = self.typed(schema) {
            parts.push(base);
        }

        for keyword in ["anyOf", "oneOf"] {
            if let Some(Value::Array(members)) = schema.get(keyword) {
                let members: Vec<Ty> = members.iter().map(|member| self.schema(member)).collect();
                parts.push(Ty::union(members));
            }
        }
        if let Some(Value::Array(members)) = schema.get("allOf") {
            for member in members {
                let ty = self.schema(member);
                parts.push(ty);
            }
        }

        let ty = Ty::intersection(parts);
        if schema.get("nullable") == Some(&Value::Bool(true)) {
            Ty::union([ty, Ty::Null])
        } else {
            ty
        }
    }

    /// The type from `type`, or from the keywords of one type when `type` is absent.
    fn typed(&mut self, schema: &Map<String, Value>) -> Option<Ty> {
        match schema.get("type") {
            Some(Value::String(name)) => Some(self.of_type(name, schema)),
            Some(Value::Array(names)) => {
                let members: Vec<Ty> = names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|name| self.of_type(name, schema))
                    .collect();
                Some(Ty::union(members))
            }
            _ => {
                let has = |keywords: &[&str]| keywords.iter().any(|key| schema.contains_key(*key));
                if has(&[
                    "properties",
                    "additionalProperties",
                    "patternProperties",
                    "required",
                ]) {
                    Some(self.object(schema))
                } else if has(&["items", "prefixItems"]) {
                    Some(self.array(schema))
                } else {
                    None
                }
            }
        }
    }

    fn of_type(&mut self, name: &str, schema: &Map<String, Value>) -> Ty {
        match name {
            "string" => Ty::String,
            "number" | "integer" => Ty::Number,
            "boolean" => Ty::Boolean,
            "null" => Ty::Null,
            "array" => self.array(schema),
            "object" => self.object(schema),
            _ => Ty::Unknown,
        }
    }

    fn object(&mut self, schema: &Map<String, Value>) -> Ty {
        let required: Vec<&str> = match schema.get("required") {
            Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        let mut properties: BTreeMap<String, Property> = BTreeMap::new();
        if let Some(Value::Object(listed)) = schema.get("properties") {
            for (name, property) in sorted(listed) {
                let ty = self.schema(property);
                properties.insert(
                    name.clone(),
                    Property {
                        name: name.clone(),
                        ty,
                        optional: !required.contains(&name.as_str()),
                        doc: doc_of(property),
                    },
                );
            }
        }
        for name in &required {
            properties
                .entry((*name).to_owned())
                .or_insert_with(|| Property {
                    name: (*name).to_owned(),
                    ty: Ty::Unknown,
                    optional: false,
                    doc: Doc::default(),
                });
        }

        let mut extra: Vec<Ty> = Vec::new();
        match schema.get("additionalProperties") {
            Some(Value::Bool(false)) => {}
            Some(additional @ (Value::Bool(true) | Value::Object(_))) => {
                extra.push(self.schema(additional));
            }
            _ => {
                // Without any property listed, the object is a map.
                if !schema.contains_key("properties")
                    && !schema.contains_key("patternProperties")
                    && required.is_empty()
                {
                    extra.push(Ty::Unknown);
                }
            }
        }
        if let Some(Value::Object(patterns)) = schema.get("patternProperties") {
            for (_, pattern) in sorted(patterns) {
                extra.push(self.schema(pattern));
            }
        }
        let index = if extra.is_empty() {
            None
        } else {
            // Every listed property must fit the index signature.
            let listed = properties.values().map(|property| property.ty.clone());
            let optional = properties
                .values()
                .any(|property| property.optional)
                .then_some(Ty::Undefined);
            Some(Box::new(Ty::union(
                extra.into_iter().chain(listed).chain(optional),
            )))
        };
        // Required properties first, each group by name.
        let (mut listed, optional): (Vec<Property>, Vec<Property>) = properties
            .into_values()
            .partition(|property| !property.optional);
        listed.extend(optional);
        Ty::Object(Object {
            properties: listed,
            index,
        })
    }

    fn array(&mut self, schema: &Map<String, Value>) -> Ty {
        // 2020-12: `prefixItems` and `items` for the rest. Before: `items` as a list and
        // `additionalItems` for the rest.
        let (prefix, rest) = match (schema.get("prefixItems"), schema.get("items")) {
            (Some(Value::Array(prefix)), rest) => (Some(prefix), rest),
            (None, Some(Value::Array(prefix))) => (Some(prefix), schema.get("additionalItems")),
            (_, items) => (None, items),
        };
        let Some(prefix) = prefix else {
            let item = rest.map_or(Ty::Unknown, |items| self.schema(items));
            return Ty::Array(Box::new(item));
        };
        let items: Vec<Ty> = prefix.iter().map(|item| self.schema(item)).collect();
        let min_items = schema
            .get("minItems")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0);
        let rest = match rest {
            Some(Value::Bool(false)) | None => None,
            Some(rest) => Some(Box::new(self.schema(rest))),
        };
        Ty::Tuple(Tuple {
            required: min_items.min(items.len()),
            items,
            rest,
        })
    }

    /// A named type for the target of a `$ref`, declared in the scope on first sight.
    fn reference(&mut self, reference: &str) -> Ty {
        if !self.references {
            return Ty::Unknown;
        }
        let Some(pointer) = self.pointer(reference) else {
            return Ty::Unknown;
        };
        if self.document.pointer(&pointer).is_none() {
            return Ty::Unknown;
        }
        if let Some(&index) = self.declared.get(&pointer) {
            return self.named(index, &pointer);
        }
        let identity = self.definitions.identity(&pointer);
        let index = match self.scope.identities.get(&identity) {
            Some(&index) => index,
            None => {
                let wanted = self.wanted_name(&pointer);
                let name = self.scope.free_name(&wanted, &self.prefix);
                let index = self.scope.declare(Declaration {
                    name,
                    ty: Ty::Unknown,
                    doc: Doc::default(),
                });
                self.scope.identities.insert(identity, index);
                self.pending.push((index, pointer.clone()));
                index
            }
        };
        self.declared.insert(pointer.clone(), index);
        self.named(index, &pointer)
    }

    /// The named type of a declaration, which `pointer` designates.
    fn named(&mut self, index: usize, pointer: &str) -> Ty {
        let name = self.scope.declarations[index].name.clone();
        if pointer.is_empty() {
            self.root = Some(name.clone());
        }
        Ty::Named(name)
    }

    /// The JSON pointer a reference designates in this document, if it is one of its own.
    fn pointer(&self, reference: &str) -> Option<String> {
        local_pointer(self.document, reference, &self.definitions.anchors)
    }

    fn wanted_name(&self, pointer: &str) -> String {
        let segments: Vec<String> = pointer.split('/').skip(1).map(unescape).collect();
        let meaningful = segments
            .iter()
            .rev()
            .find(|segment| !is_keyword(segment) && segment.parse::<u64>().is_err());
        match meaningful {
            Some(segment) => pascal_case(segment),
            None if pointer.is_empty() => {
                let role = match self.role {
                    Role::Args => "Args",
                    Role::Result => "Result",
                };
                format!("{}{role}", self.prefix)
            }
            None => format!("{}Type", self.prefix),
        }
    }
}

/// The JSON pointer of a same-document reference: `#`, `#/pointer`, `#anchor`, or the
/// document's own `$id` followed by one of these.
fn local_pointer(
    document: &Value,
    reference: &str,
    anchors: &BTreeMap<String, String>,
) -> Option<String> {
    let fragment = match reference.split_once('#') {
        Some(("", fragment)) => fragment,
        Some((base, fragment)) if document.get("$id").and_then(Value::as_str) == Some(base) => {
            fragment
        }
        Some(_) => return None,
        None if document.get("$id").and_then(Value::as_str) == Some(reference) => "",
        None => return None,
    };
    let fragment = percent_decode(fragment);
    if fragment.is_empty() || fragment.starts_with('/') {
        Some(fragment)
    } else {
        anchors.get(&fragment).cloned()
    }
}

/// The definitions of a document: what its `$ref`s designate, each read once.
struct Definitions<'a> {
    document: &'a Value,
    /// `$anchor` → JSON pointer.
    anchors: BTreeMap<String, String>,
    /// By pointer: the digest of the definition's canonical JSON, and the pointers its `$ref`s
    /// designate. `None` for a pointer to nothing.
    read: HashMap<String, Option<Definition>>,
}

struct Definition {
    digest: String,
    refers_to: Vec<String>,
}

impl<'a> Definitions<'a> {
    fn new(document: &'a Value, anchors: BTreeMap<String, String>) -> Self {
        Self {
            document,
            anchors,
            read: HashMap::new(),
        }
    }

    /// How many definitions the document's `$ref`s designate.
    fn count(&self) -> usize {
        let mut references = Vec::new();
        collect_references(self.document, &mut references);
        references
            .iter()
            .filter_map(|reference| local_pointer(self.document, reference, &self.anchors))
            .filter(|pointer| self.document.pointer(pointer).is_some())
            .collect::<BTreeSet<_>>()
            .len()
    }

    fn definition(&mut self, pointer: &str) -> Option<&Definition> {
        if !self.read.contains_key(pointer) {
            let definition = self.document.pointer(pointer).map(|target| {
                let mut references = Vec::new();
                collect_references(target, &mut references);
                let refers_to: BTreeSet<String> = references
                    .iter()
                    .filter_map(|reference| local_pointer(self.document, reference, &self.anchors))
                    .collect();
                Definition {
                    digest: Digest::of(canonical_json(target).as_bytes()).to_hex(),
                    refers_to: refers_to.into_iter().collect(),
                }
            });
            self.read.insert(pointer.to_owned(), definition);
        }
        self.read.get(pointer).and_then(Option::as_ref)
    }

    /// What makes two definitions the same type: the definition's pointer, and the canonical
    /// JSON of every subschema it refers to, transitively, by pointer (each by its digest).
    fn identity(&mut self, pointer: &str) -> String {
        let mut closure: BTreeMap<String, String> = BTreeMap::new();
        let mut stack = vec![pointer.to_owned()];
        while let Some(next) = stack.pop() {
            if closure.contains_key(&next) {
                continue;
            }
            let Some(definition) = self.definition(&next) else {
                continue;
            };
            stack.extend(definition.refers_to.iter().cloned());
            closure.insert(next, definition.digest.clone());
        }
        canonical_json(&json!({ "at": pointer, "closure": closure }))
    }
}

/// Whether the value's JSON holds in `limit` bytes. Stops writing once it does not.
fn fits(value: &Value, limit: usize) -> bool {
    struct Counter {
        left: usize,
    }
    impl io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.left = self
                .left
                .checked_sub(bytes.len())
                .ok_or_else(|| io::Error::other("too large"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(&mut Counter { left: limit }, value).is_ok()
}

fn collect_references(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if matches!(key.as_str(), "$ref" | "$dynamicRef" | "$recursiveRef")
                    && let Value::String(reference) = value
                {
                    out.push(reference.clone());
                } else {
                    collect_references(value, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_references(item, out)),
        _ => {}
    }
}

/// `$anchor` names (and the old `"$id": "#name"`) → JSON pointers.
fn anchors(document: &Value) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    collect_anchors(document, String::new(), &mut found);
    found
}

fn collect_anchors(value: &Value, pointer: String, found: &mut BTreeMap<String, String>) {
    match value {
        Value::Object(object) => {
            if let Some(Value::String(anchor)) = object.get("$anchor") {
                found
                    .entry(anchor.clone())
                    .or_insert_with(|| pointer.clone());
            }
            if let Some(Value::String(id)) = object.get("$id")
                && let Some(anchor) = id.strip_prefix('#')
                && !anchor.is_empty()
            {
                found
                    .entry(anchor.to_owned())
                    .or_insert_with(|| pointer.clone());
            }
            for (key, value) in sorted(object) {
                collect_anchors(value, format!("{pointer}/{}", escape(key)), found);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_anchors(item, format!("{pointer}/{index}"), found);
            }
        }
        _ => {}
    }
}

/// The JSDoc of a schema: its description (or title), and the constraints a model should know.
pub(crate) fn doc_of(schema: &Value) -> Doc {
    let Value::Object(schema) = schema else {
        return Doc::default();
    };
    let text = |key: &str| {
        schema
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    // Titles are left out: Pydantic gives every property its own name as a title.
    let description = text("description");
    let mut tags = Vec::new();
    if let Some(format) = text("format") {
        tags.push(format!("@format {format}"));
    }
    for key in [
        "minimum",
        "exclusiveMinimum",
        "maximum",
        "exclusiveMaximum",
        "multipleOf",
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
    ] {
        if let Some(value @ Value::Number(_)) = schema.get(key) {
            tags.push(format!("@{key} {value}"));
        }
    }
    if let Some(pattern) = text("pattern") {
        tags.push(format!("@pattern {pattern}"));
    }
    if let Some(default) = schema.get("default") {
        tags.push(format!("@default {}", canonical_json(default)));
    }
    if schema.get("deprecated") == Some(&Value::Bool(true)) {
        tags.push("@deprecated".to_owned());
    }
    Doc { description, tags }
}

fn sorted(object: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut entries: Vec<(&String, &Value)> = object.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
}

/// Keywords that hold subschemas: they do not name what they hold.
fn is_keyword(segment: &str) -> bool {
    matches!(
        segment,
        "$defs"
            | "definitions"
            | "properties"
            | "patternProperties"
            | "additionalProperties"
            | "items"
            | "prefixItems"
            | "additionalItems"
            | "contains"
            | "anyOf"
            | "oneOf"
            | "allOf"
            | "not"
            | "if"
            | "then"
            | "else"
            | "dependentSchemas"
            | "propertyNames"
            | "unevaluatedItems"
            | "unevaluatedProperties"
    )
}

/// `issue-comment`, `issue_comment`, `issueComment` → `IssueComment`. Only ASCII letters and
/// digits are kept; a name that would start with a digit gets a `T` in front.
pub(crate) fn pascal_case(text: &str) -> String {
    let mut out = String::new();
    for part in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.extend(chars);
        }
    }
    if out.is_empty() {
        return "Type".to_owned();
    }
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, 'T');
    }
    out
}

/// A JSON pointer segment, unescaped (RFC 6901).
fn unescape(segment: &str) -> String {
    segment.replace("~1", "/").replace("~0", "~")
}

fn escape(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

/// A URI fragment, percent-decoded. Invalid escapes are kept as they are.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(hex) = text.get(index + 1..index + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ty::render;

    fn args(schema: Value) -> (String, Vec<(String, String)>) {
        let mut scope = Scope::default();
        let ty = convert(&mut scope, &schema, "list_issues", Role::Args);
        let declarations = scope
            .declarations
            .iter()
            .map(|declaration| (declaration.name.clone(), render(&declaration.ty, None, 0)))
            .collect();
        (render(&ty, None, 0), declarations)
    }

    fn ty(schema: Value) -> String {
        args(schema).0
    }

    #[test]
    fn scalars_enums_and_constants() {
        assert_eq!(ty(json!({ "type": "integer" })), "number");
        assert_eq!(ty(json!({ "type": ["string", "null"] })), "string | null");
        assert_eq!(
            ty(json!({ "type": "string", "enum": ["open", "closed"] })),
            r#""open" | "closed""#
        );
        assert_eq!(ty(json!({ "const": 3 })), "3");
        assert_eq!(
            ty(json!({ "type": "string", "nullable": true })),
            "string | null"
        );
        assert_eq!(ty(json!(true)), "unknown");
        assert_eq!(ty(json!(false)), "never");
        assert_eq!(ty(json!({})), "unknown");
        assert_eq!(ty(json!({ "type": "file" })), "unknown");
    }

    #[test]
    fn objects_list_their_properties_and_are_closed_unless_told_otherwise() {
        assert_eq!(
            ty(json!({
                "type": "object",
                "properties": { "repo": { "type": "string" }, "page": { "type": "integer" } },
                "required": ["repo", "token"]
            })),
            "{ repo: string; token: unknown; page?: number }"
        );
        assert_eq!(
            ty(json!({ "type": "object" })),
            "{ [key: string]: unknown }"
        );
        assert_eq!(
            ty(json!({ "type": "object", "properties": {} })),
            "{ [key: string]: never }"
        );
        assert_eq!(
            ty(json!({
                "type": "object",
                "properties": { "a": { "type": "string" } },
                "additionalProperties": { "type": "number" }
            })),
            "{ a?: string; [key: string]: number | string | undefined }"
        );
        assert_eq!(
            ty(json!({ "patternProperties": { "^x-": { "type": "string" } } })),
            "{ [key: string]: string }"
        );
    }

    #[test]
    fn arrays_and_tuples() {
        assert_eq!(
            ty(json!({ "type": "array", "items": { "type": ["string", "number"] } })),
            "(string | number)[]"
        );
        assert_eq!(ty(json!({ "type": "array" })), "unknown[]");
        assert_eq!(
            ty(json!({
                "type": "array",
                "prefixItems": [{ "type": "string" }, { "type": "number" }],
                "minItems": 1,
                "items": { "type": "boolean" }
            })),
            "[string, number?, ...boolean[]]"
        );
        assert_eq!(
            ty(json!({ "items": [{ "type": "string" }], "minItems": 1 })),
            "[string]"
        );
    }

    #[test]
    fn combinations() {
        assert_eq!(
            ty(json!({ "anyOf": [{ "type": "string" }, { "type": "null" }], "default": null })),
            "string | null"
        );
        assert_eq!(
            ty(json!({
                "type": "object",
                "properties": { "a": { "type": "string" }, "b": { "type": "string" } },
                "oneOf": [{ "required": ["a"] }, { "required": ["b"] }]
            })),
            "{ a?: string; b?: string } & ({ a: unknown } | { b: unknown })"
        );
        assert_eq!(
            ty(json!({ "allOf": [
                { "type": "object", "properties": { "a": { "type": "string" } }, "required": ["a"] },
                { "type": "object", "properties": { "b": { "type": "number" } } }
            ] })),
            "{ a: string } & { b?: number }"
        );
    }

    #[test]
    fn references_become_named_types() {
        let (top, declarations) = args(json!({
            "type": "object",
            "properties": {
                "issue": { "$ref": "#/$defs/Issue" },
                "comment": { "$ref": "#/definitions/issue-comment" }
            },
            "required": ["issue"],
            "$defs": {
                "Issue": {
                    "type": "object",
                    "description": "An issue.",
                    "properties": {
                        "number": { "type": "integer" },
                        "parent": { "$ref": "#/$defs/Issue" }
                    },
                    "required": ["number"]
                }
            },
            "definitions": {
                "issue-comment": { "type": "string" }
            }
        }));
        assert_eq!(top, "{ issue: Issue; comment?: IssueComment }");
        assert_eq!(
            declarations,
            [
                ("IssueComment".to_owned(), "string".to_owned()),
                (
                    "Issue".to_owned(),
                    "{ number: number; parent?: Issue }".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn root_references_anchors_and_unresolvable_references() {
        let (top, declarations) = args(json!({
            "type": "object",
            "properties": {
                "children": { "type": "array", "items": { "$ref": "#" } },
                "tag": { "$ref": "#tag" },
                "remote": { "$ref": "https://example.com/schema.json" },
                "missing": { "$ref": "#/$defs/Missing" }
            },
            "$defs": { "Tag": { "$anchor": "tag", "type": "string" } }
        }));
        assert_eq!(top, "ListIssuesArgs");
        assert_eq!(
            declarations[0],
            (
                "ListIssuesArgs".to_owned(),
                "{\n  children?: ListIssuesArgs[];\n  missing?: unknown;\n  remote?: unknown;\n  tag?: Tag;\n}"
                    .to_owned()
            )
        );
        assert_eq!(declarations[1], ("Tag".to_owned(), "string".to_owned()));
    }

    #[test]
    fn same_definitions_share_a_name_and_different_ones_do_not() {
        let issue = |number: &str| {
            json!({
                "type": "object",
                "properties": { "issue": { "$ref": "#/$defs/Issue" } },
                "$defs": { "Issue": { "type": "object", "properties": { "number": { "type": number } } } }
            })
        };
        let mut scope = Scope::default();
        let a = convert(&mut scope, &issue("integer"), "get_issue", Role::Result);
        let b = convert(&mut scope, &issue("integer"), "list_issues", Role::Result);
        let c = convert(&mut scope, &issue("string"), "list_issues", Role::Args);
        assert_eq!(render(&a, None, 0), "{ issue?: Issue }");
        assert_eq!(render(&b, None, 0), "{ issue?: Issue }");
        assert_eq!(render(&c, None, 0), "{ issue?: ListIssuesIssue }");
        let d = convert(&mut scope, &issue("boolean"), "list_issues", Role::Args);
        assert_eq!(render(&d, None, 0), "{ issue?: ListIssuesIssue2 }");
    }

    #[test]
    fn documentation_comes_from_descriptions_and_constraints() {
        let doc = doc_of(&json!({
            "title": "Page",
            "description": "The page.",
            "minimum": 1,
            "maximum": 100,
            "default": 1,
            "format": "int32",
            "deprecated": true
        }));
        assert_eq!(doc.description.as_deref(), Some("The page."));
        assert_eq!(doc_of(&json!({ "title": "Page" })).description, None);
        assert_eq!(
            doc.tags,
            [
                "@format int32",
                "@minimum 1",
                "@maximum 100",
                "@default 1",
                "@deprecated"
            ]
        );
    }

    #[test]
    fn names_and_pointers() {
        assert_eq!(pascal_case("issue-comment"), "IssueComment");
        assert_eq!(pascal_case("listIssues"), "ListIssues");
        assert_eq!(pascal_case("2fa"), "T2fa");
        assert_eq!(pascal_case("é"), "Type");
        assert_eq!(percent_decode("/%24defs/a%20b"), "/$defs/a b");
        assert_eq!(unescape("a~1b~0c"), "a/b~c");
    }
}
