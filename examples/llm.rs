mod senses;
mod apis;

use crate::senses::Senses;
use anyhow::Result;
use flowcloudai_client::llm::types::SessionEvent;
use flowcloudai_client::llm::types::TurnStatus;
use flowcloudai_client::AIChatSession;
use futures_util::StreamExt;
use std::io::{stdin, stdout, Write};
use tokio::sync::mpsc;

const SYSTEM_COLOR: &str = "\x1b[94m";
const REASONING_COLOR: &str = "\x1b[92m";
const TOOL_COLOR: &str = "\x1b[90m";
const TOOL_CALL_COLOR: &str = "\x1b[96m";
const TOOL_ERROR_COLOR: &str = "\x1b[91m";
const TOOL_OK_COLOR: &str = "\x1b[32m";
const COLOR_RESET: &str = "\x1b[0m";

#[tokio::main]
async fn main() -> Result<()> {
    let senses: Senses = Senses::new();

    let bot = AIChatSession::new()
        .set_api(
            apis::QWEN_LLM.url,
            apis::QWEN_LLM.key
        )
        .load_sense(senses.militech_acs).await?
        .set_model("qwen3.5-plus")
        .set_thinking(true)
        .set_frequency_penalty(0.0)
        .set_stream(true);

    let (input_tx, input_rx) = mpsc::channel::<String>(32);

    let (mut event_stream, _handle) = bot.run(input_rx);

    let mut is_reasoning = false;

    while let Some(ev) = event_stream.next().await {
        match ev {
            SessionEvent::NeedInput => {
                // 只在 Session 需要输入时读 stdin
                print!("User: ");
                stdout().flush().ok();

                let mut s = String::new();
                stdin().read_line(&mut s)?;
                let s = s.trim_end().to_string();

                if s == "exit" {
                    break;
                }

                // 发送给 Session；如果 Session 已经结束，会 Err
                if input_tx.send(s).await.is_err() {
                    break;
                }
            }

            SessionEvent::TurnBegin { turn_id } => {
                println!("\n{}=== Turn {} ==={}", SYSTEM_COLOR, turn_id, COLOR_RESET);
                is_reasoning = false;
            }

            SessionEvent::ReasoningDelta(delta) => {
                is_reasoning = true;
                // UI 层决定怎么展示 reasoning（这里用绿色）
                print!("{}{}{}", REASONING_COLOR, delta, COLOR_RESET);
                stdout().flush().ok();
            }

            SessionEvent::ContentDelta(delta) => {
                if is_reasoning {
                    println!();
                }
                is_reasoning = false;
                print!("{}", delta);
                stdout().flush().ok();
            }

            SessionEvent::ToolCall { index, name } => {
                println!("\n{}[ToolCall] index={}\nname={}{}", TOOL_CALL_COLOR, index, name, COLOR_RESET);
            }

            SessionEvent::ToolResult { index, output, is_error } => {
                if is_error {
                    println!("\n{}[ToolResult:{}ERR{}] index={}{}\n{}{}", TOOL_CALL_COLOR, TOOL_ERROR_COLOR, TOOL_CALL_COLOR, index, TOOL_COLOR, output, COLOR_RESET);
                } else {
                    println!("\n{}[ToolResult:{}OK{}] index={}{}\n{}{}", TOOL_CALL_COLOR, TOOL_OK_COLOR, TOOL_CALL_COLOR, index, TOOL_COLOR, output, COLOR_RESET);
                }
            }

            SessionEvent::TurnEnd { status } => {
                println!("\n{}--- TurnEnd: {:?} ---{}", SYSTEM_COLOR, status, COLOR_RESET);

                // 你可以在 cancelled/interrupted 时直接退出 UI
                match status {
                    TurnStatus::Cancelled | TurnStatus::Interrupted => break,
                    _ => {}
                }
            }

            SessionEvent::Error(msg) => {
                eprintln!("\n[SessionError]\n{}", msg);
                break;
            }
        }
    }
    Ok(())
}