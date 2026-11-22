// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

use harness_exec::{
    dispatch_raw, Todo, TodoStatus, TodoTool, ToolContext, ToolDetails, MAX_TODO_ITEMS,
    MAX_TODO_ITEM_BYTES,
};
use serde_json::{json, Value};

fn todo(content: &str, status: TodoStatus) -> Todo {
    Todo {
        content: content.to_owned(),
        status,
    }
}

fn error(todos: Vec<Todo>) -> String {
    TodoTool { todos }.run().unwrap_err().message
}

#[test]
fn definition() {
    let tool = TodoTool::definition();
    assert_eq!(tool.name.as_ref(), "todo");
    assert_eq!(tool.schema, Some(TodoTool::schema()));
}

#[test]
fn returns_the_list() {
    let todos = vec![
        todo("plan", TodoStatus::Completed),
        todo("build", TodoStatus::InProgress),
        todo("test", TodoStatus::Pending),
    ];
    let out = TodoTool {
        todos: todos.clone(),
    }
    .run()
    .unwrap();
    assert_eq!(out.count, 3);
    assert_eq!(out.todos, todos);
}

#[test]
fn empty_list_clears() {
    let out = TodoTool { todos: vec![] }.run().unwrap();
    assert_eq!(out.count, 0);
    assert!(out.todos.is_empty());
}

#[test]
fn blank_content() {
    let err = error(vec![todo("  ", TodoStatus::Pending)]);
    assert!(err.contains("empty"), "{err}");
}

#[test]
fn content_too_long() {
    let long = "x".repeat(MAX_TODO_ITEM_BYTES + 1);
    let err = error(vec![todo(&long, TodoStatus::Pending)]);
    assert!(err.contains("bytes"), "{err}");
}

#[test]
fn too_many_items() {
    let todos = vec![todo("x", TodoStatus::Pending); MAX_TODO_ITEMS + 1];
    let err = error(todos);
    assert!(err.contains("todos are allowed"), "{err}");
}

#[test]
fn one_in_progress() {
    let err = error(vec![
        todo("a", TodoStatus::InProgress),
        todo("b", TodoStatus::InProgress),
    ]);
    assert!(err.contains("in_progress"), "{err}");
}

#[tokio::test]
async fn dispatched_as_compact_json_with_details() {
    let args = json!({ "todos": [{ "content": "build", "status": "in_progress" }] });
    let (content, details) = dispatch_raw("todo", &args, &ToolContext::default(), |_| {}).await;
    let out: Value = serde_json::from_str(&content).unwrap();
    assert_eq!(out, json!({ "count": 1 }));
    assert_eq!(
        details,
        Some(ToolDetails::Todo(vec![todo(
            "build",
            TodoStatus::InProgress
        )]))
    );
}

#[tokio::test]
async fn unknown_status() {
    let args = json!({ "todos": [{ "content": "build", "status": "done" }] });
    let (content, _) = dispatch_raw("todo", &args, &ToolContext::default(), |_| {}).await;
    let out: Value = serde_json::from_str(&content).unwrap();
    assert!(out["error"].as_str().unwrap().contains("done"), "{out}");
}
