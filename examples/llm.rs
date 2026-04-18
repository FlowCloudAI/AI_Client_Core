mod apis;
mod senses;

use anyhow::Result;
use flowcloudai_client::llm::types::SessionEvent;
use flowcloudai_client::llm::types::TurnStatus;
use flowcloudai_client::FlowCloudAIClient;
use futures_util::StreamExt;
use std::io::{stdin, stdout, Write};
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

const SYSTEM_COLOR: &str = "\x1b[94m";
const REASONING_COLOR: &str = "\x1b[92m";
const TOOL_COLOR: &str = "\x1b[90m";
const TOOL_CALL_COLOR: &str = "\x1b[96m";
const TOOL_ERROR_COLOR: &str = "\x1b[91m";
const TOOL_OK_COLOR: &str = "\x1b[32m";
const COLOR_RESET: &str = "\x1b[0m";

#[tokio::main]
async fn main() -> Result<()> {
    // ── 初始化客户端，扫描 ./plugins 目录 ──
    let mut client = FlowCloudAIClient::new(PathBuf::from("./plugins"), None)?;

    let acs_sense = senses::militech_acs::ACSSense::new();

    // load_plugin 现在只需要 id，Kind 由插件 manifest 声明
    client.load_plugin("deepseek-llm")?;

    // 先安装工具到全局 ToolRegistry
    client.install_sense(&acs_sense)?;

    // api_key 在创建 session 时传入；plugin_id=None 表示直通模式
    let mut session = client.create_llm_session("deepseek-llm", apis::DEEPSEEK.key)?;

    session
        .load_sense(acs_sense).await?
        .set_model("deepseek-chat").await
        .set_thinking(true).await
        .set_stream(true).await
        .set_temperature(0.75).await;

    println!("[debug] after install_sense, tool count: {:?}", client.tool_registry().tool_names());

    let (input_tx, input_rx) = mpsc::channel::<String>(32);

    // run() 返回事件流 + 句柄；句柄用于在任意时刻修改对话参数
    let (event_stream, _handle) = session.run(input_rx);



    run_chat_loop(event_stream, input_tx).await?;

    Ok(())
}

async fn run_chat_loop(
    mut events: ReceiverStream<SessionEvent>,
    input_tx: mpsc::Sender<String>,
) -> Result<()> {
    let mut is_reasoning = false;

    while let Some(ev) = events.next().await {
        match ev {
            SessionEvent::NeedInput => {
                print!("User: ");
                stdout().flush().ok();

                let mut s = String::new();
                stdin().read_line(&mut s)?;
                let s = s.trim_end().to_string();

                if s == "exit" {
                    break;
                }

                if input_tx.send(s).await.is_err() {
                    break;
                }
            }

            SessionEvent::TurnBegin { turn_id, .. } => {
                println!("\n{}=== Turn {} ==={}", SYSTEM_COLOR, turn_id, COLOR_RESET);
                is_reasoning = false;
            }

            SessionEvent::ReasoningDelta(delta) => {
                is_reasoning = true;
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

            SessionEvent::ToolCall { index, name, .. } => {
                println!(
                    "\n{}[ToolCall] index={}\nname={}{}",
                    TOOL_CALL_COLOR, index, name, COLOR_RESET
                );
            }

            SessionEvent::ToolResult { index, output, is_error } => {
                if is_error {
                    println!(
                        "\n{}[ToolResult:{}ERR{}] index={}{}\n{}{}",
                        TOOL_CALL_COLOR, TOOL_ERROR_COLOR, TOOL_CALL_COLOR,
                        index, TOOL_COLOR, output, COLOR_RESET
                    );
                } else {
                    println!(
                        "\n{}[ToolResult:{}OK{}] index={}{}\n{}{}",
                        TOOL_CALL_COLOR, TOOL_OK_COLOR, TOOL_CALL_COLOR,
                        index, TOOL_COLOR, output, COLOR_RESET
                    );
                }
            }

            SessionEvent::TurnEnd { status, .. } => {
                println!(
                    "\n{}--- TurnEnd: {:?} ---{}",
                    SYSTEM_COLOR, status, COLOR_RESET
                );
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