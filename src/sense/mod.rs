use std::sync::Arc;
use tokio::sync::Mutex;
use crate::llm::types::ChatRequest;
use crate::tool::registry::ToolRegistry;

/// 模式预设。
///
/// 与旧 `SenseLoader` 的区别：
/// - `install_tools` 目标是 `ToolRegistry`（全局工具库），不是 `ToolFunctions`（Session 私有）
/// - 新增 `tool_whitelist`：声明本模式需要的工具子集（None = 全部可用）
/// - 新增 `default_params`：声明默认参数（model, temperature 等）
///
/// Sense 只负责"声明"，不负责"本轮装配"——那是 Orchestrator 的职责。
pub trait Sense: Send + Sync {
    /// 系统提示词列表，按顺序注入 conversation。
    fn prompts(&self) -> Vec<String>;

    /// 默认的 ChatRequest 配置（model, stream, thinking 等）。
    /// 返回 None 表示不覆盖。
    fn default_request(&self) -> Option<ChatRequest> {
        None
    }

    /// 向全局 ToolRegistry 注册本模式需要的工具。
    /// 在 Client 初始化时调用一次。
    fn install_tools(&self, registry: &mut ToolRegistry) -> anyhow::Result<()>;

    /// 本模式的工具白名单。
    /// 返回 None 表示使用 registry 中所有已注册工具。
    /// 返回 Some(vec) 表示只使用指定名称的工具。
    fn tool_whitelist(&self) -> Option<Vec<String>> {
        None
    }
}

pub type SenseState<T> = Arc<Mutex<T>>;

pub fn sense_state_new<T: Default>() -> SenseState<T> {
    Arc::new(Mutex::new(T::default()))
}