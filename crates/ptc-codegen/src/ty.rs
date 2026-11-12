//! The TypeScript types codegen produces, and how they are written: in full for
//! `ptc-tools.d.ts`, on one line for the prompt.

use serde_json::Value;

/// A TypeScript type.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Ty {
    Unknown,
    Never,
    Null,
    /// Only in index signatures, for the optional properties they must cover.
    Undefined,
    Boolean,
    Number,
    String,
    /// A JSON value as a literal type: `"open"`, `42`, `true`, `null`, `{ a: 1 }`.
    Literal(Value),
    Array(Box<Ty>),
    Tuple(Tuple),
    Object(Object),
    Union(Vec<Ty>),
    Intersection(Vec<Ty>),
    /// A named type of the tool's namespace.
    Named(String),
    /// `ToolContent<T>`: what an MCP tool without an output schema returns.
    Content(Box<Ty>),
}

/// `[A, B?, ...C[]]`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Tuple {
    pub(crate) items: Vec<Ty>,
    /// The items before this index are required; the others are optional.
    pub(crate) required: usize,
    pub(crate) rest: Option<Box<Ty>>,
}

/// An object type: its properties, sorted by name, and its index signature.
#[derive(Clone, Debug, PartialEq, Default)]
pub(crate) struct Object {
    pub(crate) properties: Vec<Property>,
    /// `[key: string]: T`, for the keys not listed.
    pub(crate) index: Option<Box<Ty>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Property {
    pub(crate) name: String,
    pub(crate) ty: Ty,
    pub(crate) optional: bool,
    pub(crate) doc: Doc,
}

/// A JSDoc comment: a description, then tags such as `@default "open"`.
#[derive(Clone, Debug, PartialEq, Default)]
pub(crate) struct Doc {
    pub(crate) description: Option<String>,
    pub(crate) tags: Vec<String>,
}

impl Doc {
    pub(crate) fn is_empty(&self) -> bool {
        self.description.is_none() && self.tags.is_empty()
    }
}

impl Ty {
    /// `A | B`, flattened and without duplicates. `unknown` absorbs the other members, and
    /// `never` disappears.
    pub(crate) fn union(members: impl IntoIterator<Item = Ty>) -> Ty {
        let mut flat: Vec<Ty> = Vec::new();
        for member in members {
            match member {
                Ty::Union(inner) => inner.into_iter().for_each(|ty| push_unique(&mut flat, ty)),
                Ty::Never => {}
                ty => push_unique(&mut flat, ty),
            }
        }
        if flat.contains(&Ty::Unknown) {
            return Ty::Unknown;
        }
        match flat.len() {
            0 => Ty::Never,
            1 => flat.remove(0),
            _ => Ty::Union(flat),
        }
    }

    /// `A & B`, flattened and without duplicates. `unknown` disappears, and `never` absorbs
    /// the other members.
    pub(crate) fn intersection(members: impl IntoIterator<Item = Ty>) -> Ty {
        let mut flat: Vec<Ty> = Vec::new();
        for member in members {
            match member {
                Ty::Intersection(inner) => {
                    inner.into_iter().for_each(|ty| push_unique(&mut flat, ty));
                }
                Ty::Unknown => {}
                ty => push_unique(&mut flat, ty),
            }
        }
        if flat.contains(&Ty::Never) {
            return Ty::Never;
        }
        match flat.len() {
            0 => Ty::Unknown,
            1 => flat.remove(0),
            _ => Ty::Intersection(flat),
        }
    }

