use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use tokio::sync::watch;
use serde_json::Value;
use crate::llm::tree::ConversationTree;
use crate::llm::types::{ChatRequest, CtrlMsg, Message};
use crate::orchestrator::TaskContext;
use crate::ThinkingType;

// ═════════════════════════════════════════════════════════════
//                     会话外部句柄
// ═════════════════════════════════════════════════════════════

/// 会话外部句柄，允许 UI 层随时读取当前对话状态或发送控制指令
///
/// 提供线程安全的对话快照访问，无需获得 `LLMSession` 的所有权
#[derive(Clone)]
pub struct SessionHandle {
    pub(crate) inner: Arc<RwLock<ChatRequest>>,
    pub(crate) tree: Arc<RwLock<ConversationTree>>,
    pub(crate) system_messages: Arc<Vec<Message>>,
    pub(crate) ctrl_tx: mpsc::Sender<CtrlMsg>,
    pub(crate) cancel_tx: watch::Sender<u64>,
    pub(crate) ctx_tx: mpsc::Sender<TaskContext>,
}

impl SessionHandle {
    /// 获取当前对话的完整快照（参数 + 线性化消息历史）
    pub async fn get_conversation(&self) -> ChatRequest {
        let mut req = self.inner.read().await.clone();
        req.messages = self
            .system_messages
            .iter()
            .cloned()
            .chain(self.tree.read().await.linearize())
            .collect();
        req
    }

    /// 设置模型名称
    pub async fn set_model(&self, model: &str) {
        self.inner.write().await.model = model.to_string();
    }

    /// 设置温度
    pub async fn set_temperature(&self, v: f64) {
        self.inner.write().await.temperature = Some(v);
    }

    /// 设置流式响应
    pub async fn set_stream(&self, v: bool) {
        self.inner.write().await.stream = Some(v);
    }

    /// 设置最大生成长度
    pub async fn set_max_tokens(&self, v: i64) {
        self.inner.write().await.max_tokens = Some(v);
    }

    /// 设置是否启用思考
    pub async fn set_thinking(&self, enabled: bool) {
        self.inner.write().await.thinking = Some(if enabled {
            ThinkingType::enabled()
        } else {
            ThinkingType::disabled()
        });
    }

    /// 设置频率惩罚（-2.0 ~ 2.0）
    pub async fn set_frequency_penalty(&self, v: f64) {
        self.inner.write().await.frequency_penalty = Some(v);
    }

    /// 设置存在惩罚（-2.0 ~ 2.0）
    pub async fn set_presence_penalty(&self, v: f64) {
        self.inner.write().await.presence_penalty = Some(v);
    }

    /// 设置核采样（0.0 ~ 1.0）
    pub async fn set_top_p(&self, v: f64) {
        self.inner.write().await.top_p = Some(v);
    }

    /// 设置停止词列表
    pub async fn set_stop(&self, stop: Vec<String>) {
        self.inner.write().await.stop = Some(stop);
    }

    /// 设置响应格式（如 `{"type":"json_object"}`）
    pub async fn set_response_format(&self, format: Value) {
        self.inner.write().await.response_format = Some(format);
    }

    /// 设置并行候选数
    pub async fn set_n(&self, n: i32) {
        self.inner.write().await.n = Some(n);
    }

    /// 设置工具选择策略（"auto" / "none" / 指定工具名）
    pub async fn set_tool_choice(&self, choice: &str) {
        self.inner.write().await.tool_choice = Some(choice.to_string());
    }

    /// 设置是否返回 token 对数概率
    pub async fn set_logprobs(&self, v: bool) {
        self.inner.write().await.logprobs = Some(v);
    }

    /// 设置返回前 N 个 token 的对数概率（需先启用 logprobs）
    pub async fn set_top_logprobs(&self, n: i64) {
        self.inner.write().await.top_logprobs = Some(n);
    }

    /// 批量更新会话参数（单次加锁，适合同时修改多个字段）
    pub async fn update<F>(&self, f: F)
    where
        F: FnOnce(&mut ChatRequest),
    {
        f(&mut *self.inner.write().await);
    }

    /// 切换到另一个插件（下一轮对话生效）。
    ///
    /// 如果会话已关闭，返回错误字符串。
    pub async fn switch_plugin(&self, plugin_id: &str, api_key: &str) -> Result<(), String> {
        self.ctrl_tx
            .send(CtrlMsg::SwitchPlugin {
                plugin_id: plugin_id.to_string(),
                api_key: api_key.to_string(),
            })
            .await
            .map_err(|_| "会话已关闭".to_string())
    }

    /// 将消息树 head 移动到指定节点（重说 / 分支 / 历史回退）。
    ///
    /// 在会话等待用户输入期间生效：
    /// - 若目标节点 role 为 "user"，drive loop 立即继续（免去下一次用户输入）
    /// - 若目标节点 role 为 "assistant"，drive loop 继续等待用户输入
    ///
    /// 如果会话已关闭，返回错误字符串。
    pub async fn checkout(&self, node_id: u64) -> Result<(), String> {
        self.ctrl_tx
            .send(CtrlMsg::Checkout { node_id })
            .await
            .map_err(|_| "会话已关闭".to_string())
    }

    /// 取消当前进行中的轮次。
    pub fn cancel(&self) {
        let next = self.cancel_tx.borrow().wrapping_add(1);
        let _ = self.cancel_tx.send(next);
    }

    /// 更新编排上下文（下一轮对话开始前生效）。
    ///
    /// Session 在每轮开始前会通过 `try_recv` 拉取最新的 `TaskContext`，
    /// 再交给 `Orchestrate::assemble` 决定本轮配置。
    ///
    /// 高级用法：可在同一轮之间多次调用，Session 只会取最后一个值。
    pub async fn set_task_context(&self, ctx: TaskContext) -> Result<(), String> {
        self.ctx_tx
            .send(ctx)
            .await
            .map_err(|_| "会话已关闭".to_string())
    }
}
