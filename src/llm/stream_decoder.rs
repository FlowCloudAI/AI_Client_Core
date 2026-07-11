use crate::error::{ClientError, ErrorCode};
use crate::llm::types::{
    ChatResponseStream, DecoderEvent, DecoderEventPayload, EventInfo, ToolCall, TurnStatus, Usage,
};
use std::collections::{HashMap, HashSet};

#[derive(Default, Debug)]
pub struct StreamDecoder {
    seq: u64,
    turn_id: u64,
    started: HashSet<(usize, usize)>,
    content_snapshots: HashMap<usize, String>,
    reasoning_snapshots: HashMap<usize, String>,
    /// 部分 API（如 Qwen）将 usage 放在独立的 chunk（choices 为空）中。
    /// 该 chunk 可能在 finish_reason chunk 之后到达，这里暂存以便 TurnEnd 使用。
    pending_usage: Option<Usage>,
}

impl StreamDecoder {
    pub fn begin_turn(&mut self, turn_id: u64) {
        self.turn_id = turn_id;
        self.seq = 0;
        self.started.clear();
        self.content_snapshots.clear();
        self.reasoning_snapshots.clear();
        self.pending_usage = None;
    }

    /// 取出已暂存的 usage（用于非 TurnEnd 的退场路径，如 tool_calls / 取消 / 流异常关闭）。
    pub fn take_pending_usage(&mut self) -> Option<Usage> {
        self.pending_usage.take()
    }

    fn next_info(&mut self) -> EventInfo {
        self.seq += 1;
        EventInfo {
            time_stamp: std::time::SystemTime::now(),
            seq: self.seq,
            turn_id: self.turn_id,
        }
    }

    /// 处理流式响应
    pub fn decode(&mut self, line: &str) -> Vec<anyhow::Result<DecoderEvent>> {
        let mut out = Vec::new();
        let mut s = line.trim();

        if let Some(rest) = s.strip_prefix("data:") {
            s = rest.trim();
        }
        if s.is_empty() {
            return out;
        }
        if s == "[DONE]" {
            out.push(Ok(DecoderEvent {
                event_info: self.next_info(),
                payload: DecoderEventPayload::TurnEnd {
                    status: TurnStatus::Ok,
                    finish_reason: None,
                    usage: self.pending_usage.take(),
                },
            }));
            return out;
        }

        let resp: ChatResponseStream = match serde_json::from_str(s) {
            Ok(v) => v,
            Err(e) => {
                out.push(Err(ClientError::new(
                    ErrorCode::LlmStreamProtocolError,
                    "流式响应 JSON 解析失败",
                )
                .with_kv("source", e.to_string())
                .with_kv("line", s.to_string())
                .into()));
                return out;
            }
        };

        // 部分 API 将 usage 放在独立的 chunk 中（choices 为空），
        // 该 chunk 会在 finish_reason chunk 之前到达，需要暂存。
        if resp.usage.is_some() {
            self.pending_usage = resp.usage.clone();
        }

        for (choice_i, choice) in resp.choices.into_iter().enumerate() {
            // content delta / reasoning delta 你照旧发给 Session -> UI
            if let Some(delta) = choice.delta.content {
                if !delta.is_empty() {
                    out.push(Ok(DecoderEvent {
                        event_info: self.next_info(),
                        payload: DecoderEventPayload::AssistantContentDelta { delta },
                    }));
                }
            }
            if let Some(snapshot) = choice.delta.content_snapshot {
                let delta = snapshot_delta(&mut self.content_snapshots, choice_i, snapshot);
                if !delta.is_empty() {
                    out.push(Ok(DecoderEvent {
                        event_info: self.next_info(),
                        payload: DecoderEventPayload::AssistantContentDelta { delta },
                    }));
                }
            }
            if let Some(delta) = choice.delta.reasoning_content {
                if !delta.is_empty() {
                    out.push(Ok(DecoderEvent {
                        event_info: self.next_info(),
                        payload: DecoderEventPayload::AssistantReasoningDelta { delta },
                    }));
                }
            }
            if let Some(snapshot) = choice.delta.reasoning_content_snapshot {
                let delta = snapshot_delta(&mut self.reasoning_snapshots, choice_i, snapshot);
                if !delta.is_empty() {
                    out.push(Ok(DecoderEvent {
                        event_info: self.next_info(),
                        payload: DecoderEventPayload::AssistantReasoningDelta { delta },
                    }));
                }
            }

            if let Some(tool_calls) = choice.delta.tool_calls {
                for tc in tool_calls {
                    // log::debug!("{}", line);
                    self.emit_tool_call_events(choice_i, tc, &mut out);
                }
            }

            // 关键：当模型声明要调用工具时，flush 出 ToolCallStart
            if let Some(fr) = choice.finish_reason.as_deref() {
                log::info!(
                    "[client:stream_decoder][finish_reason] turn_id={} choice={} finish_reason={} pending_tool_starts={}",
                    self.turn_id,
                    choice_i,
                    fr,
                    self.started.len()
                );
                if fr == "tool_calls" {
                    out.push(Ok(DecoderEvent {
                        event_info: self.next_info(),
                        payload: DecoderEventPayload::ToolCallsRequired,
                    }));
                    continue;
                }

                // 非 tool_calls 才表示 turn 真结束
                out.push(Ok(DecoderEvent {
                    event_info: self.next_info(),
                    payload: DecoderEventPayload::TurnEnd {
                        status: TurnStatus::Ok,
                        finish_reason: Some(fr.to_string()),
                        usage: self.pending_usage.take(),
                    },
                }));
            }
        }

        out
    }

