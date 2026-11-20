//! The request of an API call, built from its args (`archi/04-tools.md`, "API tools"): the
//! path parameters percent-encoded into the path, the query, the headers, the body as JSON.
//! Parameters are written in OpenAPI's default styles, the only ones a pinned operation knows:
//!
//! | Place | Style | `"a"` | `[1, 2]` | `{"x": 1, "y": 2}` |
//! | --- | --- | --- | --- | --- |
//! | Path | `simple` | `a` | `1,2` | `x,1,y,2` |
//! | Query | `form`, exploded | `k=a` | `k=1&k=2` | `x=1&y=2` |
//! | Header | `simple` | `a` | `1,2` | `x,1,y,2` |
//!
//! An arg left out, or `null`, is not sent; a path parameter must be given.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::header::{HeaderName, HeaderValue};
use rewind_core::{Api, ApiOperation, ParameterLocation};
use serde_json::Value;
use url::Url;

/// What a path parameter's value keeps unencoded: RFC 3986's unreserved characters. A `/` of a
/// value never adds a segment.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// A request, before its authentication.
#[derive(Debug, Clone)]
pub(crate) struct Built {
    pub(crate) url: Url,
    pub(crate) headers: Vec<(HeaderName, HeaderValue)>,
    /// The JSON body.
    pub(crate) body: Option<Vec<u8>>,
}

/// The request of `operation` of `api` with `args`, or why the args do not make one.
pub(crate) fn build(api: &Api, operation: &ApiOperation, args: &Value) -> Result<Built, String> {
    let empty = serde_json::Map::new();
    let args = match args {
        Value::Object(args) => args,
        Value::Null => &empty,
        _ => return Err("its arguments must be an object".into()),
    };
    let given = |name: &str| args.get(name).filter(|value| !value.is_null());

    let mut path = String::new();
    let mut rest = operation.path.as_str();
    while let Some(start) = rest.find('{') {
        path.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("its path {:?} has an unclosed {{", operation.path))?;
        let name = &after[..end];
        let value = given(name).ok_or_else(|| format!("the path parameter {name} is missing"))?;
        let encoded: Vec<String> = simple(value)
            .iter()
            .map(|part| utf8_percent_encode(part, SEGMENT).to_string())
            .collect();
        path.push_str(&encoded.join(","));
        rest = &after[end + 1..];
    }
    path.push_str(rest);
    let mut url = Url::parse(&format!("{}{path}", api.base_url))
        .map_err(|error| format!("its URL {}{path} is not valid: {error}", api.base_url))?;

    let mut query = Vec::new();
    let mut headers = Vec::new();
    for parameter in &operation.parameters {
        let Some(value) = given(&parameter.name) else {
            continue;
        };
        match parameter.location {
            ParameterLocation::Path => {}
            ParameterLocation::Query => match value {
                Value::Array(items) => {
                    for item in items.iter().filter(|item| !item.is_null()) {
                        query.push((parameter.name.clone(), text(item)));
                    }
                }
                Value::Object(fields) => {
                    for (field, item) in fields.iter().filter(|(_, item)| !item.is_null()) {
                        query.push((field.clone(), text(item)));
                    }
                }
                other => query.push((parameter.name.clone(), text(other))),
            },
            ParameterLocation::Header => {
                let name = HeaderName::try_from(parameter.name.as_str())
                    .map_err(|_| format!("{:?} is not an HTTP header", parameter.name))?;
                let value = HeaderValue::from_str(&simple(value).join(",")).map_err(|_| {
                    format!(
                        "the header {} holds a character an HTTP header cannot carry",
                        parameter.name
                    )
                })?;
                headers.push((name, value));
            }
        }
    }
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    let body = match args.get("body") {
        Some(body) if operation.body => Some(
            serde_json::to_vec(body).map_err(|error| format!("its body is not JSON: {error}"))?,
        ),
        _ => None,
    };
    Ok(Built { url, headers, body })
}

/// A value in the `simple` style: each item of an array, each name and value of an object.
fn simple(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items.iter().map(text).collect(),
        Value::Object(fields) => fields
            .iter()
            .flat_map(|(name, value)| [name.clone(), text(value)])
            .collect(),
        other => vec![text(other)],
    }
}

/// A primitive as text: a string as it is, a number or a boolean as JSON writes it; anything
/// else as JSON.
fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use rewind_core::{ApiParameter, HttpMethod};
    use serde_json::json;

    use super::*;

    fn operation(path: &str, parameters: &[(&str, ParameterLocation)], body: bool) -> ApiOperation {
        ApiOperation {
            method: HttpMethod::Get,
            path: path.into(),
            parameters: parameters
                .iter()
                .map(|(name, location)| ApiParameter {
                    name: (*name).into(),
                    location: *location,
                })
                .collect(),
            body,
            idempotency_key: false,
        }
    }

    #[test]
    fn parameters_go_where_the_operation_says() {
        let api = Api::new("dax", "https://api.example.com/v1");
        let get = operation(
            "/measures/{id}/points/{range}",
            &[
                ("id", ParameterLocation::Path),
                ("range", ParameterLocation::Path),
                ("tag", ParameterLocation::Query),
                ("filter", ParameterLocation::Query),
                ("limit", ParameterLocation::Query),
                ("missing", ParameterLocation::Query),
                ("X-Trace", ParameterLocation::Header),
            ],
            false,
        );
        let built = build(
            &api,
            &get,
            &json!({
                "id": "a/b c?",
                "range": [1, 2],
                "tag": ["x", "y z"],
                "filter": { "unit": "kg" },
                "limit": 10,
                "missing": null,
                "X-Trace": "t-1",
                "body": { "ignored": true }
            }),
        )
        .unwrap();
        assert_eq!(
            built.url.as_str(),
            "https://api.example.com/v1/measures/a%2Fb%20c%3F/points/1,2?tag=x&tag=y+z&unit=kg&limit=10"
        );
        assert_eq!(built.headers.len(), 1);
        assert_eq!(built.headers[0].1, "t-1");
        assert!(built.body.is_none());

        let post = operation("/measures", &[], true);
        let built = build(&api, &post, &json!({ "body": { "name": "é" } })).unwrap();
        assert_eq!(built.body.unwrap(), "{\"name\":\"é\"}".as_bytes());
        assert_eq!(
            build(&api, &get, &json!({})).unwrap_err(),
            "the path parameter id is missing"
        );
    }
}
