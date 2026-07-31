use crate::error::ClientError;
use crate::llm::config::SecretString;
use crate::plugin::types::ThinkingEffort;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: Some(content.into()),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: Some(content.into()),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

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
    pub thinking_effort: Option<ThinkingEffort>,

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
            thinking_effort: None,
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
pub struct ChatResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponseStream {
    pub id: String,
    pub object: String,
    pub choices: Vec<ChoiceStream>,
    pub created: i64,
    pub model: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub index: i64,
    pub message: Message,
    pub finish_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChoiceStream {
    pub index: i64,
    pub delta: Delta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// 插件可将厂商返回的累计正文标记为快照，由流解码器转换为增量。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_snapshot: Option<String>,
    /// 插件可将厂商返回的累计思考内容标记为快照，由流解码器转换为增量。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content_snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

/// ---- tool call 结构（用于 a / stream delta 累积） ----
pub struct ToolFunctionArg {
    pub name: String,
    pub r#type: String,
    pub required: Option<bool>,
    pub description: Option<String>,
    pub default: Option<Value>,
    pub max: Option<Value>,
    pub min: Option<Value>,
    pub enum_values: Option<Vec<Value>>,
    pub items: Option<Box<Value>>,
    pub format: Option<String>,
    pub properties: Option<Value>,
    pub additional_properties: Option<Value>,
}

impl ToolFunctionArg {
    pub fn new(name: impl Into<String>, r#type: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            r#type: r#type.into(),
            required: Some(false),
            description: None,
            default: None,
            max: None,
            min: None,
            enum_values: None,
            items: None,
            format: None,
            properties: None,
            additional_properties: None,
        }
    }

    pub fn required(mut self, required: bool) -> Self {
        self.required = Some(required);
        self
    }

    pub fn desc(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn default<V: Into<Value>>(mut self, default: V) -> Self {
        self.default = Some(default.into());
        self
    }

    pub fn max<V: Into<Value>>(mut self, max: V) -> Self {
        self.max = Some(max.into());
        self
    }

    pub fn min<V: Into<Value>>(mut self, min: V) -> Self {
        self.min = Some(min.into());
        self
    }

    pub fn enum_values<I, V>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Into<Value>,
    {
        self.enum_values = Some(values.into_iter().map(Into::into).collect());
        self
    }

    pub fn items<V: Into<Value>>(mut self, items: V) -> Self {
        self.items = Some(Box::new(items.into()));
        self
    }

    pub fn format(mut self, format: impl Into<String>) -> Self {
        self.format = Some(format.into());
        self
    }

    pub fn properties<V: Into<Value>>(mut self, properties: V) -> Self {
        self.properties = Some(properties.into());
        self
    }

    pub fn additional_properties<V: Into<Value>>(mut self, additional_properties: V) -> Self {
        self.additional_properties = Some(additional_properties.into());
        self
    }

    pub fn schema(&self) -> Value {
        let mut v = serde_json::json!({"type": self.r#type});

        if let Some(desc) = &self.description {
            v["description"] = serde_json::json!(desc);
        }
        if let Some(vv) = &self.default {
            v["default"] = vv.clone();
        }
        if let Some(vv) = &self.max {
            v["maximum"] = vv.clone();
        }
        if let Some(vv) = &self.min {
            v["minimum"] = vv.clone();
        }
        if let Some(vv) = &self.enum_values {
            v["enum"] = serde_json::json!(vv);
        }
        if let Some(vv) = &self.items {
            v["items"] = (**vv).clone();
        }
        if let Some(vv) = &self.format {
            v["format"] = serde_json::json!(vv);
        }
        if let Some(vv) = &self.properties {
            v["properties"] = vv.clone();
        }
        if let Some(vv) = &self.additional_properties {
            v["additionalProperties"] = vv.clone();
        }
        v
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolFunctionCall {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
pub struct DecoderEvent {
    pub event_info: EventInfo,
    pub payload: DecoderEventPayload,
}

/// 轮次结束状态
#[derive(Clone, Debug)]
pub enum TurnStatus {
    Ok,
    Cancelled,
    Interrupted,
    Error(ClientError),
}

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
        finish_reason: Option<String>,
        usage: Option<Usage>,
    },
}

/// 会话控制指令（通过 SessionHandle 发往 drive loop）
#[derive(Debug)]
pub(crate) enum CtrlMsg {
    /// 切换到另一个插件（下一轮生效）
    SwitchPlugin {
        plugin_id: String,
        api_key: SecretString,
    },
    /// 将消息树 head 移动到指定节点（重说 / 分支 / 历史回退）
    Checkout { node_id: u64 },
    /// 从当前未完成的 assistant head 继续生成，不写入伪造的用户节点。
    Continue { node_id: u64 },
}

#[derive(Debug, Clone)]
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
        arguments: String,
    },
    ToolRetrying {
        index: usize,
        name: String,
        attempt: usize,
        max_retries: usize,
        delay_ms: u64,
    },
    ToolResult {
        index: usize,
        output: String,
        is_error: bool,
    },

    /// 发送副本因上下文预算被机械裁剪；持久化消息树保持不变。
    ContextTrimmed {
        dropped_rounds: usize,
        truncated_messages: usize,
        before: u64,
        after: u64,
        suggest_compaction: bool,
    },

    TurnEnd {
        status: TurnStatus,
        /// 本轮助手消息节点 ID；没有产生任何助手输出时为 None。
        node_id: Option<u64>,
        /// 供应商返回的结束原因，例如 stop / length / tool_calls。
        finish_reason: Option<String>,
        /// 本轮若由续写触发，记录被续写的助手节点 ID。
        continuation_of: Option<u64>,
        /// API 用量统计（通常在流式最后一个 chunk 或非流式响应中返回）
        usage: Option<Usage>,
        /// 用本轮真实 prompt usage 更新后的 token 估算校准系数。
        calibration_factor: Option<f64>,
    },
    /// 分支切换完成（checkout 成功）。
    BranchChanged {
        node_id: u64,
    },
    Error(ClientError),
}
