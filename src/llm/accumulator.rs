use std::collections::{HashMap, HashSet};
use serde_json::Value;
use crate::llm::types::{ToolCall, ToolFunctionCall};

// ═════════════════════════════════════════════════════════════
//                    工具调用累积器
// ═════════════════════════════════════════════════════════════

/// 流式 tool_call 增量累积器
///
/// 模型通过多条 SSE delta 分批发送 tool_call 数据：
/// - `ToolCallStart`: 首次出现，携带 function name
/// - `ToolCallDelta`: 逐段拼接 arguments JSON
/// - `ToolCallsRequired`: 收口信号，调用 `build_calls` 组装最终结果
///
/// # 示例
/// ```ignore
/// let mut acc = ToolCallAccumulator::default();
/// acc.on_start(0, Some("my_function"));
/// acc.on_delta(0, Some("my_function"), r#"{"key": "#);
/// acc.on_delta(0, Some("my_function"), r#""value"}"#);
/// let calls = acc.build_calls(1);
/// ```
#[derive(Default)]
pub struct ToolCallAccumulator {
    /// tool_call 索引 -> 函数名
    names: HashMap<usize, String>,
    /// tool_call 索引 -> 累积的 JSON arguments
    args: HashMap<usize, String>,
    /// tool_call 的处理顺序
    order: Vec<usize>,
    /// 已处理的索引集合（用于去重）
    seen: HashSet<usize>,
}

impl ToolCallAccumulator {
    /// 标记索引已处理，初始化或保留 args 条目
    fn touch(&mut self, index: usize) {
        if self.seen.insert(index) {
            self.order.push(index);
        }
        self.args.entry(index).or_default();
    }

    /// 处理 tool_call 开始事件
    pub(crate) fn on_start(&mut self, index: usize, name: Option<&str>) {
        self.touch(index);
        if let Some(name) = name.filter(|s| !s.is_empty()) {
            self.names.insert(index, name.to_string());
        }
    }

    /// 处理 tool_call delta 事件（累积 arguments）
    pub(crate) fn on_delta(&mut self, index: usize, name: Option<&str>, args_delta: &str) {
        self.touch(index);
        if let Some(name) = name.filter(|s| !s.is_empty()) {
            self.names.insert(index, name.to_string());
        }
        if !args_delta.is_empty() {
            self.args.get_mut(&index).unwrap().push_str(args_delta);
        }
    }

    /// 组装最终的 ToolCall 列表
    pub(crate) fn build_calls(&mut self, turn_id: u64) -> Vec<ToolCall> {
        let order = std::mem::take(&mut self.order);
        order
            .into_iter()
            .filter_map(|index| {
                let name = self.names.get(&index).cloned().unwrap_or_default();
                if name.is_empty() {
                    return None;
                }

                let mut args = self.args.get(&index).cloned().unwrap_or_default();
                if args.trim().is_empty() || serde_json::from_str::<Value>(&args).is_err() {
                    args = "{}".to_string();
                }

                Some(ToolCall {
                    id: Some(Self::synth_tool_call_id(turn_id, index)),
                    call_type: Some("function".to_string()),
                    function: ToolFunctionCall {
                        name,
                        arguments: args,
                    },
                    index,
                })
            })
            .collect()
    }

    /// 合成 tool_call ID
    #[inline]
    fn synth_tool_call_id(turn_id: u64, index: usize) -> String {
        format!("t{}:idx:{}", turn_id, index)
    }
}