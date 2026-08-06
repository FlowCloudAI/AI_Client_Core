//! LLM 供应商请求的映射、发送与响应归一化。
//!
//! 本模块只拆分 `LLMSession` 的实现位置；状态与生命周期仍由父模块的驱动循环管理。

use super::*;

// ── 插件映射（核心变化点） ──

impl LLMSession {
    /// 请求转换：acquire mapper → map → release（自动）。
    fn prepare_request(&self, req: &ChatRequest) -> Result<String> {
        self.pipeline
            .validate_llm_request(&req.model, req.thinking_effort)?;
        self.pipeline.prepare_request_body(req)
    }

    /// 响应转换。
    fn normalize_response(&self, raw: &str) -> Result<String> {
        self.pipeline.map_response(raw)
    }

    /// 流式行转换。
    fn normalize_stream_line(&self, line: &str) -> Result<String> {
        self.pipeline.map_stream_line(line)
    }
}

// ── 请求 & 响应处理 ──

impl LLMSession {
    pub(super) async fn send_and_process(
        &mut self,
        req: &ChatRequest,
        cancel: &mut TurnCancel,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<TurnOutput> {
        if cancel.is_cancelled() {
            return Ok(cancelled_turn_output(
                String::new(),
                String::new(),
                Vec::new(),
                None,
            ));
        }

        let calibration_factor = self.token_calibrator.factor();
        let (request_head, head_path) = {
            let tree = self.tree.read().await;
            (tree.head(), tree.path_to_head())
        };
        let baseline = self
            .last_baseline
            .as_ref()
            .filter(|baseline| baseline.extends_request(req, request_head, &head_path))
            .cloned();
        let estimate_source = if baseline.is_some() {
            EstimateSource::Baseline
        } else {
            EstimateSource::Full
        };
        log::info!(
            "[client:llm][token_estimate] turn_id={} estimate_source={} messages={}",
            self.turn_id,
            estimate_source.as_str(),
            req.messages.len()
        );
        let mut outgoing = req.clone();
        if let Some(context_window_tokens) = self.config.context_window_tokens
            && let Some(report) = trim_request_for_window(
                &mut outgoing,
                context_window_tokens,
                calibration_factor,
                baseline.as_ref(),
                TrimOptions::NORMAL,
            )?
        {
            Self::emit_context_trimmed(event_tx, &report).await?;
        }

        let base_estimate = estimate_request_tokens(&outgoing);
        let first_result = self.send_once(&outgoing, cancel, event_tx).await;
        let Err(ref error) = first_result else {
            self.observe_token_usage(&outgoing, request_head, base_estimate, &first_result);
            return first_result;
        };
        let Some(client_error) = ClientError::from_anyhow(error) else {
            return first_result;
        };
        let Some((mut repaired, rule)) = Self::repair_after_bad_request(&outgoing, client_error)
        else {
            return first_result;
        };
        if cancel.is_cancelled() {
            return Ok(cancelled_turn_output(
                String::new(),
                String::new(),
                Vec::new(),
                None,
            ));
        }
        log::warn!(
            "[client:llm][request_auto_repair] turn_id={} rule={} retry=1",
            self.turn_id,
            rule
        );
        if rule == "unsupported_reasoning_content" {
            self.strip_reasoning_content = true;
        }
        let retry_budget = if rule == "context_length_exceeded" {
            let budget = self.config.context_window_tokens.map_or_else(
                || {
                    calibrated_request_tokens(&outgoing, calibration_factor, baseline.as_ref())
                        .saturating_mul(70)
                        / 100
                },
                |window| {
                    context_budget(
                        &outgoing,
                        window,
                        TrimOptions::OVERFLOW_RETRY.budget_scale,
                        estimate_source,
                    )
                },
            );
            let report = trim_request_to_budget_with_baseline(
                &mut repaired,
                budget,
                calibration_factor,
                TrimOptions::OVERFLOW_RETRY.force_drop_oldest_round,
                baseline.as_ref(),
            )?
            .ok_or_else(|| {
                context_budget_error_with_baseline(
                    &repaired,
                    budget,
                    calibration_factor,
                    baseline.as_ref(),
                )
            })?;
            Self::emit_context_trimmed(event_tx, &report).await?;
            Some(budget)
        } else {
            None
        };
        let repaired_estimate = estimate_request_tokens(&repaired);
        let repaired_result = self.send_once(&repaired, cancel, event_tx).await;
        self.observe_token_usage(&repaired, request_head, repaired_estimate, &repaired_result);
        if let (Some(budget), Err(error)) = (retry_budget, &repaired_result)
            && ClientError::from_anyhow(error).is_some_and(Self::is_context_overflow_error)
        {
            return Err(context_budget_error_with_baseline(
                &repaired,
                budget,
                calibration_factor,
                baseline.as_ref(),
            )
            .into());
        }
        repaired_result
    }

    async fn emit_context_trimmed(
        event_tx: &mpsc::Sender<SessionEvent>,
        report: &ContextTrimReport,
    ) -> Result<()> {
        log::warn!(
            "[client:llm][context_trimmed] dropped_rounds={} truncated_messages={} before={} after={} budget={} estimate_source={}",
            report.dropped_rounds,
            report.truncated_messages,
            report.before,
            report.after,
            report.budget,
            report.estimate_source.as_str()
        );
        event_tx
            .send(SessionEvent::ContextTrimmed {
                dropped_rounds: report.dropped_rounds,
                truncated_messages: report.truncated_messages,
                before: report.before,
                after: report.after,
                suggest_compaction: report.suggest_compaction,
                estimate_source: report.estimate_source.as_str().to_string(),
            })
            .await?;
        Ok(())
    }

    fn observe_token_usage(
        &mut self,
        request: &ChatRequest,
        request_head: Option<u64>,
        base_estimate: u64,
        result: &Result<TurnOutput>,
    ) {
        let Ok((_, _, _, _, _, Some(usage))) = result else {
            return;
        };
        if let Some(factor) = self
            .token_calibrator
            .observe(usage.prompt_tokens, base_estimate)
        {
            log::info!(
                "[client:llm][token_calibrated] turn_id={} prompt_tokens={} base_estimate={} factor={:.4}",
                self.turn_id,
                usage.prompt_tokens,
                base_estimate,
                factor
            );
        }
        if let Some(head_node_id) = request_head
            && let Some(baseline) = RequestBaseline::new(usage.prompt_tokens, request, head_node_id)
        {
            self.last_baseline = Some(baseline);
        }
    }

    async fn send_once(
        &mut self,
        req: &ChatRequest,
        cancel: &mut TurnCancel,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<TurnOutput> {
        if cancel.is_cancelled() {
            return Ok(cancelled_turn_output(
                String::new(),
                String::new(),
                Vec::new(),
                None,
            ));
        }

        if req.stream.unwrap_or(false) {
            self.handle_stream(req, cancel, event_tx).await
        } else {
            self.handle_non_stream(req, cancel, event_tx).await
        }
    }

    /// 对供应商明确指出的安全问题做一次确定性修复；返回 None 时不得重试。
    pub(super) fn repair_after_bad_request(
        req: &ChatRequest,
        error: &ClientError,
    ) -> Option<(ChatRequest, &'static str)> {
        if error.code != ErrorCode::HttpBadRequest {
            return None;
        }
        let provider_message = error
            .detail
            .get("provider_message")
            .and_then(Value::as_str)?
            .to_ascii_lowercase();
        let mut repaired = req.clone();

        if Self::is_context_overflow_message(&provider_message) {
            return Some((repaired, "context_length_exceeded"));
        }

        if provider_message.contains("reasoning_content")
            && ["not allowed", "unsupported", "unknown", "extra inputs"]
                .iter()
                .any(|marker| provider_message.contains(marker))
        {
            if Self::strip_reasoning_content(&mut repaired.messages) {
                return Some((repaired, "unsupported_reasoning_content"));
            }
        }

        if provider_message.contains("invalid assistant message")
            || provider_message.contains("content or tool_calls must be set")
        {
            let previous_len = repaired.messages.len();
            repaired.messages = Self::sanitize_messages(repaired.messages);
            if repaired.messages.len() != previous_len {
                return Some((repaired, "invalid_assistant_message"));
            }
        }

        if provider_message.contains("tools")
            && ["empty", "at least one", "must contain"]
                .iter()
                .any(|marker| provider_message.contains(marker))
            && repaired.tools.as_ref().is_some_and(Vec::is_empty)
        {
            repaired.tools = None;
            repaired.tool_choice = None;
            return Some((repaired, "empty_tools"));
        }

        None
    }

    fn is_context_overflow_error(error: &ClientError) -> bool {
        error.code == ErrorCode::HttpBadRequest
            && error
                .detail
                .get("provider_message")
                .and_then(Value::as_str)
                .is_some_and(Self::is_context_overflow_message)
    }

    fn is_context_overflow_message(message: &str) -> bool {
        let message = message.to_ascii_lowercase();
        [
            "context_length_exceeded",
            "maximum context length",
            "prompt is too long",
            "input length and max_tokens exceed context limit",
            "exceeds the context window",
            "too many tokens",
            "maximum number of tokens",
        ]
        .iter()
        .any(|marker| message.contains(marker))
    }

    async fn handle_non_stream(
        &mut self,
        req: &ChatRequest,
        cancel: &mut TurnCancel,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<TurnOutput> {
        log::info!(
            "[client:llm][non_stream_prepare_start] turn_id={} messages={} tool_count={}",
            self.turn_id,
            req.messages.len(),
            req.tools.as_ref().map_or(0, Vec::len)
        );
        let stage_started = Instant::now();
        if cancel.is_cancelled() {
            return Ok(cancelled_turn_output(
                String::new(),
                String::new(),
                Vec::new(),
                None,
            ));
        }
        let body = self.prepare_request(req)?;
        if cancel.is_cancelled() {
            return Ok(cancelled_turn_output(
                String::new(),
                String::new(),
                Vec::new(),
                None,
            ));
        }
        log::info!(
            "[client:llm][non_stream_prepare_done] turn_id={} elapsed_ms={} body_bytes={}",
            self.turn_id,
            stage_started.elapsed().as_millis(),
            body.len()
        );

        let raw_line = {
            log::info!(
                "[client:llm][non_stream_http_send_start] turn_id={} base_url={}",
                self.turn_id,
                self.config.base_url
            );
            let stage_started = Instant::now();
            let post_fut =
                self.client
                    .post_collect(&self.config.base_url, self.config.api_key.expose(), body);
            let raw_body = tokio::select! {
                result = post_fut => result?,
                _ = cancel.cancelled() => {
                    return Ok(cancelled_turn_output(
                        String::new(),
                        String::new(),
                        Vec::new(),
                        None,
                    ));
                }
            };
            log::info!(
                "[client:llm][non_stream_http_send_done] turn_id={} elapsed_ms={}",
                self.turn_id,
                stage_started.elapsed().as_millis()
            );
            if raw_body.is_empty() {
                return Err(ClientError::new(ErrorCode::LlmResponseEmpty, "LLM 响应为空").into());
            }
            raw_body
        };
        log::info!(
            "[client:llm][non_stream_first_line_done] turn_id={} bytes={}",
            self.turn_id,
            raw_line.len()
        );

        if cancel.is_cancelled() {
            return Ok(cancelled_turn_output(
                String::new(),
                String::new(),
                Vec::new(),
                None,
            ));
        }
        let normalized = self.normalize_response(&raw_line)?;

        let res: ChatResponse = serde_json::from_str(&normalized).map_err(|e| {
            ClientError::new(ErrorCode::LlmResponseParseError, "LLM 响应 JSON 解析失败")
                .with_kv("source", e.to_string())
        })?;
        let choice = res.choices.first().ok_or_else(|| {
            ClientError::new(ErrorCode::LlmResponseParseError, "LLM 响应 choices 为空")
        })?;

        let reasoning = choice.message.reasoning_content.clone().unwrap_or_default();
        let content = choice.message.content.clone().unwrap_or_default();
        let finish_reason = choice.finish_reason.clone();

        if !reasoning.is_empty() {
            event_tx
                .send(SessionEvent::ReasoningDelta(reasoning.clone()))
                .await?;
        }
        if !content.is_empty() {
            event_tx
                .send(SessionEvent::ContentDelta(content.clone()))
                .await?;
        }

        let tool_calls_vec = choice.message.tool_calls.clone().unwrap_or_default();
        let tool_calls = if tool_calls_vec.is_empty() {
            None
        } else {
            for call in &tool_calls_vec {
                event_tx
                    .send(SessionEvent::ToolCall {
                        index: call.index,
                        name: call.function.name.clone(),
                        arguments: call.function.arguments.clone(),
                    })
                    .await?;
            }
            Some(tool_calls_vec)
        };

        Ok((
            content,
            reasoning,
            tool_calls,
            Some(finish_reason),
            TurnStatus::Ok,
            Some(res.usage),
        ))
    }

    async fn handle_stream(
        &mut self,
        req: &ChatRequest,
        cancel: &mut TurnCancel,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<TurnOutput> {
        // StreamDecoder 和 ToolCallAccumulator 降为方法局部变量
        let mut decoder = StreamDecoder::default();
        decoder.begin_turn(self.turn_id);
        let mut acc = ToolCallAccumulator::default();

        log::info!(
            "[client:llm][stream_prepare_start] turn_id={} messages={} tool_count={}",
            self.turn_id,
            req.messages.len(),
            req.tools.as_ref().map_or(0, Vec::len)
        );
        let stage_started = Instant::now();
        if cancel.is_cancelled() {
            return Ok(cancelled_turn_output(
                String::new(),
                String::new(),
                Vec::new(),
                None,
            ));
        }
        let body = self.prepare_request(req)?;
        if cancel.is_cancelled() {
            return Ok(cancelled_turn_output(
                String::new(),
                String::new(),
                Vec::new(),
                None,
            ));
        }
        log::info!(
            "[client:llm][stream_prepare_done] turn_id={} elapsed_ms={} body_bytes={}",
            self.turn_id,
            stage_started.elapsed().as_millis(),
            body.len()
        );

        log::info!(
            "[client:llm][stream_http_send_start] turn_id={} base_url={}",
            self.turn_id,
            self.config.base_url
        );
        let stage_started = Instant::now();
        let post_fut =
            self.client
                .post_json(&self.config.base_url, self.config.api_key.expose(), body);
        let stream = tokio::select! {
            result = post_fut => result?,
            _ = cancel.cancelled() => {
                return Ok(cancelled_turn_output(
                    String::new(),
                    String::new(),
                    Vec::new(),
                    None,
                ));
            }
        };
        log::info!(
            "[client:llm][stream_http_send_done] turn_id={} elapsed_ms={}",
            self.turn_id,
            stage_started.elapsed().as_millis()
        );
        tokio::pin!(stream);

        let mut full_content = String::new();
        let mut full_reasoning = String::new();
        let mut finish_reason: Option<String> = None;
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut turn_status = TurnStatus::Ok;
        let mut usage: Option<Usage> = None;
        let mut line_count = 0usize;
        let mut saw_tool_call_start = false;

        'outer: loop {
            let raw_line = tokio::select! {
                _ = cancel.cancelled() => {
                    turn_status = TurnStatus::Cancelled;
                    finish_reason = Some("cancelled".to_string());
                    usage = decoder.take_pending_usage();
                    break 'outer;
                }
                raw_line = stream.next() => {
                    match raw_line {
                        Some(raw_line) => raw_line,
                        None => break 'outer,
                    }
                }
            };
            let line = match raw_line {
                Ok(line) => line,
                Err(error) if !full_content.is_empty() || !full_reasoning.is_empty() => {
                    let error = ClientError::from_anyhow_owned(error);
                    log::warn!(
                        "[client:llm][stream_read_interrupted] turn_id={} content_chars={} reasoning_chars={} error={}",
                        self.turn_id,
                        full_content.chars().count(),
                        full_reasoning.chars().count(),
                        error
                    );
                    turn_status = TurnStatus::Error(error);
                    finish_reason = Some("interrupted".to_string());
                    usage = decoder.take_pending_usage();
                    break 'outer;
                }
                Err(error) => return Err(error),
            };
            if cancel.is_cancelled() {
                turn_status = TurnStatus::Cancelled;
                finish_reason = Some("cancelled".to_string());
                usage = decoder.take_pending_usage();
                break 'outer;
            }
            line_count += 1;
            if line_count == 1 || line_count.is_multiple_of(50) {
                log::info!(
                    "[client:llm][stream_line_received] turn_id={} line_count={} bytes={}",
                    self.turn_id,
                    line_count,
                    line.len()
                );
            }
            if line.is_empty() {
                continue;
            }

            // acquire → map → release，每行独立借出，不跨 await
            let normalized = match self.normalize_stream_line(&line) {
                Ok(normalized) => normalized,
                Err(error) if !full_content.is_empty() || !full_reasoning.is_empty() => {
                    let error = ClientError::from_anyhow_owned(error);
                    log::warn!(
                        "[client:llm][stream_map_interrupted] turn_id={} content_chars={} reasoning_chars={} error={}",
                        self.turn_id,
                        full_content.chars().count(),
                        full_reasoning.chars().count(),
                        error
                    );
                    turn_status = TurnStatus::Error(error);
                    finish_reason = Some("interrupted".to_string());
                    usage = decoder.take_pending_usage();
                    break 'outer;
                }
                Err(error) => return Err(error),
            };

            let events = decoder.decode(&normalized);

            for ev in events {
                let ev = match ev {
                    Ok(event) => event,
                    Err(error) if !full_content.is_empty() || !full_reasoning.is_empty() => {
                        let error = ClientError::from_anyhow_owned(error);
                        log::warn!(
                            "[client:llm][stream_decode_interrupted] turn_id={} content_chars={} reasoning_chars={} error={}",
                            self.turn_id,
                            full_content.chars().count(),
                            full_reasoning.chars().count(),
                            error
                        );
                        turn_status = TurnStatus::Error(error);
                        finish_reason = Some("interrupted".to_string());
                        usage = decoder.take_pending_usage();
                        break 'outer;
                    }
                    Err(error) => return Err(error),
                };

                match ev.payload {
                    DecoderEventPayload::AssistantReasoningDelta { delta } => {
                        full_reasoning.push_str(&delta);
                        event_tx.send(SessionEvent::ReasoningDelta(delta)).await?;
                    }

                    DecoderEventPayload::AssistantContentDelta { delta } => {
                        full_content.push_str(&delta);
                        event_tx.send(SessionEvent::ContentDelta(delta)).await?;
                    }

                    DecoderEventPayload::ToolCallStart { index, tool_name } => {
                        saw_tool_call_start = true;
                        acc.on_start(index, Some(&tool_name));
                        event_tx
                            .send(SessionEvent::ToolCall {
                                index,
                                name: tool_name,
                                arguments: String::new(),
                            })
                            .await?;
                    }

                    DecoderEventPayload::ToolCallDelta {
                        index,
                        tool_name,
                        args,
                    } => {
                        acc.on_delta(index, tool_name.as_deref(), &args);
                    }

                    DecoderEventPayload::ToolCallsRequired => {
                        // 取出可能已被暂存的 usage（部分 API 在 tool_calls 之前发送 usage chunk）
                        usage = decoder.take_pending_usage();
                        tool_calls = acc.build_calls(self.turn_id);
                        for call in &tool_calls {
                            event_tx
                                .send(SessionEvent::ToolCall {
                                    index: call.index,
                                    name: call.function.name.clone(),
                                    arguments: call.function.arguments.clone(),
                                })
                                .await?;
                        }
                        finish_reason = Some("tool_calls".to_string());
                        break 'outer;
                    }

                    DecoderEventPayload::TurnEnd {
                        status,
                        finish_reason: stream_finish_reason,
                        usage: u,
                    } => {
                        turn_status = status.clone();
                        if u.is_some() {
                            usage = u;
                        }
                        let normalized_finish_reason = match &turn_status {
                            TurnStatus::Ok => stream_finish_reason
                                .clone()
                                .unwrap_or_else(|| "stop".to_string()),
                            TurnStatus::Cancelled => "cancelled".to_string(),
                            TurnStatus::Interrupted => "interrupted".to_string(),
                            TurnStatus::Error(error)
                                if !full_content.is_empty() || !full_reasoning.is_empty() =>
                            {
                                turn_status = TurnStatus::Error(error.clone());
                                finish_reason = Some("interrupted".to_string());
                                break 'outer;
                            }
                            TurnStatus::Error(error) => return Err(error.clone().into()),
                        };
                        // [DONE] 常被解码为 stop；不能覆盖此前明确的 length 等结束原因。
                        if finish_reason.is_none() || normalized_finish_reason != "stop" {
                            finish_reason = Some(normalized_finish_reason.clone());
                        }
                        log::info!(
                            "[client:llm][stream_turn_end_event] turn_id={} status={} finish_reason={} saw_tool_call_start={} content_chars={} reasoning_chars={}",
                            self.turn_id,
                            match &turn_status {
                                TurnStatus::Ok => "ok",
                                TurnStatus::Cancelled => "cancelled",
                                TurnStatus::Interrupted => "interrupted",
                                TurnStatus::Error(_) => "error",
                            },
                            normalized_finish_reason,
                            saw_tool_call_start,
                            full_content.chars().count(),
                            full_reasoning.chars().count()
                        );

                        if saw_tool_call_start && normalized_finish_reason != "tool_calls" {
                            let status = TurnStatus::Error(
                                ClientError::new(
                                    ErrorCode::LlmStreamProtocolError,
                                    "模型开始输出工具调用，但未以 tool_calls 结束，工具未执行",
                                )
                                .with_kv("finish_reason", normalized_finish_reason.clone())
                                .with_kv("turn_id", self.turn_id)
                                .with_kv("content_chars", full_content.chars().count() as u64)
                                .with_kv("reasoning_chars", full_reasoning.chars().count() as u64),
                            );
                            log::warn!(
                                "[client:llm][incomplete_tool_call_stream] turn_id={} finish_reason={} content_chars={} reasoning_chars={}",
                                self.turn_id,
                                normalized_finish_reason,
                                full_content.chars().count(),
                                full_reasoning.chars().count()
                            );
                            turn_status = status;
                            break 'outer;
                        }

                        // Qwen 的 OpenAI 兼容流式响应会先发送 finish_reason=stop，
                        // 再发送 choices=[] 的 usage-only chunk，最后发送 [DONE]。
                        // 普通完成且尚未拿到 usage 时继续读取尾部块，避免用量统计丢失。
                        if Self::should_stop_after_stream_turn_end(&turn_status, &usage) {
                            break 'outer;
                        }
                    }

                    _ => {}
                }
            }
        }

        if finish_reason.is_none() {
            // 部分 API（如 DeepSeek v4 代理）不在流式 chunk 中携带
            // finish_reason，而是仅以 [DONE] 或 TCP 关闭表示结束。
            // 此时视为正常结束。
            finish_reason = Some("stop".to_string());
            if !matches!(turn_status, TurnStatus::Error(_)) {
                turn_status = TurnStatus::Ok;
            }
            // 尝试取出可能已被暂存在 decoder 中的 usage
            if usage.is_none() {
                usage = decoder.take_pending_usage();
            }
        }

        Ok((
            full_content,
            full_reasoning,
            if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            finish_reason,
            turn_status,
            usage,
        ))
    }

    pub(super) fn should_stop_after_stream_turn_end(
        turn_status: &TurnStatus,
        usage: &Option<Usage>,
    ) -> bool {
        usage.is_some() || !matches!(turn_status, TurnStatus::Ok)
    }
}
