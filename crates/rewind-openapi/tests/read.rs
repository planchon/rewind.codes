//! Reading OpenAPI documents into pinned APIs and tools (`archi/04-tools.md`, "API tools"):
//! a 3.0 and a 3.1 document from `fixtures/`, and the refusals.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use rewind_core::{
    ApiParameter, HttpMethod, Interruption, ParameterLocation, SecretPlacement, Tool, ToolAuth,
    Toolset,
};
use rewind_engine::schema::Validator;
use rewind_openapi::{ReadApi, ReadOptions, read};
use serde_json::{Value, json};
use url::Url;

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn tool<'a>(read: &'a ReadApi, name: &str) -> &'a Tool {
    read.tools
        .iter()
        .find(|tool| tool.name == name)
        .unwrap_or_else(|| panic!("no tool {name}"))
}

/// The schema compiles, and accepts `args`.
fn accepts(schema: &Value, args: &Value) -> bool {
    let validator = Validator::compile(schema);
    assert_eq!(validator.unusable(), None, "{schema}");
    validator.check("args", args).is_ok()
}

#[test]
fn an_openapi_3_0_document_is_read_into_tools() {
    let read = read(&fixture("measures-3.0.json"), &ReadOptions::new("dax")).unwrap();
    assert_eq!(read.api.base_url, "https://eu.api.example.com/v1");
    assert_eq!(read.api.auth, ToolAuth::Secret(SecretPlacement::bearer()));
    let names: Vec<&str> = read.tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "dax.createMeasure",
            "dax.deleteMeasure",
            "dax.getMeasure",
            "dax.listMeasures",
            "dax.renameMeasure"
        ]
    );
    let skipped: Vec<(&str, &str, &str)> = read
        .skipped
        .iter()
        .map(|skipped| {
            (
                skipped.method.as_str(),
                skipped.path.as_str(),
                skipped.reason.as_str(),
            )
        })
        .collect();
    assert_eq!(
        skipped,
        [
            (
                "put",
                "/measures/{id}/export",
                "its body is application/xml, and Rewind sends application/json only"
            ),
            (
                "get",
                "/session",
                "it has a cookie parameter, sid, which Rewind does not send"
            )
        ]
    );
    assert_eq!(
        read.skipped[0].operation_id.as_deref(),
        Some("importMeasure")
    );

    // The body's schema from a request body and a component, 3.0's keywords converted.
    let create = tool(&read, "dax.createMeasure");
    let operation = create.operation.as_ref().unwrap();
    assert_eq!(operation.method, HttpMethod::Post);
    assert!(operation.body && operation.idempotency_key);
    assert!(operation.parameters.is_empty(), "Idempotency-Key is no arg");
    assert_eq!(create.policy.on_interruption, Interruption::Retry);
    assert_eq!(
        create.description.as_deref(),
        Some("Create a measure.\n\nCreates it once per idempotency key.")
    );
    assert_eq!(create.input_schema["required"], json!(["body"]));
    assert_eq!(
        create.input_schema["$defs"]["NewMeasure"]["properties"]["threshold"],
        json!({ "type": "number", "exclusiveMinimum": 0 })
    );
    assert!(accepts(
        &create.input_schema,
        &json!({ "body": { "name": "weight", "threshold": 1 } })
    ));
    assert!(!accepts(
        &create.input_schema,
        &json!({ "body": { "name": "weight", "threshold": 0 } })
    ));
    assert!(!accepts(&create.input_schema, &json!({})));
    let output = create.output_schema.as_ref().unwrap();
    assert_eq!(output["$ref"], "#/$defs/Measure");
    assert_eq!(
        output["$defs"]["Measure"]["properties"]["note"],
        json!({ "type": ["string", "null"] })
    );
    assert_eq!(
        output["$defs"]["Measure"]["properties"]["parent"],
        json!({ "$ref": "#/$defs/Measure" })
    );
    assert!(accepts(
        output,
        &json!({ "id": "m", "name": "w", "note": null })
    ));

    // A path item's parameter, by $ref; a PATCH without Idempotency-Key is at most once.
    let get = tool(&read, "dax.getMeasure");
    assert_eq!(
        get.operation.as_ref().unwrap().parameters,
        [ApiParameter {
            name: "id".into(),
            location: ParameterLocation::Path
        }]
    );
    assert_eq!(
        get.input_schema["properties"]["id"],
        json!({ "type": "string", "description": "The measure's id." })
    );
    assert_eq!(get.input_schema["required"], json!(["id"]));
    assert!(!accepts(
        &get.input_schema,
        &json!({ "id": "m", "other": 1 })
    ));
    let rename = tool(&read, "dax.renameMeasure");
    assert_eq!(rename.policy.on_interruption, Interruption::AtMostOnce);
    assert!(rename.output_schema.is_none());
    assert_eq!(
        tool(&read, "dax.deleteMeasure").policy.on_interruption,
        Interruption::Retry
    );

    // Accept is not an arg: Rewind sets it.
    let list = tool(&read, "dax.listMeasures");
    let parameters: Vec<&str> = list
        .operation
        .as_ref()
        .unwrap()
        .parameters
        .iter()
        .map(|parameter| parameter.name.as_str())
        .collect();
    assert_eq!(parameters, ["limit", "tag"]);

    // What is read is a toolset, with a digest that does not depend on the reading's order.
    let toolset = Toolset::from_parts([], [read.api.clone()], read.tools.clone()).unwrap();
    let mut reversed = read.tools.clone();
    reversed.reverse();
    assert_eq!(
        toolset.digest(),
        Toolset::from_parts([], [read.api.clone()], reversed)
            .unwrap()
            .digest()
    );
}