    /// The names of the named types this type refers to, in order of appearance.
    pub(crate) fn named(&self, names: &mut Vec<String>) {
        match self {
            Ty::Named(name) => {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
            Ty::Array(item) | Ty::Content(item) => item.named(names),
            Ty::Tuple(tuple) => {
                tuple.items.iter().for_each(|item| item.named(names));
                if let Some(rest) = &tuple.rest {
                    rest.named(names);
                }
            }
            Ty::Object(object) => {
                object
                    .properties
                    .iter()
                    .for_each(|property| property.ty.named(names));
                if let Some(index) = &object.index {
                    index.named(names);
                }
            }
            Ty::Union(members) | Ty::Intersection(members) => {
                members.iter().for_each(|member| member.named(names));
            }
            Ty::Unknown
            | Ty::Never
            | Ty::Null
            | Ty::Undefined
            | Ty::Boolean
            | Ty::Number
            | Ty::String
            | Ty::Literal(_) => {}
        }
    }
}

fn push_unique(list: &mut Vec<Ty>, ty: Ty) {
    if !list.contains(&ty) {
        list.push(ty);
    }
}

// -------------------------------------------------------------------------------------------
// Written in full, for `ptc-tools.d.ts`

/// Objects shorter than this, without comments, are written on one line.
const INLINE_OBJECT: usize = 64;

/// The type in full. Named types get `qualifier.` in front of them: the namespace they live in,
/// when the type is written outside of it.
pub(crate) fn render(ty: &Ty, qualifier: Option<&str>, indent: usize) -> String {
    let mut out = String::new();
    write(&mut out, ty, qualifier, indent);
    out
}

fn write(out: &mut String, ty: &Ty, qualifier: Option<&str>, indent: usize) {
    match ty {
        Ty::Unknown => out.push_str("unknown"),
        Ty::Never => out.push_str("never"),
        Ty::Null => out.push_str("null"),
        Ty::Undefined => out.push_str("undefined"),
        Ty::Boolean => out.push_str("boolean"),
        Ty::Number => out.push_str("number"),
        Ty::String => out.push_str("string"),
        Ty::Literal(value) => out.push_str(&literal(value)),
        Ty::Array(item) => {
            if needs_parentheses_in_array(item) {
                out.push('(');
                write(out, item, qualifier, indent);
                out.push_str(")[]");
            } else {
                write(out, item, qualifier, indent);
                out.push_str("[]");
            }
        }
        Ty::Tuple(tuple) => {
            out.push('[');
            for (index, item) in tuple.items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                let optional = index >= tuple.required;
                if optional && needs_parentheses_in_array(item) {
                    out.push('(');
                    write(out, item, qualifier, indent);
                    out.push(')');
                } else {
                    write(out, item, qualifier, indent);
                }
                if optional {
                    out.push('?');
                }
            }
            if let Some(rest) = &tuple.rest {
                if !tuple.items.is_empty() {
                    out.push_str(", ");
                }
                out.push_str("...");
                write(out, &Ty::Array(rest.clone()), qualifier, indent);
            }
            out.push(']');
        }
        Ty::Object(object) => write_object(out, object, qualifier, indent),
        Ty::Union(members) => {
            for (index, member) in members.iter().enumerate() {
                if index > 0 {
                    out.push_str(" | ");
                }
                write(out, member, qualifier, indent);
            }
        }
        Ty::Intersection(members) => {
            for (index, member) in members.iter().enumerate() {
                if index > 0 {
                    out.push_str(" & ");
                }
                if matches!(member, Ty::Union(_)) {
                    out.push('(');
                    write(out, member, qualifier, indent);
                    out.push(')');
                } else {
                    write(out, member, qualifier, indent);
                }
            }
        }
        Ty::Named(name) => {
            if let Some(qualifier) = qualifier {
                out.push_str(qualifier);
                out.push('.');
            }
            out.push_str(name);
        }
        Ty::Content(structured) => {
            out.push_str("ToolContent");
            if **structured != Ty::Unknown {
                out.push('<');
                write(out, structured, qualifier, indent);
                out.push('>');
            }
        }
    }
}

fn needs_parentheses_in_array(ty: &Ty) -> bool {
    match ty {
        Ty::Union(_) | Ty::Intersection(_) => true,
        Ty::Literal(Value::Number(number)) => number.to_string().starts_with('-'),
        _ => false,
    }
}

fn write_object(out: &mut String, object: &Object, qualifier: Option<&str>, indent: usize) {
    if object.properties.is_empty() && object.index.is_none() {
        // `{}` would accept any value but `null` and `undefined`.
        out.push_str("{ [key: string]: never }");
        return;
    }
    if let Some(inline) = inline_object(object, qualifier)
        && inline.len() <= INLINE_OBJECT
    {
        out.push_str(&inline);
        return;
    }
    write_block(out, object, qualifier, indent);
}

/// The object on several lines, as the body of an interface.
pub(crate) fn block(object: &Object, qualifier: Option<&str>, indent: usize) -> String {
    let mut out = String::new();
    write_block(&mut out, object, qualifier, indent);
    out
}

fn write_block(out: &mut String, object: &Object, qualifier: Option<&str>, indent: usize) {
    out.push_str("{\n");
    for property in &object.properties {
        jsdoc(out, &property.doc, indent + 1);
        pad(out, indent + 1);
        out.push_str(&property_name(&property.name));
        if property.optional {
            out.push('?');
        }
        out.push_str(": ");
        write(out, &property.ty, qualifier, indent + 1);
        out.push_str(";\n");
    }
    if let Some(index) = &object.index {
        pad(out, indent + 1);
        out.push_str("[key: string]: ");
        write(out, index, qualifier, indent + 1);
        out.push_str(";\n");
    }
    pad(out, indent);
    out.push('}');
}

/// The object on one line, if none of its properties has a comment.
fn inline_object(object: &Object, qualifier: Option<&str>) -> Option<String> {
    if object
        .properties
        .iter()
        .any(|property| !property.doc.is_empty())
    {
        return None;
    }
    let mut parts = Vec::new();
    for property in &object.properties {
        let ty = render(&property.ty, qualifier, 0);
        if ty.contains('\n') {
            return None;
        }
        let optional = if property.optional { "?" } else { "" };
        parts.push(format!("{}{optional}: {ty}", property_name(&property.name)));
    }
    if let Some(index) = &object.index {
        let ty = render(index, qualifier, 0);
        if ty.contains('\n') {
            return None;
        }
        parts.push(format!("[key: string]: {ty}"));
    }
    Some(format!("{{ {} }}", parts.join("; ")))
}

/// A JSON value as a literal type.
pub(crate) fn literal(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => string_literal(text),
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(literal).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(fields) if fields.is_empty() => "{ [key: string]: never }".to_owned(),
        Value::Object(fields) => {
            let mut keys: Vec<&String> = fields.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|key| format!("{}: {}", property_name(key), literal(&fields[key])))
                .collect();
            format!("{{ {} }}", parts.join("; "))
        }
    }
}

