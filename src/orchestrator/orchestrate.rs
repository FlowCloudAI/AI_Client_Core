use anyhow::Result;
use crate::orchestrator::context::{AssembledTurn, TaskContext};

// ═════════════════════════════════════════════════════════════
//                     编排器接口（Orchestrate trait）
// ═════════════════════════════════════════════════════════════

/// 编排器接口。
///
/// 每轮对话开始前，Session 调用 `assemble` 获取本轮配置。
/// 调用方可提供任意实现——内置的 `DefaultOrchestrator` 是基于
/// `Sense + ToolRegistry` 的默认策略，但不是唯一选择。
///
/// # 设计约定
/// - `Sense` 是静态默认工具范围 + 系统提示的声明层
/// - `Orchestrate` 是每轮最终裁决层，`AssembledTurn` 的内容优先级最高
/// - Session 只消费 `AssembledTurn`，不感知具体编排策略
pub trait Orchestrate: Send + Sync {
    /// 根据当前上下文装配本轮配置。
    ///
    /// 返回的 `AssembledTurn` 是本轮 LLM 调用的完整配置快照：
    /// - `tool_schemas` / `enabled_tools`：本轮可用工具（以此为准，覆盖 Sense 默认值）
    /// - `read_only`：是否禁止写入类工具（Session 在执行工具前检查此标志）
    /// - `context_messages`：注入到消息流的额外 system 片段
    /// - `model_override` / `temperature_override` / `max_tokens_override`：参数覆盖
    fn assemble(&self, ctx: &TaskContext) -> Result<AssembledTurn>;
}
