//! An OpenAPI 3.0 or 3.1 document read into a pinned API and its tools (`archi/04-tools.md`,
//! "API tools").
//!
//! | From the document | The pinned toolset |
//! | --- | --- |
//! | The first `servers[]` (its variables at their defaults, a relative URL against the document's), unless `baseUrl` is given | The API's `base_url`: `https`, or `http` on the embedded server; no user, query nor fragment |
//! | Each operation of `paths` | A tool `<api>.<operationId>`, or `<api>.<method>_<path>` without one (`post_v1_measures`) |
//! | Its path, query and header parameters (the path item's, overridden by the operation's) | Args of the same names, and the operation's `parameters`, sorted |
//! | Its `application/json` request body | The arg `body`, required when the body is |
//! | The header `Idempotency-Key` | Not an arg: the operation's `idempotency_key`, which Rewind fills |
//! | The headers of the authentication, `Accept`, `Content-Type`, `Authorization`, `traceparent`, `tracestate` | Not args: Rewind sets them |
//! | The first `2xx` response with an `application/json` schema | The output schema; without one, the tool gets an observed type |
//! | `summary` and `description` | The description |
//! | The security requirement (the document's, else its operations' when they agree), when `auth` is not given | The API's `auth`: `bearer`, `oauth2` and `openIdConnect` are `Authorization: Bearer {secret}`; `basic` is `Authorization: Basic` with the secret in base64; an `apiKey` in a header is that header |
//!
//! Refused (`invalid-toolset`), naming the operation's path: a name that is no tool name, two
//! operations of one name, two parameters of one name or one named `body`, a `$ref` to another
//! document, an `apiKey` in the query or a cookie or several schemes at once without `auth`, an
//! `operations` entry the document does not have. Left out (`skipped`): an operation with a body
//! other than `application/json`, or with a cookie parameter.

use std::collections::{BTreeMap, BTreeSet};

use rewind_core::{
    Api, ApiOperation, ApiParameter, HttpMethod, ParameterLocation, RESERVED_HEADERS,
    SecretPlacement, Tool, ToolAuth,
};
use serde_json::{Map, Value, json};
use url::Url;

use crate::schema::{Defs, Document};

/// How to read one API of a toolset: what its registration gives beside the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOptions {
    /// Its namespace in `rewind:tools`.
    pub name: String,
    /// Where its calls go; the document's first `servers[]` when `None`.
    pub base_url: Option<String>,
    /// Its `auth` as given; `Some(ToolAuth::None)` sets aside the document's security
    /// requirement, `None` follows it.
    pub auth: Option<ToolAuth>,
    /// The operations to keep, by tool name (the `operationId`, or the name made from the method
    /// and the path); every one when `None`.
    pub operations: Option<Vec<String>>,
    /// Its calls in flight on each worker; [`Api::DEFAULT_CONCURRENCY`] when `None`.
    pub concurrency: Option<u32>,
    /// Where the document was read, against which a relative server URL is resolved.
    pub document_url: Option<Url>,
    /// Whether `base_url` may be `http`: on the embedded server only.
    pub allow_http: bool,
}

impl ReadOptions {
    /// The options of the API `name`, from its document alone, over `https`.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            base_url: None,
            auth: None,
            operations: None,
            concurrency: None,
            document_url: None,
            allow_http: false,
        }
    }
}

/// An API read from its document: the API as pinned, one tool per kept operation (sorted by
/// name, with the default policy of their method), and the operations left out.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadApi {
    pub api: Api,
    pub tools: Vec<Tool>,
    pub skipped: Vec<Skipped>,
}

/// An operation left out of the toolset, and why: in the registration's answer only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// `post`, as the document writes it.
    pub method: String,
    pub path: String,
    pub operation_id: Option<String>,
    /// Written for the harness developer: `its body is application/xml`.
    pub reason: String,
}

/// Why a document cannot be read into an API: written for the harness developer, naming the
/// API and, for an operation, its method and path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("API {api}: {message}")]
pub struct ReadError {
    pub api: String,
    pub message: String,
}

