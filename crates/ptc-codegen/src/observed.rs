//! Observed result types (`archi/04-tools.md`, "Observed result types"): the shape a projection
//! folded from the results recorded in journals, as a TypeScript type.

use ptc_core::Shape;

use crate::ty::{Doc, Object, Property, Ty};

/// The type of the values of a shape. Only the keys observed are listed, optional when some
/// results lacked them; nothing else is known, so the type may be incomplete.
pub(crate) fn type_of(shape: &Shape) -> Ty {
    match shape {
        Shape::Unknown => Ty::Unknown,
        Shape::Null => Ty::Null,
        Shape::Boolean => Ty::Boolean,
        Shape::Number => Ty::Number,
        Shape::String => Ty::String,
        Shape::Array(item) => Ty::Array(Box::new(type_of(item))),
        Shape::Object(fields) => {
            // Required properties first, each group by name, as for schemas.
            let (mut properties, optional): (Vec<Property>, Vec<Property>) = fields
                .iter()
                .map(|(name, field)| Property {
                    name: name.clone(),
                    ty: type_of(&field.shape),
                    optional: field.optional,
                    doc: Doc::default(),
                })
                .partition(|property| !property.optional);
            properties.extend(optional);
            Ty::Object(Object {
                properties,
                index: None,
            })
        }
        Shape::Union(members) => Ty::union(members.iter().map(type_of)),
    }
}

/// What an MCP tool without an output schema returned in `structuredContent`, when its
/// recorded results had some: the result itself is the content, and `json()` returns this.
pub(crate) fn structured_content(shape: &Shape) -> Option<&Shape> {
    let object = match shape {
        Shape::Object(fields) => fields,
        Shape::Union(members) => members.iter().find_map(|member| match member {
            Shape::Object(fields) => Some(fields),
            _ => None,
        })?,
        _ => return None,
    };
    object
        .get("structuredContent")
        .map(|field| &field.shape)
        .filter(|shape| **shape != Shape::Unknown)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ty::render;

    #[test]
    fn shapes_become_types() {
        let shape = Shape::of(&json!({ "id": 1, "tags": ["a"], "closed_at": null }))
            .merge(Shape::of(&json!({ "id": 2, "tags": [], "title": "x" })));
        assert_eq!(
            render(&type_of(&shape), None, 0),
            "{ id: number; tags: string[]; closed_at?: null; title?: string }"
        );
        assert_eq!(
            render(&type_of(&Shape::of(&json!([1, "a", null]))), None, 0),
            "(null | number | string)[]"
        );
    }

    #[test]
    fn structured_content_is_read_from_mcp_results() {
        let with = Shape::of(&json!({ "content": [], "structuredContent": { "id": 1 } }));
        assert_eq!(
            structured_content(&with),
            Some(&Shape::of(&json!({ "id": 1 })))
        );
        let without = Shape::of(&json!({ "content": [{ "type": "text", "text": "x" }] }));
        assert_eq!(structured_content(&without), None);
    }
}
