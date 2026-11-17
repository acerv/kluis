use harness_tools::ToolError;
use serde_json::json;

#[test]
fn error_json_and_display() {
    let err = ToolError::new("a.txt: boom");
    let v = serde_json::to_value(&err).unwrap();
    assert_eq!(v, json!({"message":"a.txt: boom"}));
    assert_eq!(serde_json::from_value::<ToolError>(v).unwrap(), err);
    assert_eq!(err.to_string(), "a.txt: boom");
}