/// An operation of the document, before it is read.
struct Found<'a> {
    method: HttpMethod,
    path: &'a str,
    item: &'a Value,
    operation: &'a Value,
    name: String,
    operation_id: Option<&'a str>,
}

impl Found<'_> {
    /// `POST /v1/measures`, how errors name it.
    fn label(&self) -> String {
        format!(
            "{} {}",
            self.method.as_str().to_ascii_uppercase(),
            self.path
        )
    }
}

/// Reads `document` into the API `options.name` and its tools.
pub fn read(document: &Value, options: &ReadOptions) -> Result<ReadApi, ReadError> {
    let fail = |message: String| ReadError {
        api: options.name.clone(),
        message,
    };
    let version = match (document.get("openapi"), document.get("swagger")) {
        (Some(Value::String(version)), _) => version.as_str(),
        (None, Some(_)) => {
            return Err(fail(
                "its document is Swagger 2.0: give an OpenAPI 3.0 or 3.1 document".into(),
            ));
        }
        _ => {
            return Err(fail(
                "its document is not an OpenAPI 3.0 or 3.1 document: it has no \"openapi\" \
                 version"
                    .into(),
            ));
        }
    };
    let v30 = version.starts_with("3.0");
    if !v30 && !version.starts_with("3.1") {
        return Err(fail(format!(
            "its document is OpenAPI {version}: Rewind reads 3.0 and 3.1"
        )));
    }
    let doc = Document {
        root: document,
        v30,
    };
    let base_url = base_url(document, options).map_err(fail)?;

    let found = operations(&doc).map_err(fail)?;
    let found = keep(found, options.operations.as_deref()).map_err(fail)?;
    let auth = match &options.auth {
        Some(auth) => auth.clone(),
        None => security(&doc, &found).map_err(fail)?,
    };
    let auth_header = match &auth {
        ToolAuth::Secret(SecretPlacement::Header { name, .. }) => Some(name.to_ascii_lowercase()),
        ToolAuth::Rewind => Some("authorization".to_owned()),
        _ => None,
    };

    let mut tools = Vec::new();
    let mut skipped = Vec::new();
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    for operation in &found {
        if let Some(other) = names.insert(operation.name.clone(), operation.label()) {
            return Err(fail(format!(
                "{other} and {} are both named {:?}: two tools cannot share a name",
                operation.label(),
                operation.name
            )));
        }
        match tool(&doc, &options.name, operation, auth_header.as_deref()) {
            Ok(Read::Tool(tool)) => tools.push(*tool),
            Ok(Read::Skipped(reason)) => skipped.push(Skipped {
                method: operation.method.as_str().to_owned(),
                path: operation.path.to_owned(),
                operation_id: operation.operation_id.map(str::to_owned),
                reason,
            }),
            Err(message) => return Err(fail(format!("{}: {message}", operation.label()))),
        }
    }
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    let mut api = Api::new(options.name.clone(), base_url);
    api.auth = auth;
    if let Some(concurrency) = options.concurrency {
        api.concurrency = concurrency;
    }
    Ok(ReadApi {
        api,
        tools,
        skipped,
    })
}

