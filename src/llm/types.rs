use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}
impl Message {
    #[allow(dead_code)]
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: Some(content.into()),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    #[allow(dead_code)]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: Some(content.into()),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    #[allow(dead_code)]
    pub fn assistant(
        content: Option<impl Into<String>>,
        reasoning: Option<impl Into<String>>,
        tool_calls: Option<Vec<ToolCall>>,
    ) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.map(|v| v.into()),
            reasoning_content: reasoning.map(|v| v.into()),
            tool_call_id: None,
            tool_calls,
        }
    }

    #[allow(dead_code)]
    pub fn tool(content: impl Into<String>, tool_call_id: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(content.into()),
            reasoning_content: None,
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct ThinkingType {
    #[serde(rename = "type")]
    pub thinking_type: String,
}

impl ThinkingType {
    pub fn enabled() -> ThinkingType {
        ThinkingType {
            thinking_type: "enabled".to_string(),
        }
    }

    pub fn disabled() -> ThinkingType {
        ThinkingType {
            thinking_type: "disabled".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub messages: Vec<Message>,

    pub model: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingType>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<Value>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<i32>,
}

impl Default for ChatRequest {
    fn default() -> Self {
        Self {
            messages: vec![],
            model: "".to_string(),
            thinking: None,
            frequency_penalty: None,
            max_tokens: None,
            presence_penalty: None,
            response_format: None,
            stop: None,
            stream: None,
            stream_options: None,
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: Some("auto".to_string()),
            logprobs: None,
            top_logprobs: None,
            n: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct ChatResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct ChatResponseStream {
    pub id: String,
    pub object: String,
    pub choices: Vec<ChoiceStream>,
    pub created: i64,
    pub model: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Choice {
    pub index: i64,
    pub message: Message,
    pub finish_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct ChoiceStream {
    pub index: i64,
    pub delta: Delta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Delta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Usage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

/// ---- tool call 结构（用于 a / stream delta 累积） ----

#[allow(dead_code)]
pub struct ToolFunctionArg {
    pub name: String,
    pub r#type: String,
    pub required: Option<bool>,
    pub description: Option<String>,
    pub default: Option<Value>,
    pub max: Option<Value>,
    pub min: Option<Value>,
}

impl ToolFunctionArg {
    #[allow(dead_code)]
    pub fn new(name: impl Into<String>, r#type: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            r#type: r#type.into(),
            required: Some(true),
            description: None,
            default: None,
            max: None,
            min: None,
        }
    }

    #[allow(dead_code)]
    pub fn required(mut self, required: bool) -> Self {
        self.required = Some(required);
        self
    }

    #[allow(dead_code)]
    pub fn desc(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    #[allow(dead_code)]
    pub fn default<V: Into<Value>>(mut self, default: V) -> Self {
        self.default = Some(default.into());
        self
    }

    #[allow(dead_code)]
    pub fn max<V: Into<Value>>(mut self, max: V) -> Self {
        self.max = Some(max.into());
        self
    }

    #[allow(dead_code)]
    pub fn min<V: Into<Value>>(mut self, min: V) -> Self {
        self.min = Some(min.into());
        self
    }

    #[allow(dead_code)]
    pub fn schema(&self) -> Value {
        let mut v = serde_json::json!({"type": self.r#type});

        if let Some(vv) = &self.default { v["default"] = vv.clone(); }
        if let Some(vv) = &self.max { v["maximum"] = vv.clone(); }
        if let Some(vv) = &self.min { v["minimum"] = vv.clone(); }
        v
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(dead_code)]
pub struct ToolFunctionCall {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(dead_code)]
pub struct ToolCall {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "type", default)]
    pub call_type: Option<String>,
    #[serde(default)]
    pub function: ToolFunctionCall,
    #[serde(default)]
    pub index: usize,
}

#[allow(dead_code)]
pub struct EventInfo {
    pub time_stamp: std::time::SystemTime,
    pub seq: u64,
    pub turn_id: u64,
}

/// 数据流事件
///
/// event_info: EventInfo 事件信息，
///
/// payload: DSEventPayload 事件负载，包含事件的具体内容。
#[allow(dead_code)]
pub struct DecoderEvent {
    pub event_info: EventInfo,
    pub payload: DecoderEventPayload,
}

/// 轮次结束状态
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum TurnStatus {
    Ok,
    Cancelled,
    Interrupted,
    Error(String),
}

#[allow(dead_code)]
pub enum DecoderEventPayload {
    TurnStart {
        model: String,
    },
    AssistantContentDelta {
        delta: String,
    },
    AssistantReasoningDelta {
        delta: String,
    },
    ToolCallStart {
        index: usize,
        tool_name: String,
    },
    ToolCallDelta {
        index: usize,
        tool_name: Option<String>,
        args: String,
    },
    ToolCallsRequired,
    TurnEnd {
        status: TurnStatus,
        usage: Option<Usage>,
    },
}

/// 会话控制指令（通过 SessionHandle 发往 drive loop）
#[derive(Debug)]
pub(crate) enum CtrlMsg {
    /// 切换到另一个插件（下一轮生效）
    SwitchPlugin { plugin_id: String, api_key: String },
    /// 将消息树 head 移动到指定节点（重说 / 分支 / 历史回退）
    Checkout { node_id: u64 },
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum SessionEvent {
    NeedInput,
    TurnBegin {
        turn_id: u64,
        /// 本轮开始时的 head 节点 ID（用户消息节点，或工具结果节点）
        /// 树为空时为 0（实践中不会发生）
        node_id: u64,
    },
    ReasoningDelta(String),
    ContentDelta(String),

    ToolCall {
        index: usize,
        name: String,
    },
    ToolResult {
        index: usize,
        output: String,
        is_error: bool,
    },

    TurnEnd {
        status: TurnStatus,
        /// 本轮助手消息节点 ID（供前端做历史导航 / 分支 / 重说）
        node_id: u64,
    },
    Error(String),
}