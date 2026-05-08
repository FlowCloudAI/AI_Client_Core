// 演示 Orchestrate 接口的两种用法：
//   1. 内置 DefaultOrchestrator（基于 Sense 白名单 + task_type 策略）
//   2. 自定义 MyOrchestrator（完全自行决定每轮配置）
//
// 运行：cargo run --example orchestrate

mod apis;
mod senses;

use anyhow::Result;
use flowcloudai_client::llm::types::SessionEvent;
use flowcloudai_client::llm::types::TurnStatus;
use flowcloudai_client::{AssembledTurn, FlowCloudAIClient, Orchestrate, TaskContext};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::io::{stdin, stdout, Write};
use std::path::PathBuf;
use tokio::sync::mpsc;

// ═══════════════════════════════════════════════════════════
// 示例一：自定义 Orchestrate 实现
// ═══════════════════════════════════════════════════════════

/// 根据 `attributes["mode"]` 动态切换模型参数和系统提示注入。
///
/// 支持三种模式（通过 `handle.set_task_context` 在对话中动态切换）：
/// - "precise"  → temperature 0.0，提示"请给出精确答案"
/// - "creative" → temperature 0.9，提示"请发挥创意"
/// - 其他/未设置 → 使用模型默认值
struct ModeOrchestrator {
    base_prompt: String,
}

impl ModeOrchestrator {
    fn new(base_prompt: impl Into<String>) -> Self {
        Self {
            base_prompt: base_prompt.into(),
        }
    }
}

impl Orchestrate for ModeOrchestrator {
    fn assemble(&self, ctx: &TaskContext) -> Result<AssembledTurn> {
        let mode = ctx.attr("mode").unwrap_or("default");
        let read_only = ctx.flag("read_only");

        let (temperature, hint) = match mode {
            "precise" => (Some(0.0), "请给出精确、简洁的答案。"),
            "creative" => (Some(0.9), "请发挥创意，给出富有想象力的回答。"),
            _ => (None, ""),
        };

        let mut context_messages = vec![self.base_prompt.clone()];
        if !hint.is_empty() {
            context_messages.push(format!("[当前模式：{}] {}", mode, hint));
        }

        Ok(AssembledTurn {
            context_messages,
            temperature_override: temperature,
            read_only,
            // tool_schemas = None → 不干预工具选择，由 Session 内的 ToolRegistry 决定
            ..Default::default()
        })
    }
}

// ═══════════════════════════════════════════════════════════
// 主入口
// ═══════════════════════════════════════════════════════════

#[tokio::main]
async fn main() -> Result<()> {
    let client = FlowCloudAIClient::new(PathBuf::from("./plugins"), None)?;
    client.load_plugin("deepseek-llm")?;

    // ── 方式 A：DefaultOrchestrator（Sense 白名单 + task_type 策略）──────────
    //
    //   let acs_sense = senses::militech_acs::ACSSense::new();
    //   client.install_sense(&acs_sense)?;
    //   let orch = DefaultOrchestrator::new(
    //       Box::new(acs_sense),
    //       Arc::clone(client.tool_registry()),
    //   );
    //   let mut session = client
    //       .create_orchestrated_session("deepseek-llm", apis::DEEPSEEK.key, Box::new(orch), None)?;

    // ── 方式 B：自定义 ModeOrchestrator（本示例实际运行）─────────────────────
    let orch = ModeOrchestrator::new("你是一个通用助手，请用中文回复。");
    let mut session = client
        .create_orchestrated_session("deepseek-llm", apis::DEEPSEEK.key, Box::new(orch), None)?;

    session
        .set_model("deepseek-chat").await
        .set_stream(true).await;

    let (input_tx, input_rx) = mpsc::channel::<String>(32);
    let (mut events, handle) = session.run(input_rx);

    // 对话开始前设置初始上下文（可在每轮前更新）
    handle.set_task_context(TaskContext {
        attributes: HashMap::from([
            ("mode".to_string(), "default".to_string()),
        ]),
        ..Default::default()
    }).await.ok();

    println!("提示：输入 !precise / !creative / !default 在运行时切换模式");
    println!("      输入 exit 退出\n");

    // ── 驱动循环 ──────────────────────────────────────────────────────────────
    let handle_clone = handle.clone();
    tokio::spawn(async move {
        while let Some(ev) = events.next().await {
            match ev {
                SessionEvent::NeedInput => {
                    // 内部循环：允许连续输入切换指令，直到输入实际内容才发送给 Session
                    loop {
                        print!("User: ");
                        stdout().flush().ok();

                        let mut s = String::new();
                        stdin().read_line(&mut s).ok();
                        let s = s.trim_end().to_string();

                        if s == "exit" {
                            return;
                        }

                        // 运行时切换模式：更新上下文后继续提示输入，Session 不感知
                        if let Some(mode) = s.strip_prefix('!') {
                            let new_ctx = TaskContext {
                                attributes: HashMap::from([
                                    ("mode".to_string(), mode.to_string()),
                                ]),
                                ..Default::default()
                            };
                            handle_clone.set_task_context(new_ctx).await.ok();
                            println!("[系统] 模式已切换为：{}", mode);
                            continue; // 继续内部循环，再次提示输入
                        }

                        if input_tx.send(s).await.is_err() {
                            return;
                        }
                        break; // 发出真实输入后退出内部循环，回到事件循环
                    }
                }

                SessionEvent::TurnBegin { turn_id, .. } => {
                    println!("\n=== Turn {} ===", turn_id);
                }

                SessionEvent::ContentDelta(delta) => {
                    print!("{}", delta);
                    stdout().flush().ok();
                }

                SessionEvent::TurnEnd { status, .. } => {
                    println!("\n--- {:?} ---", status);
                    match status {
                        TurnStatus::Cancelled | TurnStatus::Interrupted => break,
                        _ => {}
                    }
                }

                SessionEvent::Error(msg) => {
                    eprintln!("\n[Error] {}", msg);
                    break;
                }

                _ => {}
            }
        }
    })
    .await?;

    Ok(())
}
