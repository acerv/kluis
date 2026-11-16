//! Simulates a host: parse a model tool call, execute it, print the result.

use harness_tools::{execute, ToolCall};

#[tokio::main]
async fn main() {
    let path = std::env::temp_dir().join("harness-tools-example.txt");
    let path = path.to_string_lossy();

    let calls = [
        format!(r#"{{"name":"write","input":{{"path":"{path}","content":"hello\nworld\n"}}}}"#),
        format!(r#"{{"name":"read","input":{{"path":"{path}","offset":2}}}}"#),
        r#"{"name":"read","input":{"path":"/does/not/exist"}}"#.to_string(),
    ];

    for json in calls {
        println!("call:   {json}");
        let call: ToolCall = match serde_json::from_str(&json) {
            Ok(c) => c,
            Err(e) => {
                println!("error:  invalid tool call: {e}\n");
                continue;
            }
        };
        match execute(call).await {
            Ok(out) => println!("output: {}\n", serde_json::to_string(&out).unwrap()),
            Err(err) => println!("error:  {}\n", serde_json::to_string(&err).unwrap()),
        }
    }
}
