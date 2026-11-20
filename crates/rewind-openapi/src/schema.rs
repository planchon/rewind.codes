//! `$ref` within the document, and the schemas of an OpenAPI document as JSON Schema
//! (`archi/04-tools.md`, "API tools").
//!
//! - A `$ref` is resolved within the document (`#/…`); one to another document refuses the
//!   registration.
//! - A schema's references to `#/components/schemas/<name>` become `#/$defs/<name>`, and the
//!   component joins the `$defs` of the tool's schema, converted in turn: recursive schemas
//!   stay recursive, and each component is written once. Any other reference (a parameter's
//!   schema, a path) is inlined, a cycle through one refused.
//! - OpenAPI 3.0 schemas are a dialect of JSON Schema draft 4: `nullable: true` becomes `"null"`
//!   among the types (and the values of `enum`), and the boolean `exclusiveMinimum` and
//!   `exclusiveMaximum` become the numbers of JSON Schema 2020-12. OpenAPI 3.1 schemas are JSON
//!   Schema 2020-12 already. `$id` and `$schema` are dropped: the references are rewritten
//!   relative to the tool's schema.
//!
//! The values of `example`, `examples`, `default`, `enum`, `const` and of the extensions
//! (`x-…`) are data, not schemas: they are copied as they are.

use std::cell::Cell;
use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// How deep inlined references may go before they are taken for a cycle.
const MAX_INLINE_DEPTH: usize = 64;

/// How many values one reading's schemas may expand to, references inlined. A reference used
/// twice is inlined twice: a document of a few KB whose references fan out level after level,
/// or a large subschema referred to many times, would otherwise expand into gigabytes on the
/// API's process.
pub(crate) const MAX_SCHEMA_VALUES: usize = 1_000_000;

const COMPONENT_SCHEMAS: &str = "#/components/schemas/";

/// An OpenAPI document being read.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Document<'a> {
    pub(crate) root: &'a Value,
    /// OpenAPI 3.0: its schemas are converted.
    pub(crate) v30: bool,
    /// The values this reading's schemas expanded to so far, at most [`MAX_SCHEMA_VALUES`].
    pub(crate) values: &'a Cell<usize>,
}

/// The component schemas one tool's schema designates, converted, by name.
#[derive(Debug, Default)]
pub(crate) struct Defs(BTreeMap<String, Value>);

impl Defs {
    /// `schema` with these definitions in its `$defs`, if it designates any.
    pub(crate) fn attach(self, schema: Value) -> Value {
        if self.0.is_empty() {
            return schema;
        }
        let defs: Map<String, Value> = self.0.into_iter().collect();
        match schema {
            Value::Object(mut object) => {
                object.insert("$defs".into(), Value::Object(defs));
                Value::Object(object)
            }
            // `true` or `false`: as an object, with the definitions.
            Value::Bool(true) => Value::Object(Map::from_iter([("$defs".into(), defs.into())])),
            other => other,
        }
    }
}

