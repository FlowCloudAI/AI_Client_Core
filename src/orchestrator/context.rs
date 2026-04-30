use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;

// ═════════════════════════════════════════════════════════════
//                       任务上下文（TaskContext）
// ═════════════════════════════════════════════════════════════

/// 每轮对话的动态上下文。
///
/// 由调用方（UI 层 / API 层）在每次对话前构建，
/// 通过 `SessionHandle::set_task_context` 发送给 Session，
/// 再由 `Orchestrate::assemble` 消费转换为 `AssembledTurn`。
///
/// # 字段分层约定
/// - **中性扩展字段**（首选）：`attributes` / `flags` / `payload`
///   适合自定义 `Orchestrate` 实现；不与具体业务绑定。
/// - **遗留字段**：`task_type` / `project_id` / `selection` / `entities` / `read_only`
///   由 `DefaultOrchestrator` 消费；自定义实现可忽略它们。
///   未来版本可能逐步迁移到 `attributes` / `flags`。
#[derive(Clone, Debug, Default)]
pub struct TaskContext {
    // ── 中性扩展字段（推荐使用） ──

    /// 任意字符串键值对上下文（推荐用于自定义 Orchestrate 实现）。
    ///
    /// 示例：`{"scene": "editor", "language": "zh"}`
    pub attributes: HashMap<String, String>,

    /// 任意布尔标志（推荐用于自定义 Orchestrate 实现）。
    ///
    /// 示例：`{"read_only": true, "streaming": false}`
    pub flags: HashMap<String, bool>,

    /// 非结构化附加数据（适合传递复杂对象）。
    pub payload: Option<Value>,

    // ── 遗留字段（DefaultOrchestrator 使用） ──

    /// 任务类型（如 "creative_writing", "proofreading", "code_generation"）。
    /// `DefaultOrchestrator` 用此字段决定参数覆盖策略。
    pub task_type: String,

    /// 当前项目 ID（可选）。
    pub project_id: Option<String>,

    /// 当前选区 / 焦点内容（如选中的文本段落）。
    pub selection: Option<String>,

    /// 相关词条 / 实体（如角色名、地点名）。
    pub entities: Vec<String>,

    /// 只读权限标记。
    /// `DefaultOrchestrator` 会将此值传播到 `AssembledTurn::read_only`。
    /// 自定义实现可通过 `flags["read_only"]` 传递。
    pub read_only: bool,
}

impl TaskContext {
    /// 便捷构造：从 attributes/flags 读取，带默认值回退。
    pub fn flag(&self, key: &str) -> bool {
        self.flags.get(key).copied().unwrap_or(false)
    }

    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).map(String::as_str)
    }

    /// 将 `payload` 反序列化为指定类型。
    ///
    /// - `payload` 为 `None` 时返回 `Ok(None)`
    /// - 反序列化失败时返回 `Err`
    pub fn decode_payload<T: DeserializeOwned>(&self) -> anyhow::Result<Option<T>> {
        match &self.payload {
            Some(v) => Ok(Some(serde_json::from_value(v.clone())?)),
            None => Ok(None),
        }
    }
}

// ═════════════════════════════════════════════════════════════
//                       组装后的轮次（AssembledTurn）
// ═════════════════════════════════════════════════════════════

/// Orchestrate::assemble 的输出：本轮 LLM 调用的完整配置快照。
///
/// Session 只消费此结构，不感知具体的编排策略或上下文格式。
/// `Orchestrate` 实现是最终裁决层，此结构中的值优先级高于 Sense 默认值。
#[derive(Clone, Debug, Default)]
pub struct AssembledTurn {
    /// 额外注入的 system messages。
    ///
    /// 插入位置语义由 Session 固定定义为：
    /// - 优先插在“最后一个待续会话块”之前；
    /// - 若不存在待续会话块，则退化为插在最新用户消息之前。
    ///
    /// 其中“待续会话块”当前指请求尾部的
    /// `assistant(tool_calls) + tool...` 连续片段，
    /// 用于保证工具续轮时不破坏模型要求的相邻顺序。
    pub context_messages: Vec<String>,

    /// 本轮工具配置，三种语义严格区分：
    ///
    /// | 值 | 含义 |
    /// |---|---|
    /// | `None` | 不干预，沿用 Session 当前工具配置（snapshot 阶段的 ToolRegistry） |
    /// | `Some(vec![])` | 显式禁用全部工具（LLM 本轮不可调用任何工具） |
    /// | `Some(schemas)` | 显式覆盖为给定工具集，替换 Session 当前配置 |
    pub tool_schemas: Option<Vec<Value>>,

    /// 本轮可用的工具名列表（用于执行时白名单校验）。
    pub enabled_tools: Vec<String>,

    /// 是否禁止写入类工具调用（Session 在 execute_tool_calls 前检查此标志）。
    /// 优先级高于 TaskContext::read_only——由 Orchestrate 实现最终决定。
    pub read_only: bool,

    /// 参数覆盖（优先级高于 Sense 默认值）。
    pub model_override: Option<String>,
    pub temperature_override: Option<f64>,
    pub max_tokens_override: Option<i64>,
}
