// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use harness_core::OutputSchema;
use serde_json::{json, Value};

/// What `answer` gives against `schema`.
fn parse(schema: Value, answer: &str) -> Result<Value, String> {
    OutputSchema::new(schema).unwrap().parse(answer)
}

fn error(schema: Value, answer: &str) -> String {
    parse(schema, answer).unwrap_err()
}

#[test]
fn takes_an_answer_matching_the_schema() {
    let schema = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string", "minLength": 1 },
            "tags": { "type": "array", "items": { "enum": ["a", "b"] }, "maxItems": 2 },
            "size": { "type": ["integer", "null"], "minimum": 0 }
        },
        "required": ["name"],
        "additionalProperties": false
    });
    let answer = r#"{ "name": "x", "tags": ["a", "b"], "size": 3.0 }"#;

    let value = parse(schema, answer).unwrap();

    assert_eq!(value["tags"], json!(["a", "b"]));
}

#[test]
fn takes_an_answer_in_a_code_fence() {
    let schema = json!({ "type": "array" });

    assert_eq!(parse(schema.clone(), "```json\n[1]\n```\n"), Ok(json!([1])));
    assert_eq!(parse(schema, " [1] "), Ok(json!([1])));
}

#[test]
fn tells_where_an_answer_does_not_match() {
    let schema = json!({
        "type": "object",
        "properties": {
            "items": { "type": "array", "items": { "type": "object", "required": ["id"] } },
            "kind": { "const": "list" },
            "size": { "type": "integer", "maximum": 10 },
            "name": { "type": "string", "maxLength": 2 }
        },
        "additionalProperties": { "type": "boolean" }
    });
    let cases = [
        ("no", "the answer is not JSON"),
        ("[]", "$: expected \"object\""),
        (r#"{"items":[{"id":1},{}]}"#, "$.items[1]: `id` is missing"),
        (r#"{"kind":"map"}"#, "$.kind: expected \"list\""),
        (r#"{"size":1.5}"#, "$.size: expected \"integer\""),
        (r#"{"size":11}"#, "$.size: 11 is out of range"),
        (r#"{"name":"abc"}"#, "$.name: 3 characters is out of range"),
        (r#"{"other":1}"#, "$.other: expected \"boolean\""),
    ];

    for (answer, expected) in cases {
        let err = error(schema.clone(), answer);
        assert!(err.starts_with(expected), "{answer}: {err}");
    }
}

#[test]
fn combines_schemas() {
    let schema = json!({
        "anyOf": [{ "type": "string" }, { "type": "integer" }],
        "oneOf": [{ "minimum": 5 }, { "maximum": 7 }],
        "allOf": [{ "enum": ["a", 3, 3.5, 6, 9] }]
    });

    assert_eq!(parse(schema.clone(), "9"), Ok(json!(9)));
    assert!(error(schema.clone(), "3.5").contains("`anyOf`"));
    // 6 is both at least 5 and at most 7.
    assert!(error(schema.clone(), "6").contains("`oneOf`"));
    assert!(error(schema, "4").contains("expected one of"));
}

#[test]
fn boolean_schemas_take_anything_or_nothing() {
    assert_eq!(parse(json!(true), "1"), Ok(json!(1)));
    assert!(error(json!({ "items": false }), "[1]").starts_with("$[0]"));
}

#[test]
fn refuses_a_schema_it_cannot_check() {
    let cases = [
        (json!({ "pattern": "a" }), "$: `pattern` is not supported"),
        (
            json!({ "properties": { "a": { "$ref": "#/x" } } }),
            "$.properties.a: `$ref` is not supported",
        ),
        (json!({ "type": "text" }), "$: `type` must be"),
        (json!({ "required": "a" }), "$: `required` must be"),
        (json!({ "anyOf": [] }), "$: `anyOf` must be"),
        (json!({ "items": [1] }), "$.items: a schema must be"),
        (json!({ "maxItems": -1 }), "$: `maxItems` must be"),
    ];

    for (schema, expected) in cases {
        let err = OutputSchema::new(schema.clone()).unwrap_err();
        assert!(err.starts_with(expected), "{schema}: {err}");
    }
}

#[test]
fn instruction_holds_the_schema() {
    let schema = OutputSchema::new(json!({ "title": "T", "type": "string" })).unwrap();

    assert!(schema.instruction().contains(r#""type":"string""#));
}
