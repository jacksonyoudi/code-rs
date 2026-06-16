use anthropic_ai_sdk::{
    client::{AnthropicClient, AnthropicClientBuilder},
    types::{
        message::{
            ContentBlock, CreateMessageParams, Message, MessageClient, MessageContent,
            MessageError, RequiredMessageParams,
            Role::{self, User},
            StopReason, Tool,
        },
        message_batches::RequestCounts,
    },
};
use anyhow::{Context, Result};
use inquire::Text;
use serde::{Deserialize, Serialize};
use std::{self, process::Output, time::Duration};
use tokio::{process::Command, time::timeout};

fn get_model() -> anyhow::Result<String> {
    dotenvy::dotenv().ok();
    std::env::var("ANTHROPIC_MODEL").context("ANTHROPIC_MODEL is not set")
}

fn get_tools() -> Vec<Tool> {
    vec![Tool {
        name: "bash".to_string(),
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
    }]
}

const SYSTEM: &str = r#"You are a coding agent.
Use bash to inspect and change the workspace. Act first, then report clearly.
"#;

struct LoopState {
    client: AnthropicClient,
    pub context: Vec<Message>,
    turn_count: usize,
    transition_reason: Option<String>,
}

impl LoopState {
    fn new(client: AnthropicClient) -> Self {
        Self {
            client,
            context: Vec::new(),
            turn_count: 1,
            transition_reason: None,
        }
    }
}

/// 程序入口
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv()?;

    let anthropic_api_key =
        std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY is not set")?;
    let anthropic_base_url =
        std::env::var("ANTHROPIC_BASE_URL").context("ANTHROPIC_BASE_URL is not set")?;

    let client = AnthropicClientBuilder::new(anthropic_api_key, "")
        .with_api_base_url(anthropic_base_url)
        .build::<MessageError>()
        .context("can't create client")?;

    let mut state = LoopState::new(client.clone());

    loop {
        let query = Text::new("-- how can i help you?")
            .prompt()
            .context("An error happened or user cancelled the input.")?;

        if query.trim() == "exit()" {
            break;
        }

        state.context.push(Message::new_text(User, query));

        agent_loop(&mut state).await?;

        let Some(last) = state.context.last() else {
            continue;
        };
        println!("--- Final response:\n{}", extract_text(&last.content));
    }

    Ok(())
}

/// 安全地执行 bash 命令，包含危险命令拦截、超时控制和输出截断
pub async fn run_bash(command: &str) -> String {
    // 1. 危险命令黑名单检查
    let dangerous = ["rm -rf /", "sudo", "shutdown", "reboot", "> /dev/"];
    if dangerous.iter().any(|item| command.contains(item)) {
        return "Error: Dangerous command blocked".to_string();
    }

    // 2. 构建异步命令（通过 sh -c 启用 shell 解析）
    let child = match Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true) // 当 Child 被丢弃时自动杀死进程
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return format!("Error: {}", e),
    };

    // 3. 等待输出，带 120 秒超时
    let output_future = child.wait_with_output();
    match timeout(Duration::from_secs(120), output_future).await {
        Ok(Ok(output)) => {
            // 正常完成，合并 stdout 和 stderr
            let combined = [output.stdout, output.stderr].concat();
            let out_str = String::from_utf8_lossy(&combined);
            let trimmed = out_str.trim();

            if trimmed.is_empty() {
                "(no output)".to_string()
            } else {
                // 截取前 50000 个字符（安全处理 UTF-8 边界）
                trimmed.chars().take(50000).collect()
            }
        }
        Ok(Err(e)) => {
            // 执行错误（例如命令不存在）
            format!("Error: {}", e)
        }
        Err(_) => {
            // 超时发生：由于设置了 kill_on_drop(true)，此时 child 会被自动杀死
            "Error: Timeout (120s)".to_string()
        }
    }
}

/// 从messagecontent中提取纯文本内容
/// 支持两种格式， 直接文本和分块内容 (仅提取Text块)
fn extract_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text { content } => content.clone(),
        MessageContent::Blocks { content } => content
            .iter()
            .filter_map(|block| {
                if let ContentBlock::Text { text } = block {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<&str>>()
            .join("\n"),
    }
}

/// 执行响应中工具调用， 遍历内容块， 找到bash工具调用并执行
/// 返回None 表示没有工具调用， 返回Some包含所有的工具执行结果
async fn execute_tool_call(content: &[ContentBlock]) -> Option<Vec<ContentBlock>> {
    let mut result = vec![];
    let mut has_tool_use = false;
    for block in content {
        if let ContentBlock::ToolUse { id, name, input } = block
            && name == "bash"
            && let Some(command) = input.get("command").and_then(|v| v.as_str())
        {
            has_tool_use = true;
            let output = run_bash(command).await;

            println!("Command{} output: {}", command, output);

            result.push(ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: output,
            });
        }
    }

    if !has_tool_use {
        return None;
    }

    Some(result)
}

/// 执行一轮对话， 发送请求 --> 处理响应 --> 执行工具调用
/// 返回true表示还需继续（即有工具调用需要执行） false，表示本轮结束
async fn run_one_turn(state: &mut LoopState) -> Result<bool> {
    let request = CreateMessageParams::new(RequiredMessageParams {
        model: get_model()?,
        messages: state.context.clone(),
        max_tokens: 8000,
    })
    .with_system(SYSTEM)
    .with_tools(get_tools());
    // 调用API获取响应
    let response = state.client.create_message(Some(&request)).await?;
    // 处理响应
    state.context.push(Message::new_blocks(
        Role::Assistant,
        response.content.clone(),
    ));

    // 如果停止原因不是工具调用 如 end_turn, 则本轮结束
    if let Some(stop_reason) = response.stop_reason
        && !matches!(stop_reason, StopReason::ToolUse)
    {
        state.transition_reason = None;
        return Ok(false);
    }

    // 执行工具调用
    let Some(result) = execute_tool_call(&response.content).await else {
        state.transition_reason = None;
        return Ok(false);
    };

    // 将工具结果作为用户消息追加到上下文中，触发下一轮对话
    state.context.push(Message::new_blocks(Role::User, result));
    state.turn_count += 1;
    Ok(true)
}

async fn agent_loop(state: &mut LoopState) -> Result<()> {
    while run_one_turn(state).await? {}
    Ok(())
}
