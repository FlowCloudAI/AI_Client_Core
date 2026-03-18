use std::sync::Arc;
use tokio::sync::RwLock;
use crate::llm::types::ChatRequest;
use crate::ThinkingType;

// ═════════════════════════════════════════════════════════════
//                     会话外部句柄
// ═════════════════════════════════════════════════════════════

/// 会话外部句柄，允许 UI 层随时读取当前对话状态
///
/// 提供线程安全的对话快照访问，无需获得 `LLMSession` 的所有权
#[derive(Clone)]
pub struct SessionHandle {
    pub(crate) inner: Arc<RwLock<ChatRequest>>,
}

impl SessionHandle {
    /// 获取当前对话的完整快照
    pub async fn get_conversation(&self) -> ChatRequest {
        self.inner.read().await.clone()
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
}