// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Answers that are JSON values of a given shape, for `kl --output-schema`.
//!
//! The shape is a JSON Schema using the keywords below. Any other keyword
//! is refused when the schema is loaded, so that no answer passes a rule
//! that was never checked.

use serde_json::{Map, Value};

/// Keywords that describe a schema and constrain nothing.
const ANNOTATIONS: &[&str] = &[
    "$schema",
    "$id",
    "$comment",
    "title",
    "description",
    "default",
    "examples",
];

const TYPES: &[&str] = &[
    "null", "boolean", "object", "array", "number", "integer", "string",
];

/// A JSON Schema the final answer of a prompt has to match.
#[derive(Debug, Clone)]
pub struct OutputSchema(Value);

impl OutputSchema {
    /// `schema`, unless it uses a keyword that is not supported or gives
    /// one a value of the wrong type.
    pub fn new(schema: Value) -> Result<Self, String> {
        check(&schema, "$")?;
        Ok(Self(schema))
    }

    /// What to add to a prompt so that its answer matches the schema.
    pub fn instruction(&self) -> String {
        format!(
            "Your final answer must be one JSON value matching the JSON Schema below, \
             and nothing else: no text around it and no code fence.\n\n{}",
            self.0
        )
    }

    /// What to prompt after an answer that failed with `error`.
    pub fn retry_instruction(error: &str) -> String {
        format!(
            "Your answer is not valid: {error}. Answer again with only the JSON value \
             matching the schema."
        )
    }

    /// The value `answer` holds, when it matches the schema.
    pub fn parse(&self, answer: &str) -> Result<Value, String> {
        let value = serde_json::from_str(unfenced(answer))
            .map_err(|err| format!("the answer is not JSON: {err}"))?;
        validate(&self.0, &value, "$")?;
        Ok(value)
    }
}

/// `answer` without the code fence models like to put around JSON.
fn unfenced(answer: &str) -> &str {
    let answer = answer.trim();
    answer
        .strip_prefix("```")
        .and_then(|rest| rest.strip_suffix("```"))
        .and_then(|rest| rest.split_once('\n'))
        .map_or(answer, |(_language, body)| body.trim())
}

/// The schemas of the list `value`, the value of `keyword` at `path`.
fn schemas<'a>(value: &'a Value, keyword: &str, path: &str) -> Result<&'a Vec<Value>, String> {
    value
        .as_array()
        .filter(|list| !list.is_empty())
        .ok_or_else(|| format!("{path}: `{keyword}` must be a list of schemas"))
}

/// Refuse what [`validate`] would not check in `schema`, found at `path`.
fn check(schema: &Value, path: &str) -> Result<(), String> {
    let object = match schema {
        Value::Bool(_) => return Ok(()),
        Value::Object(object) => object,
        _ => return Err(format!("{path}: a schema must be an object or a boolean")),
    };
    for (keyword, value) in object {
        let wrong = |what: &str| Err(format!("{path}: `{keyword}` must be {what}"));
        let below = format!("{path}.{keyword}");
        match keyword.as_str() {
            "type" => {
                let known = |name: &Value| name.as_str().is_some_and(|name| TYPES.contains(&name));
                let valid = match value {
                    Value::Array(names) => !names.is_empty() && names.iter().all(known),
                    name => known(name),
                };
                if !valid {
                    return wrong("a JSON type or a list of them");
                }
            }
            "enum" if !value.is_array() => return wrong("a list"),
            "enum" | "const" => {}
            "required" => {
                let names = value.as_array();
                if !names.is_some_and(|names| names.iter().all(Value::is_string)) {
                    return wrong("a list of names");
                }
            }
            "properties" => {
                let Some(properties) = value.as_object() else {
                    return wrong("an object");
                };
                for (name, property) in properties {
                    check(property, &format!("{below}.{name}"))?;
                }
            }
            "items" | "additionalProperties" => check(value, &below)?,
            "anyOf" | "oneOf" | "allOf" => {
                for (i, option) in schemas(value, keyword, path)?.iter().enumerate() {
                    check(option, &format!("{below}[{i}]"))?;
                }
            }
            "minimum" | "maximum" if !value.is_number() => return wrong("a number"),
            "minLength" | "maxLength" | "minItems" | "maxItems" if !value.is_u64() => {
                return wrong("a count");
            }
            "minimum" | "maximum" | "minLength" | "maxLength" | "minItems" | "maxItems" => {}
            keyword if ANNOTATIONS.contains(&keyword) => {}
            _ => return Err(format!("{path}: `{keyword}` is not supported")),
        }
    }
    Ok(())
}

