mod apis;
mod senses;

use anyhow::Result;
use flowcloudai_client::llm::types::{SessionEvent, TurnStatus};
use flowcloudai_client::FlowCloudAIClient;
use futures_util::StreamExt;
use std::io::{stdout, Write};
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

#[derive(Default)]
struct BotState {
    content_buf: String,
    pending_input: Option<String>,
    turns_finished: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Active {
    A,
    B,
}

#[tokio::main]
async fn main() -> Result<()> {
    let senses_a = senses::llm_a::LLMASense::new();
    let senses_b = senses::llm_b::LLMBSense::new();

    // ── 初始化客户端与插件 ──
    let mut client = FlowCloudAIClient::new(PathBuf::from("./plugins"), None)?;
    
    client.install_sense(&senses_a)?;
    client.install_sense(&senses_b)?;
    
    client.load_plugin("deepseek-llm")?;

    // ── 机器人 A ──
    let mut bot_a = client.create_llm_session("deepseek-llm", apis::DEEPSEEK.key, None)?;
    bot_a.load_sense(senses_a).await?
        .set_model("deepseek-chat").await
        .set_thinking(false).await
        .set_stream(true).await
        .set_frequency_penalty(0.0).await;

    // ── 机器人 B ──
    let mut bot_b = client.create_llm_session("deepseek-llm", apis::DEEPSEEK.key, None)?;
    bot_b.load_sense(senses_b).await?
        .set_model("deepseek-chat").await
        .set_thinking(false).await
        .set_stream(true).await;

    let (a_tx, a_rx) = mpsc::channel::<String>(32);
    let (b_tx, b_rx) = mpsc::channel::<String>(32);

    let (a_stream, _a_handle) = bot_a.run(a_rx);
    let (b_stream, _b_handle) = bot_b.run(b_rx);

    run_chat_loop(a_stream, b_stream, a_tx, b_tx).await?;

    Ok(())
}

async fn run_chat_loop(
    mut a_stream: ReceiverStream<SessionEvent>,
    mut b_stream: ReceiverStream<SessionEvent>,
    a_tx: mpsc::Sender<String>,
    b_tx: mpsc::Sender<String>,
) -> Result<()> {
    let mut a_state = BotState::default();
    let mut b_state = BotState::default();

    let mut active = Active::A;
    let mut is_reasoning = false;
    let max_turns = 10;

    loop {
        tokio::select! {
            biased;

            ev = a_stream.next(), if active == Active::A => {
                let Some(ev) = ev else { break };

                handle_event(
                    Active::A,
                    ev,
                    &a_tx,
                    &mut a_state,
                    &mut b_state,
                    &mut active,
                    &mut is_reasoning,
                ).await?;
            }

            ev = b_stream.next(), if active == Active::B => {
                let Some(ev) = ev else { break };

                handle_event(
                    Active::B,
                    ev,
                    &b_tx,
                    &mut b_state,
                    &mut a_state,
                    &mut active,
                    &mut is_reasoning,
                ).await?;
            }
        }

        if a_state.turns_finished >= max_turns || b_state.turns_finished >= max_turns {
            println!(
                "\n{}Reached max_turns={}, stop.{}",
                SYSTEM_COLOR, max_turns, COLOR_RESET
            );
            break;
        }
    }

    Ok(())
}

async fn handle_event(
    me: Active,
    ev: SessionEvent,
    me_tx: &mpsc::Sender<String>,
    me_state: &mut BotState,
    other_state: &mut BotState,
    active: &mut Active,
    is_reasoning: &mut bool,
) -> Result<()> {
    match ev {
        SessionEvent::NeedInput => {
            if let Some(msg) = me_state.pending_input.take() {
                if me_tx.send(msg).await.is_err() {
                    return Ok(());
                }
            }
        }

        SessionEvent::TurnBegin { turn_id, .. } => {
            println!(
                "\n{}=== {:?} Turn {} ==={}",
                SYSTEM_COLOR, me, turn_id, COLOR_RESET
            );
            *is_reasoning = false;
        }

        SessionEvent::ReasoningDelta(delta) => {
            *is_reasoning = true;
            print!("{}{}{}", REASONING_COLOR, delta, COLOR_RESET);
            stdout().flush().ok();
        }

        SessionEvent::ContentDelta(delta) => {
            if *is_reasoning {
                println!();
            }
            *is_reasoning = false;

            me_state.content_buf.push_str(&delta);
            print!("{}", delta);
            stdout().flush().ok();
        }

        SessionEvent::ToolCall { index, name, .. } => {
            println!(
                "\n{}[{:?} ToolCall] index={}\nname={}{}",
                TOOL_CALL_COLOR, me, index, name, COLOR_RESET
            );
        }

        SessionEvent::ToolResult {
            index,
            output,
            is_error,
        } => {
            if is_error {
                println!(
                    "\n{}[{:?} ToolResult:{}ERR{}] index={}{}\n{}{}",
                    TOOL_CALL_COLOR,
                    me,
                    TOOL_ERROR_COLOR,
                    TOOL_CALL_COLOR,
                    index,
                    TOOL_COLOR,
                    output,
                    COLOR_RESET
                );
            } else {
                println!(
                    "\n{}[{:?} ToolResult:{}OK{}] index={}{}\n{}{}",
                    TOOL_CALL_COLOR,
                    me,
                    TOOL_OK_COLOR,
                    TOOL_CALL_COLOR,
                    index,
                    TOOL_COLOR,
                    output,
                    COLOR_RESET
                );
            }
        }

        SessionEvent::TurnEnd { status, .. } => {
            println!(
                "\n{}--- {:?} TurnEnd: {:?} ---{}",
                SYSTEM_COLOR, me, status, COLOR_RESET
            );

            match status {
                TurnStatus::Ok => {
                    me_state.turns_finished += 1;

                    let msg = me_state.content_buf.trim().to_string();
                    me_state.content_buf.clear();

                    if !msg.is_empty() {
                        other_state.pending_input = Some(msg);
                    }

                    *active = match *active {
                        Active::A => Active::B,
                        Active::B => Active::A,
                    };
                }

                TurnStatus::Cancelled | TurnStatus::Interrupted => {}

                _ => {}
            }
        }

        SessionEvent::Error(msg) => {
            eprintln!("\n[{:?} SessionError]\n{}", me, msg);
        }
        _ => {}
    }

    Ok(())
}