#[test]
fn an_openapi_3_1_document_is_read_into_tools() {
    let mut options = ReadOptions::new("crm");
    options.document_url = Some(Url::parse("https://crm.example.com/openapi.json").unwrap());
    let read = read(&fixture("crm-3.1.json"), &options).unwrap();
    // A relative server, against the document's URL.
    assert_eq!(read.api.base_url, "https://crm.example.com/api");
    // The first alternative of the requirement Rewind can follow: the key in a header.
    assert_eq!(
        read.api.auth,
        ToolAuth::Secret(SecretPlacement::header("X-Api-Key"))
    );
    let names: Vec<&str> = read.tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(
        names,
        ["crm.contacts.list", "crm.post_contacts_contactId_notes"]
    );
    let note = tool(&read, "crm.post_contacts_contactId_notes");
    let parameters: Vec<(&str, ParameterLocation)> = note
        .operation
        .as_ref()
        .unwrap()
        .parameters
        .iter()
        .map(|parameter| (parameter.name.as_str(), parameter.location))
        .collect();
    // The header of the authentication is no arg; the parameters are sorted by name.
    assert_eq!(
        parameters,
        [
            ("X-Request-Source", ParameterLocation::Header),
            ("contactId", ParameterLocation::Path)
        ]
    );
    assert!(note.operation.as_ref().unwrap().body);
    assert_eq!(note.description.as_deref(), Some("Add a note."));
    // `2XX`, and `$id` left out: the references are the tool's own.
    let output = note.output_schema.as_ref().unwrap();
    assert!(output["$defs"]["Contact"].get("$id").is_none());
    assert!(accepts(
        output,
        &json!({ "name": null, "manager": { "name": "Ada" } })
    ));
    assert!(accepts(
        &note.input_schema,
        &json!({ "contactId": 4, "body": { "text": "hi" } })
    ));
    assert!(tool(&read, "crm.contacts.list").output_schema.is_none());
    Toolset::from_parts([], [read.api], read.tools).unwrap();
}

#[test]
fn the_registration_s_options_win_over_the_document() {
    let mut options = ReadOptions::new("dax");
    options.base_url = Some("http://127.0.0.1:8080/v1/".into());
    options.allow_http = true;
    options.auth = Some(ToolAuth::Rewind);
    options.operations = Some(vec!["getMeasure".into(), "createMeasure".into()]);
    options.concurrency = Some(2);
    let read = read(&fixture("measures-3.0.json"), &options).unwrap();
    assert_eq!(read.api.base_url, "http://127.0.0.1:8080/v1");
    assert_eq!(read.api.auth, ToolAuth::Rewind);
    assert_eq!(read.api.concurrency, 2);
    assert_eq!(read.tools.len(), 2);
    assert!(read.skipped.is_empty());
    // `auth: none` sets aside the document's requirement.
    let mut options = ReadOptions::new("dax");
    options.auth = Some(ToolAuth::None);
    assert_eq!(
        read_error(&fixture("measures-3.0.json"), &options),
        None,
        "auth none reads"
    );
}

fn read_error(document: &Value, options: &ReadOptions) -> Option<String> {
    read(document, options).err().map(|error| error.to_string())
}

fn refused(document: Value) -> String {
    read_error(&document, &ReadOptions::new("api")).expect("refused")
}

/// A document with these paths and components.
fn document(paths: Value, components: Value) -> Value {
    json!({
        "openapi": "3.1.0",
        "info": { "title": "t", "version": "1" },
        "servers": [{ "url": "https://api.example.com" }],
        "paths": paths,
        "components": components
    })
}

