use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::path::expand_tilde;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteParams {
    pub path: String,
    pub content: String,
}

impl WriteParams {
    /// JSON Schema of these parameters.
    pub fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteOutput {
    pub bytes_written: usize,
}

/// Create or overwrite a file, creating missing parent directories.
///
/// A leading `~` in the path expands to `$HOME`. Existing targets must be
/// regular files (symlinks to regular files are followed); directories,
/// FIFOs, devices and broken symlinks are rejected.
pub async fn write(p: WriteParams) -> Result<WriteOutput, ToolError> {
    // Without this, the OS reports a confusing "Is a directory" error
    // even when nothing exists at the path.
    if p.path.ends_with('/') {
        return Err(ToolError::new(format!(
            "{}: path ends with '/', expected a file",
            p.path
        )));
    }
    let path = expand_tilde(&p.path);

    match tokio::fs::metadata(&path).await {
        Ok(meta) if meta.is_dir() => {
            return Err(ToolError::new(format!("{}: is a directory", p.path)));
        }
        // FIFOs, sockets and devices can block forever or have side effects.
        Ok(meta) if !meta.is_file() => {
            return Err(ToolError::new(format!("{}: not a regular file", p.path)));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // The path itself exists but its target does not.
            if tokio::fs::symlink_metadata(&path).await.is_ok() {
                return Err(ToolError::new(format!("{}: broken symlink", p.path)));
            }
        }
        Err(e) => return Err(ToolError::io(&p.path, e)),
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::io(&p.path, e))?;
        }
    }

    tokio::fs::write(&path, p.content.as_bytes())
        .await
        .map_err(|e| ToolError::io(&p.path, e))?;

    Ok(WriteOutput {
        bytes_written: p.content.len(),
    })
}