fn is_type(value: &Value, name: &str) -> bool {
    match name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "number" => value.is_number(),
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0)
        }
        "string" => value.is_string(),
        _ => false,
    }
}

/// Check the size `actual` of what is at `path` against the `min` and
/// `max` keywords of `schema`, counting in `unit`.
fn check_size(
    schema: &Map<String, Value>,
    (min, max): (&str, &str),
    actual: usize,
    unit: &str,
    path: &str,
) -> Result<(), String> {
    let limit = |keyword| schema.get(keyword).and_then(Value::as_u64);
    let actual = actual as u64;
    if limit(min).is_some_and(|min| actual < min) || limit(max).is_some_and(|max| actual > max) {
        return Err(format!("{path}: {actual} {unit} is out of range"));
    }
    Ok(())
}

/// Check `value`, found at `path` of the answer, against `schema`, which
/// passed [`check`].
fn validate(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    let schema = match schema {
        Value::Object(schema) => schema,
        Value::Bool(false) => return Err(format!("{path}: nothing is allowed here")),
        _ => return Ok(()),
    };
    let list = |keyword| {
        schema
            .get(keyword)
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
    };

    let matches_type = match schema.get("type") {
        Some(Value::Array(names)) => names
            .iter()
            .any(|name| name.as_str().is_some_and(|name| is_type(value, name))),
        Some(name) => name.as_str().is_some_and(|name| is_type(value, name)),
        None => true,
    };
    if !matches_type {
        return Err(format!("{path}: expected {}", schema["type"]));
    }
    if schema.contains_key("enum") && !list("enum").contains(value) {
        return Err(format!("{path}: expected one of {}", schema["enum"]));
    }
    if schema
        .get("const")
        .is_some_and(|expected| expected != value)
    {
        return Err(format!("{path}: expected {}", schema["const"]));
    }

    let number = |keyword| schema.get(keyword).and_then(Value::as_f64);
    if let Some(actual) = value.as_f64() {
        if number("minimum").is_some_and(|min| actual < min)
            || number("maximum").is_some_and(|max| actual > max)
        {
            return Err(format!("{path}: {value} is out of range"));
        }
    }
    if let Some(text) = value.as_str() {
        let limits = ("minLength", "maxLength");
        check_size(schema, limits, text.chars().count(), "characters", path)?;
    }
    if let Some(items) = value.as_array() {
        check_size(schema, ("minItems", "maxItems"), items.len(), "items", path)?;
        if let Some(item_schema) = schema.get("items") {
            for (i, item) in items.iter().enumerate() {
                validate(item_schema, item, &format!("{path}[{i}]"))?;
            }
        }
    }
    if let Some(object) = value.as_object() {
        for name in list("required").iter().filter_map(Value::as_str) {
            if !object.contains_key(name) {
                return Err(format!("{path}: `{name}` is missing"));
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        for (name, member) in object {
            let member_schema = properties
                .and_then(|properties| properties.get(name))
                .or_else(|| schema.get("additionalProperties"));
            if let Some(member_schema) = member_schema {
                validate(member_schema, member, &format!("{path}.{name}"))?;
            }
        }
    }

    for option in list("allOf") {
        validate(option, value, path)?;
    }
    let matching = |keyword| {
        list(keyword)
            .iter()
            .filter(|option| validate(option, value, path).is_ok())
            .count()
    };
    if schema.contains_key("anyOf") && matching("anyOf") == 0 {
        return Err(format!("{path}: matches none of `anyOf`"));
    }
    if schema.contains_key("oneOf") && matching("oneOf") != 1 {
        return Err(format!("{path}: must match exactly one of `oneOf`"));
    }
    Ok(())
}