impl<'a> Document<'a> {
    /// The value a JSON pointer `#/…` designates.
    pub(crate) fn pointer(&self, reference: &str) -> Result<&'a Value, String> {
        let Some(pointer) = reference.strip_prefix('#') else {
            return Err(format!(
                "the $ref {reference:?} is to another document, which Rewind does not read"
            ));
        };
        let pointer = percent_decode(pointer);
        self.root
            .pointer(&pointer)
            .ok_or_else(|| format!("the $ref {reference:?} designates nothing in the document"))
    }

    /// `value`, or what its `$ref` designates, followed until a value that is no reference: for
    /// the objects that are not schemas (parameters, request bodies, responses, path items,
    /// security schemes).
    pub(crate) fn deref(&self, value: &'a Value) -> Result<&'a Value, String> {
        let mut current = value;
        for _ in 0..MAX_INLINE_DEPTH {
            match current.get("$ref").and_then(Value::as_str) {
                Some(reference) => current = self.pointer(reference)?,
                None => return Ok(current),
            }
        }
        Err("its $ref chain does not end".into())
    }

    /// `schema` as a tool pins it, its component references gathered in `defs`.
    pub(crate) fn schema(&self, schema: &Value, defs: &mut Defs) -> Result<Value, String> {
        self.convert(schema, defs, 0)
    }

    fn convert(&self, value: &Value, defs: &mut Defs, depth: usize) -> Result<Value, String> {
        let values = self.values.get() + 1;
        if values > MAX_SCHEMA_VALUES {
            return Err(format!(
                "its schemas expand to more than {MAX_SCHEMA_VALUES} values once their $ref are \
                 inlined: name the schemas used several times in components.schemas, or keep \
                 fewer operations (`operations`)"
            ));
        }
        self.values.set(values);
        let Value::Object(object) = value else {
            return Ok(match value {
                Value::Array(items) => Value::Array(
                    items
                        .iter()
                        .map(|item| self.convert(item, defs, depth))
                        .collect::<Result<_, _>>()?,
                ),
                other => other.clone(),
            });
        };
        let mut converted = Map::new();
        if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
            if let Some(rest) = reference.strip_prefix(COMPONENT_SCHEMAS) {
                let name = percent_decode(rest.split('/').next().unwrap_or_default());
                let name = name.replace("~1", "/").replace("~0", "~");
                self.define(&name, defs)?;
                converted.insert("$ref".into(), Value::String(format!("#/$defs/{rest}")));
            } else {
                if depth >= MAX_INLINE_DEPTH {
                    return Err(format!("the $ref {reference:?} is part of a cycle"));
                }
                let target = self.pointer(reference)?;
                let inlined = self.convert(target, defs, depth + 1)?;
                // The reference's own keywords, beside it, join what it designates.
                let Value::Object(mut inlined) = inlined else {
                    return Ok(inlined);
                };
                for (key, value) in object {
                    if key != "$ref" {
                        inlined.insert(key.clone(), self.keyword(key, value, defs, depth)?);
                    }
                }
                return Ok(Value::Object(self.dialect(inlined)));
            }
        }
        for (key, value) in object {
            if matches!(key.as_str(), "$ref" | "$id" | "$schema") {
                continue;
            }
            converted.insert(key.clone(), self.keyword(key, value, defs, depth)?);
        }
        Ok(Value::Object(self.dialect(converted)))
    }

    /// A keyword's value: data copied as it is, subschemas converted.
    fn keyword(
        &self,
        key: &str,
        value: &Value,
        defs: &mut Defs,
        depth: usize,
    ) -> Result<Value, String> {
        if matches!(
            key,
            "example" | "examples" | "default" | "enum" | "const" | "required"
        ) || key.starts_with("x-")
        {
            return Ok(value.clone());
        }
        if matches!(
            key,
            "properties" | "patternProperties" | "$defs" | "definitions"
        ) && let Value::Object(map) = value
        {
            // A map of names to schemas: a property may be named as a keyword.
            let mut converted = Map::new();
            for (name, schema) in map {
                converted.insert(name.clone(), self.convert(schema, defs, depth)?);
            }
            return Ok(Value::Object(converted));
        }
        self.convert(value, defs, depth)
    }

    /// Converts the component schema `name` into `defs`, once.
    fn define(&self, name: &str, defs: &mut Defs) -> Result<(), String> {
        if defs.0.contains_key(name) {
            return Ok(());
        }
        let schema = self
            .root
            .pointer("/components/schemas")
            .and_then(|schemas| schemas.get(name))
            .ok_or_else(|| {
                format!("the $ref to the schema {name:?} designates nothing in components.schemas")
            })?;
        // Reserved first: a schema that refers to itself finds itself defined.
        defs.0.insert(name.to_owned(), Value::Bool(true));
        let converted = self.convert(schema, defs, 0)?;
        defs.0.insert(name.to_owned(), converted);
        Ok(())
    }

    /// OpenAPI 3.0's keywords as JSON Schema 2020-12 writes them.
    fn dialect(&self, mut object: Map<String, Value>) -> Map<String, Value> {
        if !self.v30 {
            return object;
        }
        if object.remove("nullable") == Some(Value::Bool(true)) {
            match object.get_mut("type") {
                Some(Value::String(single)) => {
                    let single = std::mem::take(single);
                    object.insert(
                        "type".into(),
                        Value::Array(vec![Value::String(single), "null".into()]),
                    );
                }
                Some(Value::Array(types)) if !types.contains(&Value::from("null")) => {
                    types.push("null".into());
                }
                _ => {}
            }
            if let Some(Value::Array(values)) = object.get_mut("enum")
                && !values.contains(&Value::Null)
            {
                values.push(Value::Null);
            }
        }
        for (exclusive, bound) in [
            ("exclusiveMinimum", "minimum"),
            ("exclusiveMaximum", "maximum"),
        ] {
            match object.get(exclusive) {
                Some(Value::Bool(true)) => {
                    match object.remove(bound) {
                        Some(limit) => object.insert(exclusive.into(), limit),
                        None => object.remove(exclusive),
                    };
                }
                Some(Value::Bool(false)) => {
                    object.remove(exclusive);
                }
                _ => {}
            }
        }
        object
    }
}

