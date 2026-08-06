//! LLM 工具调用的执行、重试、取消与占位结果处理。
//!
//! 本模块复用父模块的会话状态，只隔离工具调用这一段实现。

use super::*;

// ── 工具执行 ──

/// 工具被取消/超限时补进树的占位 tool 消息文案。
/// 前缀对齐既有 "工具执行失败: ..." 风格，便于模型理解这不是正常结果。
const CANCEL_PLACEHOLDER_REASON: &str = "工具执行失败: 用户取消了本轮对话，该工具调用未执行";
pub(super) const MAX_ROUNDS_PLACEHOLDER_REASON: &str =
    "工具执行失败: 已达最大连续工具调用轮数上限，该工具调用未执行";
const TOOL_RETRY_DELAYS_MS: [u64; 2] = [200, 800];
const MAX_TOOL_RETRY_DELAY_MS: u64 = 5_000;

pub(super) fn tool_retry_delay_ms(retry_after_ms: Option<u64>, retry_count: usize) -> u64 {
    retry_after_ms
        .unwrap_or(TOOL_RETRY_DELAYS_MS[retry_count])
        .min(MAX_TOOL_RETRY_DELAY_MS)
}

impl LLMSession {
    pub(super) async fn execute_tool_calls(
        &mut self,
        tool_calls: Vec<ToolCall>,
        enabled_tools: &Option<HashSet<String>>,
        read_only: bool,
        auto_confirm_writes: bool,
        fatal_tools: &mut HashSet<String>,
        cancel: &mut TurnCancel,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<bool> {
        let total_calls = tool_calls.len();
        let enabled_scope = enabled_tools
            .as_ref()
            .map(|tools| tools.len().to_string())
            .unwrap_or_else(|| "all".to_string());
        log::info!(
            "[client:tools][batch_start] turn_id={} calls={} read_only={} enabled_scope={}",
            self.turn_id,
            total_calls,
            read_only,
            enabled_scope
        );

        let mut calls_iter = tool_calls.into_iter().enumerate();
        while let Some((call_position, call)) = calls_iter.next() {
            if cancel.is_cancelled() {
                log::warn!(
                    "[client:tools][batch_cancelled_before_call] turn_id={} next_call_position={} calls={}",
                    self.turn_id,
                    call_position + 1,
                    total_calls
                );
                let pending: Vec<ToolCall> = std::iter::once(call)
                    .chain(calls_iter.by_ref().map(|(_, c)| c))
                    .collect();
                self.append_unexecuted_tool_placeholders(
                    CANCEL_PLACEHOLDER_REASON,
                    pending,
                    event_tx,
                )
                .await;
                return Ok(true);
            }

            let func_name = &call.function.name;
            let args_str = call.function.arguments.trim();
            log::info!(
                "[client:tools][call_start] turn_id={} call_position={}/{} index={} name={} args_chars={} args_preview={:?}",
                self.turn_id,
                call_position + 1,
                total_calls,
                call.index,
                func_name,
                args_str.chars().count(),
                Self::log_preview(args_str, 512)
            );

            let (output, is_error) = if enabled_tools
                .as_ref()
                .is_some_and(|tools| !tools.contains(func_name))
            {
                log::warn!(
                    "[client:tools][call_blocked_not_enabled] turn_id={} index={} name={}",
                    self.turn_id,
                    call.index,
                    func_name
                );
                let failure = ToolFailure::Denied {
                    reason: format!("本轮不允许调用工具 '{func_name}'"),
                };
                (failure.model_message(), true)
            } else if read_only && !self.tool_registry.is_read_tool(func_name) {
                log::warn!(
                    "[client:tools][call_blocked_read_only] turn_id={} index={} name={}",
                    self.turn_id,
                    call.index,
                    func_name
                );
                let failure = ToolFailure::Denied {
                    reason: format!(
                        "只读模式下仅允许显式标注为读的工具，'{func_name}' 未标注或为写工具"
                    ),
                };
                (failure.model_message(), true)
            } else {
                let args_v: Value = if args_str.is_empty() {
                    Value::Object(Default::default())
                } else {
                    match serde_json::from_str(args_str) {
                        Ok(v) => v,
                        Err(e) => {
                            let output = format!("工具执行失败: 工具参数不是合法 JSON: {}", e);
                            log::warn!(
                                "[client:tools][call_args_parse_failed] turn_id={} index={} name={} error={} args_preview={:?}",
                                self.turn_id,
                                call.index,
                                func_name,
                                e,
                                Self::log_preview(args_str, 512)
                            );
                            event_tx
                                .send(SessionEvent::ToolResult {
                                    index: call.index,
                                    output: output.clone(),
                                    is_error: true,
                                })
                                .await?;
                            log::info!(
                                "[client:tools][tool_result_event_sent] turn_id={} index={} name={} is_error=true output_chars={}",
                                self.turn_id,
                                call.index,
                                func_name,
                                output.chars().count()
                            );
                            let tool_call_id = Self::tool_message_id(&call, self.turn_id);
                            let _ = self.add_message(Message::tool(output, tool_call_id)).await;
                            log::info!(
                                "[client:tools][tool_message_added] turn_id={} index={} name={}",
                                self.turn_id,
                                call.index,
                                func_name
                            );
                            if cancel.is_cancelled() {
                                log::warn!(
                                    "[client:tools][batch_cancelled_after_parse_error] turn_id={} index={} name={}",
                                    self.turn_id,
                                    call.index,
                                    func_name
                                );
                                let pending: Vec<ToolCall> =
                                    calls_iter.by_ref().map(|(_, c)| c).collect();
                                self.append_unexecuted_tool_placeholders(
                                    CANCEL_PLACEHOLDER_REASON,
                                    pending,
                                    event_tx,
                                )
                                .await;
                                return Ok(true);
                            }
                            continue;
                        }
                    }
                };

                let mut retry_count = 0;
                let result = loop {
                    log::info!(
                        "[client:tools][conduct_start] turn_id={} index={} name={} attempt={}",
                        self.turn_id,
                        call.index,
                        func_name,
                        retry_count + 1
                    );
                    let conduct_started = Instant::now();
                    let conduct_fut = crate::tool::with_auto_confirm_writes(
                        auto_confirm_writes,
                        self.tool_registry.conduct(
                            func_name,
                            Some(&args_v),
                            Duration::from_secs(600),
                        ),
                    );
                    let attempt = tokio::select! {
                        result = conduct_fut => result,
                        _ = cancel.cancelled() => {
                            log::warn!(
                                "[client:tools][conduct_cancelled] turn_id={} index={} name={} elapsed_ms={}",
                                self.turn_id,
                                call.index,
                                func_name,
                                conduct_started.elapsed().as_millis()
                            );
                            let pending: Vec<ToolCall> = std::iter::once(call.clone())
                                .chain(calls_iter.by_ref().map(|(_, c)| c))
                                .collect();
                            self.append_unexecuted_tool_placeholders(
                                CANCEL_PLACEHOLDER_REASON,
                                pending,
                                event_tx,
                            )
                            .await;
                            return Ok(true);
                        },
                    };
                    match attempt {
                        Ok(output) => {
                            log::info!(
                                "[client:tools][conduct_done] turn_id={} index={} name={} attempt={} elapsed_ms={} output_chars={}",
                                self.turn_id,
                                call.index,
                                func_name,
                                retry_count + 1,
                                conduct_started.elapsed().as_millis(),
                                output.chars().count()
                            );
                            break Ok(output);
                        }
                        Err(error) => {
                            let failure = ToolFailure::classify(&error);
                            log::warn!(
                                "[client:tools][conduct_failed] turn_id={} index={} name={} attempt={} elapsed_ms={} class={:?} error={}",
                                self.turn_id,
                                call.index,
                                func_name,
                                retry_count + 1,
                                conduct_started.elapsed().as_millis(),
                                failure,
                                error
                            );
                            if let ToolFailure::Transient { retry_after_ms } = &failure
                                && retry_count < TOOL_RETRY_DELAYS_MS.len()
                            {
                                let delay_ms = tool_retry_delay_ms(*retry_after_ms, retry_count);
                                retry_count += 1;
                                event_tx
                                    .send(SessionEvent::ToolRetrying {
                                        index: call.index,
                                        name: func_name.clone(),
                                        attempt: retry_count,
                                        max_retries: TOOL_RETRY_DELAYS_MS.len(),
                                        delay_ms,
                                    })
                                    .await?;
                                tokio::select! {
                                    _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {},
                                    _ = cancel.cancelled() => {
                                        let pending: Vec<ToolCall> = std::iter::once(call.clone())
                                            .chain(calls_iter.by_ref().map(|(_, c)| c))
                                            .collect();
                                        self.append_unexecuted_tool_placeholders(
                                            CANCEL_PLACEHOLDER_REASON,
                                            pending,
                                            event_tx,
                                        )
                                        .await;
                                        return Ok(true);
                                    },
                                }
                                continue;
                            }
                            break Err(failure);
                        }
                    }
                };
                match result {
                    Ok(output) => (output, false),
                    Err(failure) => {
                        if failure == ToolFailure::Fatal {
                            fatal_tools.insert(func_name.clone());
                        }
                        (failure.model_message(), true)
                    }
                }
            };

            let tool_call_id = Self::tool_message_id(&call, self.turn_id);

            event_tx
                .send(SessionEvent::ToolResult {
                    index: call.index,
                    output: output.clone(),
                    is_error,
                })
                .await?;
            log::info!(
                "[client:tools][tool_result_event_sent] turn_id={} index={} name={} is_error={} output_chars={} output_preview={:?}",
                self.turn_id,
                call.index,
                func_name,
                is_error,
                output.chars().count(),
                Self::log_preview(&output, 512)
            );

            let _ = self.add_message(Message::tool(output, tool_call_id)).await;
            log::info!(
                "[client:tools][tool_message_added] turn_id={} index={} name={}",
                self.turn_id,
                call.index,
                func_name
            );
            if cancel.is_cancelled() {
                log::warn!(
                    "[client:tools][batch_cancelled_after_call] turn_id={} index={} name={}",
                    self.turn_id,
                    call.index,
                    func_name
                );
                let pending: Vec<ToolCall> = calls_iter.by_ref().map(|(_, c)| c).collect();
                self.append_unexecuted_tool_placeholders(
                    CANCEL_PLACEHOLDER_REASON,
                    pending,
                    event_tx,
                )
                .await;
                return Ok(true);
            }
        }

        log::info!(
            "[client:tools][batch_done] turn_id={} calls={}",
            self.turn_id,
            total_calls
        );
        Ok(false)
    }

    fn log_preview(value: &str, max_chars: usize) -> String {
        let mut chars = value.chars();
        let preview: String = chars.by_ref().take(max_chars).collect();
        if chars.next().is_some() {
            format!("{}...(truncated)", preview)
        } else {
            preview
        }
    }

    /// tool 消息的 tool_call_id：provider 给了真实 id 就用它（与 assistant 侧
    /// tool_calls 里的 id 配对），否则退回合成 ID。
    fn tool_message_id(call: &ToolCall, turn_id: u64) -> String {
        call.id
            .as_deref()
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| Self::synth_tool_call_id(turn_id, call.index))
    }

    /// 为未执行的 tool_calls 补占位 tool 消息。
    ///
    /// assistant(tool_calls) 节点已入树且会被持久化；任何提前退出若不为每个
    /// call 留下配对的 tool 消息，该历史再发给 OpenAI 兼容 API 会被 400 拒绝
    /// 且每轮复发。树写入优先于事件——事件发送失败只影响本次 UI 展示。
    pub(super) async fn append_unexecuted_tool_placeholders(
        &self,
        reason: &str,
        calls: Vec<ToolCall>,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) {
        for call in calls {
            let tool_call_id = Self::tool_message_id(&call, self.turn_id);
            let _ = self.add_message(Message::tool(reason, tool_call_id)).await;
            let _ = event_tx
                .send(SessionEvent::ToolResult {
                    index: call.index,
                    output: reason.to_string(),
                    is_error: true,
                })
                .await;
            log::info!(
                "[client:tools][placeholder_tool_message_added] turn_id={} index={} name={}",
                self.turn_id,
                call.index,
                call.function.name
            );
        }
    }

    #[inline]
    fn synth_tool_call_id(turn_id: u64, index: usize) -> String {
        format!("t{}:idx:{}", turn_id, index)
    }
}