/// A string literal, in double quotes. JSON's escapes are TypeScript's.
pub(crate) fn string_literal(text: &str) -> String {
    Value::String(text.to_owned()).to_string()
}

/// A property or method name: as is when it is an identifier, quoted otherwise.
pub(crate) fn property_name(name: &str) -> String {
    if is_identifier(name) {
        name.to_owned()
    } else {
        string_literal(name)
    }
}

/// An ECMAScript identifier in ASCII.
pub(crate) fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

pub(crate) fn pad(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

/// A JSDoc comment, on one line when it is short.
pub(crate) fn jsdoc(out: &mut String, doc: &Doc, indent: usize) {
    if doc.is_empty() {
        return;
    }
    let mut lines: Vec<String> = Vec::new();
    if let Some(description) = &doc.description {
        lines.extend(description.lines().map(|line| line.trim_end().to_owned()));
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        while lines.first().is_some_and(String::is_empty) {
            lines.remove(0);
        }
    }
    if !doc.tags.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.extend(doc.tags.iter().cloned());
    }
    let lines: Vec<String> = lines
        .into_iter()
        .map(|line| line.replace("*/", "*\\/"))
        .collect();
    if let [line] = lines.as_slice()
        && line.len() + indent * 2 <= 90
    {
        pad(out, indent);
        out.push_str("/** ");
        out.push_str(line);
        out.push_str(" */\n");
        return;
    }
    pad(out, indent);
    out.push_str("/**\n");
    for line in &lines {
        pad(out, indent);
        if line.is_empty() {
            out.push_str(" *\n");
        } else {
            out.push_str(" * ");
            out.push_str(line);
            out.push('\n');
        }
    }
    pad(out, indent);
    out.push_str(" */\n");
}

// -------------------------------------------------------------------------------------------
// On one line, for the prompt

/// Union members shown in the prompt before `…`.
const COMPACT_UNION: usize = 8;

/// Writes types on one line, compactly: `{ repo: string, state?: "open" | "closed" }`.
/// Named types are expanded where they are used, as the prompt has no other place for them,
/// unless they are recursive or the nesting is deeper than the budget.
pub(crate) struct Compact<'a> {
    pub(crate) named: &'a dyn Fn(&str) -> Option<&'a Ty>,
}

