use std::collections::HashSet;
use crate::llm::types::{ChatResponseStream, DecoderEvent, DecoderEventPayload, EventInfo, ToolCall, TurnStatus};

#[derive(Default, Debug)]
pub struct StreamDecoder {
    seq: u64,
    turn_id: u64,
    started: HashSet<(usize, usize)>,
}

impl StreamDecoder {
    pub fn begin_turn(&mut self, turn_id: u64) {
        self.turn_id = turn_id;
        self.seq = 0;
        self.started.clear();
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
        if s.is_empty() || s == "[DONE]" {
            return out;
        }

        let resp: ChatResponseStream = match serde_json::from_str(s) {
            Ok(v) => v,
            Err(e) => {
                out.push(Err(anyhow::anyhow!("[decoder] 解析 JSON 失败: {e};\nline={s}")));
                return out;
            }
        };

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
            if let Some(delta) = choice.delta.reasoning_content {
                if !delta.is_empty() {
                    out.push(Ok(DecoderEvent {
                        event_info: self.next_info(),
                        payload: DecoderEventPayload::AssistantReasoningDelta { delta },
                    }));
                }
            }

            if let Some(tool_calls) = choice.delta.tool_calls {
                for tc in tool_calls {
                    //println!("{}", line);
                    self.emit_tool_call_events(choice_i, tc, &mut out);
                }
            }

            // 关键：当模型声明要调用工具时，flush 出 ToolCallStart
            if let Some(fr) = choice.finish_reason.as_deref() {
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
                        usage: resp.usage.clone(),
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

        // args delta -> ToolCallDelta
        if !tc.function.arguments.is_empty() {
            out.push(Ok(DecoderEvent {
                event_info: self.next_info(),
                payload: DecoderEventPayload::ToolCallDelta {
                    index,
                    tool_name: if tc.function.name.is_empty() { None } else { Some(tc.function.name) },
                    args: tc.function.arguments,
                },
            }));
        }
    }
}