    fn emit_tool_call_events(
        &mut self,
        choice_i: usize,
        tc: ToolCall,
        out: &mut Vec<anyhow::Result<DecoderEvent>>,
    ) {
        let index = tc.index;

        // name 首次出现 -> ToolCallStart
        if !tc.function.name.is_empty() && !self.started.contains(&(choice_i, index)) {
            self.started.insert((choice_i, index));
            out.push(Ok(DecoderEvent {
                event_info: self.next_info(),
                payload: DecoderEventPayload::ToolCallStart {
                    index,
                    tool_name: tc.function.name.clone(),
                },
            }));
        }

        // args delta → ToolCallDelta（参数增量 → 工具调用增量）
        if !tc.function.arguments.is_empty() {
            out.push(Ok(DecoderEvent {
                event_info: self.next_info(),
                payload: DecoderEventPayload::ToolCallDelta {
                    index,
                    tool_name: if tc.function.name.is_empty() {
                        None
                    } else {
                        Some(tc.function.name)
                    },
                    args: tc.function.arguments,
                },
            }));
        }
    }
}

fn snapshot_delta(
    snapshots: &mut HashMap<usize, String>,
    choice_index: usize,
    snapshot: String,
) -> String {
    let delta = snapshots
        .get(&choice_index)
        .and_then(|previous| snapshot.strip_prefix(previous))
        .unwrap_or(&snapshot)
        .to_string();
    snapshots.insert(choice_index, snapshot);
    delta
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_chunk(content: &str, reasoning: &str) -> String {
        serde_json::json!({
            "id": "chunk-1",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "snapshot-model",
            "choices": [{
                "index": 0,
                "delta": {
                    "content_snapshot": content,
                    "reasoning_content_snapshot": reasoning
                },
                "finish_reason": null
            }]
        })
        .to_string()
    }

    #[test]
    fn cumulative_snapshots_are_emitted_as_deltas_and_reset_each_turn() {
        let mut decoder = StreamDecoder::default();
        decoder.begin_turn(1);

        let first = decoder.decode(&snapshot_chunk("你", "想"));
        let second = decoder.decode(&snapshot_chunk("你好", "想好了"));

        assert!(matches!(
            &first[0].as_ref().unwrap().payload,
            DecoderEventPayload::AssistantContentDelta { delta } if delta == "你"
        ));
        assert!(matches!(
            &first[1].as_ref().unwrap().payload,
            DecoderEventPayload::AssistantReasoningDelta { delta } if delta == "想"
        ));
        assert!(matches!(
            &second[0].as_ref().unwrap().payload,
            DecoderEventPayload::AssistantContentDelta { delta } if delta == "好"
        ));
        assert!(matches!(
            &second[1].as_ref().unwrap().payload,
            DecoderEventPayload::AssistantReasoningDelta { delta } if delta == "好了"
        ));

        decoder.begin_turn(2);
        let reset = decoder.decode(&snapshot_chunk("你好", "想好了"));
        assert!(matches!(
            &reset[0].as_ref().unwrap().payload,
            DecoderEventPayload::AssistantContentDelta { delta } if delta == "你好"
        ));
    }
}