/// A JSON pointer as a URI fragment writes it, its `%XX` decoded.
fn percent_decode(pointer: &str) -> String {
    percent_encoding::percent_decode_str(pointer)
        .decode_utf8_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn components_go_to_defs_and_other_references_are_inlined() {
        let root = json!({
            "components": {
                "schemas": {
                    "Measure": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "parent": { "$ref": "#/components/schemas/Measure" },
                            "unit": { "$ref": "#/components/schemas/Unit" }
                        }
                    },
                    "Unit": { "type": "string", "enum": ["kg", "m"] }
                },
                "parameters": { "Id": { "name": "id", "in": "path", "schema": { "type": "integer" } } }
            }
        });
        let values = Cell::new(0);
        let document = Document {
            root: &root,
            v30: false,
            values: &values,
        };
        let mut defs = Defs::default();
        let schema = document
            .schema(
                &json!({ "type": "array", "items": { "$ref": "#/components/schemas/Measure" } }),
                &mut defs,
            )
            .unwrap();
        let schema = defs.attach(schema);
        assert_eq!(schema["items"], json!({ "$ref": "#/$defs/Measure" }));
        assert_eq!(
            schema["$defs"]["Measure"]["properties"]["parent"],
            json!({ "$ref": "#/$defs/Measure" })
        );
        assert_eq!(schema["$defs"]["Unit"]["enum"], json!(["kg", "m"]));
        let mut defs = Defs::default();
        assert_eq!(
            document
                .schema(
                    &json!({ "$ref": "#/components/parameters/Id/schema" }),
                    &mut defs
                )
                .unwrap(),
            json!({ "type": "integer" })
        );
        let elsewhere = document
            .schema(&json!({ "$ref": "other.json#/Measure" }), &mut defs)
            .unwrap_err();
        assert!(elsewhere.contains("another document"), "{elsewhere}");
        let nowhere = document
            .schema(&json!({ "$ref": "#/components/schemas/Gone" }), &mut defs)
            .unwrap_err();
        assert!(nowhere.contains("designates nothing"), "{nowhere}");
    }

    #[test]
    fn openapi_3_0_keywords_become_json_schema() {
        let root = json!({});
        let values = Cell::new(0);
        let document = Document {
            root: &root,
            v30: true,
            values: &values,
        };
        let mut defs = Defs::default();
        let schema = document
            .schema(
                &json!({
                    "type": "object",
                    "properties": {
                        "note": { "type": "string", "nullable": true },
                        "level": { "type": "string", "enum": ["low", "high"], "nullable": true },
                        "count": { "type": "integer", "minimum": 0, "exclusiveMinimum": true },
                        "ratio": { "type": "number", "maximum": 1, "exclusiveMaximum": false },
                        "nullable": { "type": "boolean" }
                    },
                    "example": { "nullable": true, "$ref": "kept" }
                }),
                &mut defs,
            )
            .unwrap();
        assert_eq!(
            schema["properties"]["note"],
            json!({ "type": ["string", "null"] })
        );
        assert_eq!(
            schema["properties"]["level"]["enum"],
            json!(["low", "high", null])
        );
        assert_eq!(
            schema["properties"]["count"],
            json!({ "type": "integer", "exclusiveMinimum": 0 })
        );
        assert_eq!(
            schema["properties"]["ratio"],
            json!({ "type": "number", "maximum": 1 })
        );
        assert_eq!(
            schema["properties"]["nullable"],
            json!({ "type": "boolean" })
        );
        assert_eq!(
            schema["example"],
            json!({ "nullable": true, "$ref": "kept" })
        );
    }
}