#[test]
fn documents_rewind_cannot_follow_are_refused() {
    let ok = json!({ "200": { "description": "ok" } });
    assert_eq!(
        refused(json!({ "swagger": "2.0" })),
        "API api: its document is Swagger 2.0: give an OpenAPI 3.0 or 3.1 document"
    );
    assert_eq!(
        refused(json!({ "openapi": "4.0.0" })),
        "API api: its document is OpenAPI 4.0.0: Rewind reads 3.0 and 3.1"
    );
    assert_eq!(
        refused(document(
            json!({
                "/a": { "get": { "operationId": "same", "responses": ok } },
                "/b": { "get": { "operationId": "same", "responses": ok } }
            }),
            json!({})
        )),
        "API api: GET /a and GET /b are both named \"same\": two tools cannot share a name"
    );
    assert_eq!(
        refused(document(
            json!({ "/a": { "get": { "operationId": "list items", "responses": ok } } }),
            json!({})
        )),
        "API api: GET /a: its operationId \"list items\" is not a tool name: 1 to 128 letters, \
         digits, _, - and ."
    );
    assert!(
        refused(document(
            json!({ "/a": { "post": {
                "parameters": [{ "name": "body", "in": "query", "schema": {} }],
                "responses": ok
            } } }),
            json!({})
        ))
        .starts_with("API api: POST /a: a parameter is named \"body\"")
    );
    assert_eq!(
        refused(document(
            json!({ "/a/{id}": { "get": {
                "parameters": [
                    { "name": "id", "in": "path", "required": true, "schema": {} },
                    { "name": "id", "in": "query", "schema": {} }
                ],
                "responses": ok
            } } }),
            json!({})
        )),
        "API api: GET /a/{id}: two of its parameters are named \"id\""
    );
    assert!(
        refused(document(
            json!({ "/a": { "get": { "responses": { "200": { "description": "ok", "content": {
                "application/json": { "schema": { "$ref": "https://other.example.com/s.json#/S" } }
            } } } } } }),
            json!({})
        ))
        .contains("is to another document, which Rewind does not read")
    );
    let query_key = json!({ "securitySchemes": {
        "key": { "type": "apiKey", "in": "query", "name": "key" },
        "bearer": { "type": "http", "scheme": "bearer" }
    } });
    let mut in_query = document(
        json!({ "/a": { "get": { "responses": ok } } }),
        query_key.clone(),
    );
    in_query["security"] = json!([{ "key": [] }]);
    assert_eq!(
        refused(in_query.clone()),
        "API api: its security requirement cannot be followed: key is an apiKey in the query: \
         a secret never goes in a URL or a cookie; give auth"
    );
    let mut options = ReadOptions::new("api");
    options.auth = Some(ToolAuth::Secret(SecretPlacement::header("X-Key")));
    assert_eq!(read_error(&in_query, &options), None, "auth given reads");
    let mut both = in_query.clone();
    both["security"] = json!([{ "key": [], "bearer": [] }]);
    assert_eq!(
        refused(both),
        "API api: its security requirement cannot be followed: it requires several schemes at \
         once (bearer, key); give auth"
    );
    let different = document(
        json!({
            "/a": { "get": { "security": [{ "key": [] }], "responses": ok } },
            "/b": { "get": { "security": [{ "bearer": [] }], "responses": ok } }
        }),
        json!({ "securitySchemes": {
            "key": { "type": "apiKey", "in": "header", "name": "X-Key" },
            "bearer": { "type": "http", "scheme": "bearer" }
        } }),
    );
    assert_eq!(
        refused(different.clone()),
        "API api: its operations require different security schemes, and an API has one auth: \
         give auth"
    );
    // Kept apart by `operations`, they agree.
    let mut options = ReadOptions::new("api");
    options.operations = Some(vec!["get_b".into()]);
    let one = read(&different, &options).unwrap();
    assert_eq!(one.api.auth, ToolAuth::Secret(SecretPlacement::bearer()));
    options.operations = Some(vec!["get_c".into()]);
    assert_eq!(
        read_error(&different, &options).unwrap(),
        "API api: operations names \"get_c\", which its document does not have"
    );

    // Where the calls go.
    let mut http = document(json!({}), json!({}));
    http["servers"] = json!([{ "url": "http://api.example.com" }]);
    assert_eq!(
        refused(http),
        "API api: its base URL \"http://api.example.com\" is http: a deployed server calls APIs \
         over https"
    );
    let mut relative = document(json!({}), json!({}));
    relative["servers"] = json!([{ "url": "/v1" }]);
    assert_eq!(
        refused(relative),
        "API api: its document's first server \"/v1\" is relative: give its baseUrl"
    );
    let mut user = document(json!({}), json!({}));
    user["servers"] = json!([{ "url": "https://ada:secret@api.example.com" }]);
    assert!(refused(user).contains("a secret never goes in a URL"));
    let mut none = document(json!({}), json!({}));
    none.as_object_mut().unwrap().remove("servers");
    assert_eq!(
        refused(none),
        "API api: its document names no servers: give its baseUrl"
    );
}