impl Compact<'_> {
    /// The type, with objects expanded down to `depth` levels; deeper objects are `{…}`.
    pub(crate) fn render(&self, ty: &Ty, depth: usize) -> String {
        let mut out = String::new();
        self.write(&mut out, ty, depth, &mut Vec::new());
        out
    }

    /// The properties of an object, as the arguments of a call: `{ repo: string, state? }`.
    /// With `types: false`, only their names.
    pub(crate) fn arguments(&self, object: &Object, depth: usize, types: bool) -> String {
        let mut parts = Vec::new();
        for property in &object.properties {
            let optional = if property.optional { "?" } else { "" };
            let name = property_name(&property.name);
            if types {
                let ty = self.render(&property.ty, depth);
                parts.push(format!("{name}{optional}: {ty}"));
            } else {
                parts.push(format!("{name}{optional}"));
            }
        }
        if let Some(index) = &object.index {
            if types {
                parts.push(format!("[key: string]: {}", self.render(index, depth)));
            } else {
                parts.push("…".to_owned());
            }
        }
        if parts.is_empty() {
            "{}".to_owned()
        } else {
            format!("{{ {} }}", parts.join(", "))
        }
    }

    fn write(&self, out: &mut String, ty: &Ty, depth: usize, stack: &mut Vec<String>) {
        match ty {
            Ty::Array(item) => {
                let parentheses = needs_parentheses_in_array(item)
                    || {
                        // An expanded named union needs them too.
                        matches!(&**item, Ty::Named(name)
                        if self.expands(name, depth, stack)
                            && matches!((self.named)(name), Some(Ty::Union(_) | Ty::Intersection(_))))
                    };
                if parentheses {
                    out.push('(');
                }
                self.write(out, item, depth, stack);
                if parentheses {
                    out.push(')');
                }
                out.push_str("[]");
            }
            Ty::Tuple(tuple) => {
                out.push('[');
                for (index, item) in tuple.items.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    self.write(out, item, depth, stack);
                    if index >= tuple.required {
                        out.push('?');
                    }
                }
                if let Some(rest) = &tuple.rest {
                    if !tuple.items.is_empty() {
                        out.push_str(", ");
                    }
                    out.push_str("...");
                    self.write(out, &Ty::Array(rest.clone()), depth, stack);
                }
                out.push(']');
            }
            Ty::Object(object) => {
                if object.properties.is_empty() && object.index.is_none() {
                    out.push_str("{}");
                } else if depth == 0 {
                    out.push_str("{…}");
                } else {
                    let mut parts = Vec::new();
                    for property in &object.properties {
                        let mut part = property_name(&property.name);
                        if property.optional {
                            part.push('?');
                        }
                        part.push_str(": ");
                        self.write(&mut part, &property.ty, depth - 1, stack);
                        parts.push(part);
                    }
                    if let Some(index) = &object.index {
                        let mut part = "[key: string]: ".to_owned();
                        self.write(&mut part, index, depth - 1, stack);
                        parts.push(part);
                    }
                    out.push_str("{ ");
                    out.push_str(&parts.join(", "));
                    out.push_str(" }");
                }
            }
            Ty::Union(members) => {
                for (index, member) in members.iter().enumerate() {
                    if index == COMPACT_UNION && members.len() > COMPACT_UNION + 1 {
                        out.push_str(" | …");
                        break;
                    }
                    if index > 0 {
                        out.push_str(" | ");
                    }
                    self.write(out, member, depth, stack);
                }
            }
            Ty::Intersection(members) => {
                for (index, member) in members.iter().enumerate() {
                    if index > 0 {
                        out.push_str(" & ");
                    }
                    let parentheses = matches!(member, Ty::Union(_));
                    if parentheses {
                        out.push('(');
                    }
                    self.write(out, member, depth, stack);
                    if parentheses {
                        out.push(')');
                    }
                }
            }
            Ty::Named(name) => {
                if self.expands(name, depth, stack)
                    && let Some(body) = (self.named)(name)
                {
                    stack.push(name.clone());
                    self.write(out, body, depth, stack);
                    stack.pop();
                } else {
                    out.push_str(name);
                }
            }
            Ty::Content(structured) => {
                out.push_str("ToolContent");
                if **structured != Ty::Unknown {
                    out.push('<');
                    self.write(out, structured, depth, stack);
                    out.push('>');
                }
            }
            Ty::Unknown
            | Ty::Never
            | Ty::Null
            | Ty::Undefined
            | Ty::Boolean
            | Ty::Number
            | Ty::String
            | Ty::Literal(_) => out.push_str(&render(ty, None, 0)),
        }
    }

    /// Whether a named type is written out in place: not inside itself, and not once the
    /// budget is spent.
    fn expands(&self, name: &str, depth: usize, stack: &[String]) -> bool {
        depth > 0 && !stack.iter().any(|seen| seen == name)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn object(properties: &[(&str, Ty, bool)]) -> Ty {
        Ty::Object(Object {
            properties: properties
                .iter()
                .map(|(name, ty, optional)| Property {
                    name: (*name).to_owned(),
                    ty: ty.clone(),
                    optional: *optional,
                    doc: Doc::default(),
                })
                .collect(),
            index: None,
        })
    }

    #[test]
    fn unions_flatten_and_absorb() {
        assert_eq!(
            Ty::union([Ty::String, Ty::Union(vec![Ty::Null, Ty::String]), Ty::Never]),
            Ty::Union(vec![Ty::String, Ty::Null])
        );
        assert_eq!(Ty::union([Ty::String, Ty::Unknown]), Ty::Unknown);
        assert_eq!(Ty::union([]), Ty::Never);
        assert_eq!(Ty::intersection([Ty::Unknown, Ty::String]), Ty::String);
        assert_eq!(Ty::intersection([Ty::Never, Ty::String]), Ty::Never);
        assert_eq!(Ty::intersection([]), Ty::Unknown);
    }

    #[test]
    fn writes_types_in_full() {
        let union = Ty::union([Ty::Literal(json!("open")), Ty::Literal(json!("closed"))]);
        assert_eq!(
            render(&Ty::Array(Box::new(union.clone())), None, 0),
            r#"("open" | "closed")[]"#
        );
        assert_eq!(
            render(&Ty::Array(Box::new(Ty::Literal(json!(-1)))), None, 0),
            "(-1)[]"
        );
        assert_eq!(
            render(
                &Ty::intersection([Ty::Named("A".into()), Ty::union([Ty::String, Ty::Null])]),
                Some("github"),
                0
            ),
            "github.A & (string | null)"
        );
        assert_eq!(
            render(
                &Ty::Tuple(Tuple {
                    items: vec![Ty::String, Ty::Number],
                    required: 1,
                    rest: Some(Box::new(Ty::Boolean)),
                }),
                None,
                0
            ),
            "[string, number?, ...boolean[]]"
        );
        assert_eq!(
            render(
                &object(&[("repo", Ty::String, false), ("state", union, true)]),
                None,
                0
            ),
            r#"{ repo: string; state?: "open" | "closed" }"#
        );
        assert_eq!(
            render(&Ty::Object(Object::default()), None, 0),
            "{ [key: string]: never }"
        );
        assert_eq!(
            literal(&json!({ "b-c": [1, "x"], "a": null })),
            r#"{ a: null; "b-c": [1, "x"] }"#
        );
        assert_eq!(
            render(&Ty::Content(Box::new(Ty::Unknown)), None, 0),
            "ToolContent"
        );
    }

    #[test]
    fn long_or_documented_objects_take_several_lines() {
        let mut ty = object(&[
            ("a_rather_long_property_name", Ty::String, false),
            ("another_long_property_name", Ty::Number, true),
        ]);
        assert_eq!(
            render(&ty, None, 1),
            "{\n    a_rather_long_property_name: string;\n    another_long_property_name?: number;\n  }"
        );
        if let Ty::Object(object) = &mut ty {
            object.properties.truncate(1);
            object.properties[0].doc.description = Some("A name. */ not the end".into());
            object.index = Some(Box::new(Ty::Unknown));
        }
        assert_eq!(
            render(&ty, None, 0),
            "{\n  /** A name. *\\/ not the end */\n  a_rather_long_property_name: string;\n  [key: string]: unknown;\n}"
        );
    }

    #[test]
    fn jsdoc_separates_tags_from_the_description() {
        let mut out = String::new();
        jsdoc(
            &mut out,
            &Doc {
                description: Some("First line.\n\nSecond paragraph.\n".into()),
                tags: vec!["@default 3".into()],
            },
            1,
        );
        assert_eq!(
            out,
            "  /**\n   * First line.\n   *\n   * Second paragraph.\n   *\n   * @default 3\n   */\n"
        );
    }

    #[test]
    fn compact_rendering_expands_named_types_within_a_budget() {
        let issue = object(&[
            ("number", Ty::Number, false),
            ("parent", Ty::Named("Issue".into()), true),
        ]);
        let named = |name: &str| (name == "Issue").then_some(&issue);
        let compact = Compact { named: &named };
        let list = Ty::Array(Box::new(Ty::Named("Issue".into())));
        assert_eq!(
            compact.render(&list, 2),
            "{ number: number, parent?: Issue }[]"
        );
        assert_eq!(compact.render(&list, 0), "Issue[]");
        assert_eq!(
            compact.render(&object(&[("a", issue.clone(), false)]), 1),
            "{ a: {…} }"
        );
        let many = Ty::union((0..12).map(|n| Ty::Literal(json!(n))));
        assert_eq!(
            compact.render(&many, 1),
            "0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | …"
        );
    }
}
