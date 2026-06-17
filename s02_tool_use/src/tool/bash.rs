use std::{borrow::Cow, time::Duration};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use tokio::{process::Command, time::timeout};

use crate::tool::Tool;
use crate::tool::ToolSpec;

pub struct BashTool;
pub fn bash_tool() -> Box<dyn Tool> {
    Box::new(BashTool {}) as Box<dyn Tool>
}

#[async_trait]
impl Tool for BashTool {
    async fn invoke(&self, input: &Value) -> Result<String> {
        let command = input
            .get("command")
            .and_then(|v| v.as_str())
            .context("Invalid command")?;

        let dangerous = ["rm -rf /", "sudo", "shutdown", "reboot", "> /dev/"];

        if dangerous.iter().any(|item| command.contains(item)) {
            return Err(anyhow::anyhow!("Error: Dangerous command blocked"));
        }

        // 构建异步命令
        let child = match Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true) // 当 Child 被丢弃时自动杀死进程
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return Err(anyhow::anyhow!("Error: {}", e)),
        };

        let output_feature = child.wait_with_output();

        match timeout(Duration::from_secs(120), output_feature).await {
            Ok(Ok(output)) => {
                let combined = [output.stdout, output.stderr].concat();
                let out_str = String::from_utf8_lossy(&combined);
                let trimmed = out_str.trim();
                if trimmed.is_empty() {
                    Ok("(no output)".to_string())
                } else {
                    Ok(trimmed.chars().take(50000).collect())
                }
            }
            Ok(Err(e)) => {
                // 执行错误（例如命令不存在）
                Err(anyhow::anyhow!("Error: {}", e))
            }
            Err(_) => {
                // 超时发生：由于设置了 kill_on_drop(true)，此时 child 会被自动杀死
                Err(anyhow::anyhow!("Error: Timeout (120s)"))
            }
        }
    }

    fn name(&self) -> Cow<'_, str> {
        "bash".into()
    }

    fn tool_spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: Some("Run a shell command in the current workspace.".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string"
                    }
                },
                "required": ["command"]
            }),
        }
    }
}
