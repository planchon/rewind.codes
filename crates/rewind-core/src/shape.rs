//! Observed result types (`archi/04-tools.md`, "Observed result types").
//!
//! Most MCP servers declare no `outputSchema`, but every result is in a journal. A projection
//! folds the results of each tool into a [`Shape`]: keys and types, never values. Codegen uses
//! it, marked `@observed`, when the server declares nothing. Shapes serve type-checking and the
//! prompt only: results are never validated against them, and they are not part of the action set
//! digest.
//!
//! [`Shape::merge`] is commutative, associative and idempotent, so a projection may fold
//! results in any order, and fold one twice, and still reach the same shape.

use std::collections::BTreeMap;

use serde_json::Value;

/// The shape of JSON values: their types and keys.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Shape {
    /// Nothing observed: the elements of arrays that were always empty.
    Unknown,
    Null,
    Boolean,
    Number,
    String,
    Array(Box<Shape>),
    Object(BTreeMap<String, Field>),
    /// Values of several types: at most one shape per type, in the order of the variants above,
    /// with arrays and objects merged.
    Union(Vec<Shape>),
}

/// A key of an object shape.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Field {
    pub shape: Shape,
    /// Missing from some of the observed objects.
    pub optional: bool,
}

impl Shape {
    /// The shape of one value.
    pub fn of(value: &Value) -> Shape {
        match value {
            Value::Null => Shape::Null,
            Value::Bool(_) => Shape::Boolean,
            Value::Number(_) => Shape::Number,
            Value::String(_) => Shape::String,
            Value::Array(items) => Shape::Array(Box::new(
                items
                    .iter()
                    .map(Shape::of)
                    .fold(Shape::Unknown, Shape::merge),
            )),
            Value::Object(fields) => Shape::Object(
                fields
                    .iter()
                    .map(|(key, value)| {
                        let field = Field {
                            shape: Shape::of(value),
                            optional: false,
                        };
                        (key.clone(), field)
                    })
                    .collect(),
            ),
        }
    }

    /// The shape of the values of both shapes. A key present on one side only is optional.
    pub fn merge(self, other: Shape) -> Shape {
        let mut slots: [Option<Shape>; 6] = Default::default();
        for shape in self.into_members().into_iter().chain(other.into_members()) {
            let slot = &mut slots[shape.rank()];
            *slot = Some(match slot.take() {
                None => shape,
                Some(existing) => existing.merge_same_type(shape),
            });
        }
        let mut members: Vec<Shape> = slots.into_iter().flatten().collect();
        match members.len() {
            0 => Shape::Unknown,
            1 => members.remove(0),
            _ => Shape::Union(members),
        }
    }

    /// The members of a union, or the shape itself; nothing for `Unknown`.
    fn into_members(self) -> Vec<Shape> {
        match self {
            Shape::Unknown => Vec::new(),
            Shape::Union(members) => members,
            shape => vec![shape],
        }
    }

    /// The position of a type in a union.
    fn rank(&self) -> usize {
        match self {
            Shape::Null => 0,
            Shape::Boolean => 1,
            Shape::Number => 2,
            Shape::String => 3,
            Shape::Array(_) => 4,
            Shape::Object(_) | Shape::Unknown | Shape::Union(_) => 5,
        }
    }

    /// Merges two shapes of the same type (same rank).
    fn merge_same_type(self, other: Shape) -> Shape {
        match (self, other) {
            (Shape::Array(a), Shape::Array(b)) => Shape::Array(Box::new(a.merge(*b))),
            (Shape::Object(mut a), Shape::Object(mut b)) => {
                let keys: Vec<String> = a.keys().chain(b.keys()).cloned().collect();
                let mut merged = BTreeMap::new();
                for key in keys {
                    if merged.contains_key(&key) {
                        continue;
                    }
                    let field = match (a.remove(&key), b.remove(&key)) {
                        (Some(x), Some(y)) => Field {
                            shape: x.shape.merge(y.shape),
                            optional: x.optional || y.optional,
                        },
                        (Some(only), None) | (None, Some(only)) => Field {
                            shape: only.shape,
                            optional: true,
                        },
                        (None, None) => continue,
                    };
                    merged.insert(key, field);
                }
                Shape::Object(merged)
            }
            (same, _) => same,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn object(fields: &[(&str, Shape, bool)]) -> Shape {
        Shape::Object(
            fields
                .iter()
                .map(|(key, shape, optional)| {
                    let field = Field {
                        shape: shape.clone(),
                        optional: *optional,
                    };
                    ((*key).to_owned(), field)
                })
                .collect(),
        )
    }

    #[test]
    fn keys_missing_from_some_results_are_optional() {
        let a = Shape::of(&json!({ "id": 1, "title": "x", "labels": [] }));
        let b = Shape::of(&json!({ "id": 2, "closed_at": null, "labels": ["bug"] }));
        assert_eq!(
            a.merge(b),
            object(&[
                ("closed_at", Shape::Null, true),
                ("id", Shape::Number, false),
                ("labels", Shape::Array(Box::new(Shape::String)), false),
                ("title", Shape::String, true),
            ])
        );
    }

    #[test]
    fn different_types_make_a_union_in_a_fixed_order() {
        let shape = Shape::of(&json!(["a", 1, null, { "x": 1 }, { "y": true }]));
        assert_eq!(
            shape,
            Shape::Array(Box::new(Shape::Union(vec![
                Shape::Null,
                Shape::Number,
                Shape::String,
                object(&[("x", Shape::Number, true), ("y", Shape::Boolean, true)]),
            ])))
        );
        assert_eq!(
            Shape::of(&json!([])),
            Shape::Array(Box::new(Shape::Unknown))
        );
    }

    #[test]
    fn merging_is_commutative_associative_and_idempotent() {
        let values = [
            json!({ "a": 1, "b": [1, "x"] }),
            json!({ "a": "s", "c": { "d": null } }),
            json!([{ "e": 1 }]),
            json!(null),
            json!({ "b": [], "c": { "d": 4, "f": true } }),
        ];
        let shapes: Vec<Shape> = values.iter().map(Shape::of).collect();
        let forward = shapes.iter().cloned().fold(Shape::Unknown, Shape::merge);
        let backward = shapes
            .iter()
            .rev()
            .cloned()
            .fold(Shape::Unknown, Shape::merge);
        assert_eq!(forward, backward);
        assert_eq!(forward.clone().merge(forward.clone()), forward);
        let twice = shapes
            .iter()
            .chain(shapes.iter())
            .cloned()
            .fold(Shape::Unknown, Shape::merge);
        assert_eq!(twice, forward);
        let grouped = shapes[0]
            .clone()
            .merge(shapes[1].clone().merge(shapes[2].clone()))
            .merge(shapes[3].clone().merge(shapes[4].clone()));
        assert_eq!(grouped, forward);
    }
}