/// Where the calls go: `baseUrl`, or the document's first server.
fn base_url(document: &Value, options: &ReadOptions) -> Result<String, String> {
    let given = match &options.base_url {
        Some(url) => url.clone(),
        None => {
            let server = document
                .get("servers")
                .and_then(Value::as_array)
                .and_then(|servers| servers.first())
                .ok_or("its document names no servers: give its baseUrl")?;
            let mut url = server
                .get("url")
                .and_then(Value::as_str)
                .ok_or("its document's first server has no url: give its baseUrl")?
                .to_owned();
            if let Some(Value::Object(variables)) = server.get("variables") {
                for (name, variable) in variables {
                    if let Some(default) = variable.get("default").and_then(Value::as_str) {
                        url = url.replace(&format!("{{{name}}}"), default);
                    }
                }
            }
            match Url::parse(&url) {
                Ok(_) => url,
                Err(url::ParseError::RelativeUrlWithoutBase) => {
                    let Some(document_url) = &options.document_url else {
                        return Err(format!(
                            "its document's first server {url:?} is relative: give its baseUrl"
                        ));
                    };
                    document_url
                        .join(&url)
                        .map_err(|error| format!("its server {url:?} is not a URL: {error}"))?
                        .to_string()
                }
                Err(error) => return Err(format!("its server {url:?} is not a URL: {error}")),
            }
        }
    };
    let parsed = Url::parse(&given).map_err(|error| format!("its base URL {given:?}: {error}"))?;
    match parsed.scheme() {
        "https" => {}
        "http" if options.allow_http => {}
        "http" => {
            return Err(format!(
                "its base URL {given:?} is http: a deployed server calls APIs over https"
            ));
        }
        _ => return Err(format!("its base URL {given:?} is not an https URL")),
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(
            "its base URL holds a user or a password: a secret never goes in a URL, give \
             auth"
                .into(),
        );
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(format!("its base URL {given:?} has a query or a fragment"));
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(format!("its base URL {given:?} has no host"));
    }
    Ok(parsed.as_str().trim_end_matches('/').to_owned())
}

/// Every operation of `paths`, sorted by path then method, whatever the order of the document's
/// keys.
fn operations<'a>(doc: &Document<'a>) -> Result<Vec<Found<'a>>, String> {
    let Some(paths) = doc.root.get("paths") else {
        return Ok(Vec::new());
    };
    let Value::Object(paths) = paths else {
        return Err("its paths are not an object".into());
    };
    let paths: BTreeMap<&String, &Value> = paths.iter().collect();
    let mut found = Vec::new();
    for (path, item) in paths {
        let item = doc
            .deref(item)
            .map_err(|error| format!("the path {path}: {error}"))?;
        for method in [
            HttpMethod::Get,
            HttpMethod::Put,
            HttpMethod::Post,
            HttpMethod::Delete,
            HttpMethod::Options,
            HttpMethod::Head,
            HttpMethod::Patch,
            HttpMethod::Trace,
        ] {
            let Some(operation) = item.get(method.as_str()) else {
                continue;
            };
            let operation_id = operation.get("operationId").and_then(Value::as_str);
            let name = match operation_id {
                Some(id) => id.to_owned(),
                None => derived_name(method, path),
            };
            found.push(Found {
                method,
                path,
                item,
                operation,
                name,
                operation_id,
            });
        }
    }
    Ok(found)
}

/// The name of an operation without `operationId`: its method and its path, each run of other
/// characters than letters and digits an `_`: `post_v1_measures`, `get_v1_measures_id`.
fn derived_name(method: HttpMethod, path: &str) -> String {
    let mut name = method.as_str().to_owned();
    let mut gap = true;
    for c in path.chars() {
        if c.is_ascii_alphanumeric() {
            if gap {
                name.push('_');
            }
            name.push(c);
            gap = false;
        } else {
            gap = true;
        }
    }
    name
}

/// A tool's name inside its namespace: 1 to 128 ASCII letters, digits, `_`, `-` and `.`, as MCP
/// names its tools.
fn is_tool_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

/// The operations `operations` keeps, each of its entries found.
fn keep<'a>(
    found: Vec<Found<'a>>,
    operations: Option<&[String]>,
) -> Result<Vec<Found<'a>>, String> {
    let Some(wanted) = operations else {
        return Ok(found);
    };
    let wanted: BTreeSet<&str> = wanted.iter().map(String::as_str).collect();
    let names: BTreeSet<&str> = found.iter().map(|found| found.name.as_str()).collect();
    if let Some(unknown) = wanted.iter().find(|name| !names.contains(*name)) {
        return Err(format!(
            "operations names {unknown:?}, which its document does not have"
        ));
    }
    Ok(found
        .into_iter()
        .filter(|found| wanted.contains(found.name.as_str()))
        .collect())
}

