use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::tools::read::{read, ReadOutput, ReadParams};
use crate::tools::write::{write, WriteOutput, WriteParams};

/// A tool invocation, e.g. `{"name":"read","input":{"path":"a.txt"}}`.
///
/// Tool names also accept the capitalized and upper-case forms
/// (`Read`, `READ`), since some harnesses expose tools that way.
/// Serialization always uses lowercase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", content = "input", rename_all = "lowercase")]
pub enum ToolCall {
    #[serde(alias = "Read", alias = "READ")]
    Read(ReadParams),
    #[serde(alias = "Write", alias = "WRITE")]
    Write(WriteParams),
}

/// A tool result, e.g. `{"name":"write","output":{"bytes_written":5}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", content = "output", rename_all = "lowercase")]
pub enum ToolOutput {
    Read(ReadOutput),
    Write(WriteOutput),
}

/// Run a tool call.
pub async fn execute(call: ToolCall) -> Result<ToolOutput, ToolError> {
    match call {
        ToolCall::Read(p) => read(p).await.map(ToolOutput::Read),
        ToolCall::Write(p) => write(p).await.map(ToolOutput::Write),
    }
}