/// The `auth` of the document's security requirement: the document's own, else the kept
/// operations', when they agree.
fn security(doc: &Document<'_>, found: &[Found<'_>]) -> Result<ToolAuth, String> {
    if let Some(requirements) = doc.root.get("security") {
        return requirement(doc, requirements).map(Option::unwrap_or_default);
    }
    let mut chosen: Vec<ToolAuth> = Vec::new();
    for operation in found {
        let Some(requirements) = operation.operation.get("security") else {
            continue;
        };
        let read = requirement(doc, requirements)
            .map_err(|error| format!("{}: {error}", operation.label()))?;
        if let Some(read) = read
            && !chosen.contains(&read)
        {
            chosen.push(read);
        }
    }
    match chosen.len() {
        0 | 1 => Ok(chosen.pop().unwrap_or_default()),
        _ => Err(
            "its operations require different security schemes, and an API has one auth: give \
             auth"
                .into(),
        ),
    }
}

/// The `auth` a list of security requirements means: the first alternative Rewind can follow;
/// `None` when the list is empty or every alternative is empty. Refused when no alternative
/// can be followed.
fn requirement(doc: &Document<'_>, requirements: &Value) -> Result<Option<ToolAuth>, String> {
    let Value::Array(alternatives) = requirements else {
        return Err("its security is not a list of requirements".into());
    };
    let mut reasons = Vec::new();
    let mut anonymous = alternatives.is_empty();
    for alternative in alternatives {
        let Value::Object(schemes) = alternative else {
            return Err("a security requirement is not an object".into());
        };
        match schemes.len() {
            0 => anonymous = true,
            1 => {
                let (name, _) = schemes.iter().next().unwrap_or_else(|| unreachable!());
                match scheme(doc, name)? {
                    Ok(auth) => return Ok(Some(auth)),
                    Err(reason) => reasons.push(reason),
                }
            }
            _ => {
                // Sorted: the order of a JSON object's keys depends on how serde_json is built.
                let names: BTreeSet<&str> = schemes.keys().map(String::as_str).collect();
                reasons.push(format!(
                    "it requires several schemes at once ({})",
                    names.into_iter().collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }
    if anonymous {
        return Ok(None);
    }
    Err(format!(
        "its security requirement cannot be followed: {}; give auth",
        reasons.join("; ")
    ))
}

/// The `auth` of the security scheme `name`, or why Rewind cannot follow it.
fn scheme(doc: &Document<'_>, name: &str) -> Result<Result<ToolAuth, String>, String> {
    let scheme = doc
        .root
        .pointer("/components/securitySchemes")
        .and_then(|schemes| schemes.get(name))
        .ok_or_else(|| {
            format!(
                "its security requirement names {name:?}, which components.securitySchemes \
                 does not define"
            )
        })?;
    let scheme = doc.deref(scheme)?;
    let kind = scheme
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(match kind {
        "http" => {
            let http = scheme
                .get("scheme")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            match http.as_str() {
                "bearer" => Ok(ToolAuth::Secret(SecretPlacement::bearer())),
                "basic" => Ok(ToolAuth::Secret(SecretPlacement::basic())),
                other => Err(format!("{name} is HTTP {other} authentication")),
            }
        }
        "oauth2" | "openIdConnect" => Ok(ToolAuth::Secret(SecretPlacement::bearer())),
        "apiKey" => match scheme.get("in").and_then(Value::as_str) {
            Some("header") => match scheme.get("name").and_then(Value::as_str) {
                Some(header) => Ok(ToolAuth::Secret(SecretPlacement::header(header))),
                None => Err(format!("{name} is an apiKey without a name")),
            },
            Some(place @ ("query" | "cookie")) => Err(format!(
                "{name} is an apiKey in the {place}: a secret never goes in a URL or a cookie"
            )),
            _ => Err(format!("{name} is an apiKey without a place")),
        },
        other => Err(format!("{name} is a {other:?} scheme")),
    })
}

/// What an operation becomes.
enum Read {
    Tool(Box<Tool>),
    Skipped(String),
}

/// The tool of `operation`, or why it is left out.
fn tool(
    doc: &Document<'_>,
    api: &str,
    operation: &Found<'_>,
    auth_header: Option<&str>,
) -> Result<Read, String> {
    if !is_tool_name(&operation.name) {
        return Err(match operation.operation_id {
            Some(id) => format!(
                "its operationId {id:?} is not a tool name: 1 to 128 letters, digits, _, - and ."
            ),
            None => format!(
                "the name made from its method and path, {:?}, is not a tool name: give it an \
                 operationId",
                operation.name
            ),
        });
    }

    // The path item's parameters, then the operation's, which override them by name and place.
    let mut parameters: Vec<&Value> = Vec::new();
    for list in [
        operation.item.get("parameters"),
        operation.operation.get("parameters"),
    ]
    .into_iter()
    .flatten()
    {
        let Value::Array(list) = list else {
            return Err("its parameters are not a list".into());
        };
        for parameter in list {
            let parameter = doc.deref(parameter)?;
            let key = |p: &Value| (p.get("name").cloned(), p.get("in").cloned());
            parameters.retain(|known| key(known) != key(parameter));
            parameters.push(parameter);
        }
    }

    let mut properties = Map::new();
    let mut required = Vec::new();
    let mut pinned = Vec::new();
    let mut idempotency_key = false;
    let mut defs = Defs::default();
    for parameter in parameters {
        let name = parameter
            .get("name")
            .and_then(Value::as_str)
            .ok_or("a parameter has no name")?;
        let location = match parameter.get("in").and_then(Value::as_str) {
            Some("path") => ParameterLocation::Path,
            Some("query") => ParameterLocation::Query,
            Some("header") => {
                let lower = name.to_ascii_lowercase();
                if lower == "idempotency-key" {
                    idempotency_key = true;
                    continue;
                }
                if matches!(lower.as_str(), "accept" | "content-type" | "authorization")
                    || RESERVED_HEADERS.contains(&lower.as_str())
                    || auth_header == Some(lower.as_str())
                {
                    continue;
                }
                ParameterLocation::Header
            }
            Some("cookie") => {
                return Ok(Read::Skipped(format!(
                    "it has a cookie parameter, {name}, which Rewind does not send"
                )));
            }
            other => {
                return Err(format!(
                    "its parameter {name:?} is in {}, not in a path, a query or a header",
                    other.unwrap_or("no place")
                ));
            }
        };
        if name == "body" {
            return Err(
                "a parameter is named \"body\", the arg of the request body: give the \
                 operation another name for it"
                    .into(),
            );
        }
        if properties.contains_key(name) {
            return Err(format!("two of its parameters are named {name:?}"));
        }
        let schema = match (parameter.get("schema"), parameter.get("content")) {
            (Some(schema), _) => doc.schema(schema, &mut defs)?,
            (None, Some(Value::Object(content))) => match content
                .iter()
                .min_by(|a, b| a.0.cmp(b.0))
                .map(|(_, media)| media)
            {
                Some(media) => match media.get("schema") {
                    Some(schema) => doc.schema(schema, &mut defs)?,
                    None => json!({}),
                },
                None => json!({}),
            },
            _ => json!({}),
        };
        let schema = described(schema, parameter.get("description"));
        properties.insert(name.to_owned(), schema);
        let is_required = location == ParameterLocation::Path
            || parameter.get("required") == Some(&Value::Bool(true));
        if is_required {
            required.push(Value::String(name.to_owned()));
        }
        pinned.push(ApiParameter {
            name: name.to_owned(),
            location,
        });
    }

    let mut body = false;
    if let Some(request_body) = operation.operation.get("requestBody") {
        let request_body = doc.deref(request_body)?;
        let content = request_body
            .get("content")
            .and_then(Value::as_object)
            .filter(|content| !content.is_empty());
        if let Some(content) = content {
            let json = content
                .iter()
                .filter(|(media, _)| is_json_body(media))
                .min_by(|a, b| a.0.cmp(b.0))
                .map(|(_, media)| media);
            let Some(media) = json else {
                let types: BTreeSet<&str> = content.keys().map(String::as_str).collect();
                let types: Vec<&str> = types.into_iter().collect();
                return Ok(Read::Skipped(format!(
                    "its body is {}, and Rewind sends application/json only",
                    types.join(", ")
                )));
            };
            let schema = match media.get("schema") {
                Some(schema) => doc.schema(schema, &mut defs)?,
                None => json!({}),
            };
            properties.insert(
                "body".into(),
                described(schema, request_body.get("description")),
            );
            if request_body.get("required") == Some(&Value::Bool(true)) {
                required.push("body".into());
            }
            body = true;
        }
    }

    let mut input = Map::new();
    input.insert("type".into(), "object".into());
    input.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        input.insert("required".into(), Value::Array(required));
    }
    input.insert("additionalProperties".into(), Value::Bool(false));
    let input = defs.attach(Value::Object(input));

    // Sorted by name, as the toolset pins them: the digest does not depend on the document's
    // order.
    pinned.sort_by(|a, b| a.name.cmp(&b.name));
    let pinned_operation = ApiOperation {
        method: operation.method,
        path: operation.path.to_owned(),
        parameters: pinned,
        body,
        idempotency_key,
    };
    let mut tool = Tool::api(
        format!("{api}.{}", operation.name),
        api,
        pinned_operation,
        input,
    );
    tool.description = description(operation.operation);
    tool.output_schema = output(doc, operation.operation)?;
    Ok(Read::Tool(Box::new(tool)))
}

/// Whether a media type of a request body is `application/json`, its parameters aside.
fn is_json_body(media: &str) -> bool {
    media
        .split(';')
        .next()
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
}

/// Whether a response's media type is JSON: `application/json` or a `+json` type.
pub(crate) fn is_json(media: &str) -> bool {
    let essence = media.split(';').next().unwrap_or_default().trim();
    essence.eq_ignore_ascii_case("application/json")
        || essence.to_ascii_lowercase().ends_with("+json")
}

/// `schema` with `description` when it has none.
fn described(schema: Value, description: Option<&Value>) -> Value {
    match (schema, description) {
        (Value::Object(mut schema), Some(Value::String(text)))
            if !schema.contains_key("description") && !text.trim().is_empty() =>
        {
            schema.insert("description".into(), Value::String(text.trim().to_owned()));
            Value::Object(schema)
        }
        (schema, _) => schema,
    }
}

/// `summary` and `description`, as the tool's description.
fn description(operation: &Value) -> Option<String> {
    let parts: Vec<&str> = ["summary", "description"]
        .iter()
        .filter_map(|key| operation.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .collect();
    match parts.as_slice() {
        [] => None,
        [one] => Some((*one).to_owned()),
        [summary, description] if description.starts_with(summary) => {
            Some((*description).to_owned())
        }
        parts => Some(parts.join("\n\n")),
    }
}

/// The schema of the first `2xx` response in `application/json`: `200` to `299` in order,
/// then `2XX`.
fn output(doc: &Document<'_>, operation: &Value) -> Result<Option<Value>, String> {
    let Some(Value::Object(responses)) = operation.get("responses") else {
        return Ok(None);
    };
    let mut codes: Vec<&String> = responses
        .keys()
        .filter(|code| {
            code.len() == 3
                && code.starts_with('2')
                && (code.chars().skip(1).all(|c| c.is_ascii_digit())
                    || code.eq_ignore_ascii_case("2XX"))
        })
        .collect();
    codes.sort_by_key(|code| code.to_ascii_uppercase());
    for code in codes {
        let response = doc.deref(&responses[code.as_str()])?;
        let Some(Value::Object(content)) = response.get("content") else {
            continue;
        };
        let schema = content
            .iter()
            .filter(|(media, _)| is_json_body(media))
            .min_by(|a, b| a.0.cmp(b.0))
            .and_then(|(_, media)| media.get("schema"));
        if let Some(schema) = schema {
            let mut defs = Defs::default();
            let schema = doc.schema(schema, &mut defs)?;
            return Ok(Some(defs.attach(schema)));
        }
    }
    Ok(None)
}
