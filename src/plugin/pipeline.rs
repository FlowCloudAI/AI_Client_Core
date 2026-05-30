// plugin/pipeline.rs——API 管道
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

use crate::error::{ClientError, ErrorCode};
use crate::plugin::mapper::{ApiMapper, PassthroughMapper};
use crate::plugin::registry::PluginRegistry;
use crate::plugin::types::{PluginKind, ThinkingEffort};

/// 可复用的 mapper 管道。
/// LLM / Image / TTS session 各自组合持有一个。
pub struct ApiPipeline {
    registry: Arc<PluginRegistry>,
    plugin_id: Option<String>,
    mode: PipelineMode,
}

/// 插件管道模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineMode {
    /// 严格模式：指定插件但未加载时返回错误。
    Strict,
    /// 兼容模式：指定插件不可用时允许直通。
    AllowPassthrough,
}

impl ApiPipeline {
    /// 创建严格管道。
    ///
    /// 兼容旧 API：引用计数锁异常时不会 panic。新代码应优先使用 `try_new`。
    pub fn new(registry: Arc<PluginRegistry>, plugin_id: Option<String>) -> Self {
        Self::with_mode(registry, plugin_id, PipelineMode::Strict)
    }

    /// 创建指定模式的管道。
    ///
    /// 兼容旧 API：引用计数锁异常时不会 panic。新代码应优先使用 `try_with_mode`。
    pub fn with_mode(
        registry: Arc<PluginRegistry>,
        plugin_id: Option<String>,
        mode: PipelineMode,
    ) -> Self {
        if let Some(id) = &plugin_id {
            registry.increment_ref(id);
        }
        Self {
            registry,
            plugin_id,
            mode,
        }
    }

    /// 创建严格管道，失败时返回错误。
    pub fn try_new(registry: Arc<PluginRegistry>, plugin_id: Option<String>) -> Result<Self> {
        Self::try_with_mode(registry, plugin_id, PipelineMode::Strict)
    }

    /// 创建指定模式的管道，失败时返回错误。
    pub fn try_with_mode(
        registry: Arc<PluginRegistry>,
        plugin_id: Option<String>,
        mode: PipelineMode,
    ) -> Result<Self> {
        if let Some(id) = &plugin_id {
            if mode == PipelineMode::Strict {
                registry.try_increment_loaded_ref(id)?;
            } else {
                registry.try_increment_ref(id)?;
            }
        }
        Ok(Self {
            registry,
            plugin_id,
            mode,
        })
    }

    /// 切换到另一个插件（正确维护引用计数）。
    ///
    /// 兼容旧 API：切换失败时保留原插件配置。新代码应优先使用 `try_set_plugin`。
    pub fn set_plugin(&mut self, new_plugin_id: Option<String>) {
        let _ = self.try_set_plugin(new_plugin_id);
    }

    /// 切换到另一个插件，失败时返回错误并尽量保留原插件配置。
    pub fn try_set_plugin(&mut self, new_plugin_id: Option<String>) -> Result<()> {
        if self.plugin_id == new_plugin_id {
            return Ok(());
        }

        self.registry.try_switch_ref(
            self.plugin_id.as_deref(),
            new_plugin_id.as_deref(),
            self.mode == PipelineMode::Strict,
        )?;
        self.plugin_id = new_plugin_id;
        Ok(())
    }

    /// 查询指定插件的 API 端点 URL。
    pub fn get_url(&self, plugin_id: &str) -> Result<String> {
        self.registry.get_url(plugin_id)
    }

    /// 校验插件类型。
    pub fn ensure_plugin_kind(&self, plugin_id: &str, expected: PluginKind) -> Result<()> {
        let meta = self.registry.try_get_meta(plugin_id)?.ok_or_else(|| {
            ClientError::new(
                ErrorCode::PluginNotFound,
                format!("插件 '{}' 不存在", plugin_id),
            )
            .with_kv("plugin_id", plugin_id.to_string())
        })?;
        if meta.kind != expected {
            return Err(ClientError::new(
                ErrorCode::PluginKindMismatch,
                format!("插件 '{}' 类型不匹配", plugin_id),
            )
            .with_kv("plugin_id", plugin_id.to_string())
            .with_kv("expected_kind", format!("{:?}", expected))
            .with_kv("actual_kind", format!("{:?}", meta.kind))
            .into());
        }
        Ok(())
    }

    /// 校验当前 LLM 请求是否符合插件声明的模型能力。
    pub fn validate_llm_request(
        &self,
        model: &str,
        thinking_effort: Option<ThinkingEffort>,
    ) -> Result<()> {
        let Some(plugin_id) = &self.plugin_id else {
            return Ok(());
        };
        let meta = self.registry.try_get_meta(plugin_id)?.ok_or_else(|| {
            ClientError::new(
                ErrorCode::PluginNotFound,
                format!("插件 '{}' 不存在", plugin_id),
            )
            .with_kv("plugin_id", plugin_id.to_string())
        })?;
        meta.validate_llm_request(model, thinking_effort)
    }

    fn acquire_mapper(&self) -> Result<Box<dyn ApiMapper + Send + '_>> {
        match &self.plugin_id {
            None => Ok(Box::new(PassthroughMapper)),
            Some(id) if self.registry.try_is_loaded(id)? => {
                let pooled = self.registry.acquire(id)?;
                Ok(Box::new(pooled))
            }
            Some(_) if self.mode == PipelineMode::AllowPassthrough => {
                Ok(Box::new(PassthroughMapper))
            }
            Some(id) => Err(ClientError::new(
                ErrorCode::PluginNotLoaded,
                format!("已选择插件 '{}' 但未加载", id),
            )
            .with_kv("plugin_id", id.clone())
            .into()),
        }
    }

    pub fn map_request(&self, req: &str) -> Result<String> {
        let mut mapper = self.acquire_mapper()?;
        mapper.map_request(req)
    }

    pub fn map_response(&self, raw: &str) -> Result<String> {
        let mut mapper = self.acquire_mapper()?;
        mapper.map_response(raw)
    }

    pub fn map_stream_line(&self, line: &str) -> Result<String> {
        let mut mapper = self.acquire_mapper()?;
        mapper.map_stream_line(line)
    }

    /// 便捷方法：序列化 → map → 反序列化
    pub fn prepare_request_json(&self, json: &Value) -> Result<Value> {
        let raw = serde_json::to_string(json).map_err(|e| {
            ClientError::new(ErrorCode::LlmRequestBadPayload, "请求 JSON 序列化失败")
                .with_kv("source", e.to_string())
        })?;
        let mapped = self.map_request(&raw)?;
        serde_json::from_str(&mapped).map_err(|e| {
            ClientError::new(
                ErrorCode::LlmRequestBadPayload,
                "映射后请求 JSON 反序列化失败",
            )
            .with_kv("source", e.to_string())
            .into()
        })
    }
}

impl Drop for ApiPipeline {
    fn drop(&mut self) {
        // Session 销毁时减少引用计数
        if let Some(id) = &self.plugin_id {
            let _ = self.registry.try_decrement_ref(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_mode_rejects_selected_unloaded_plugin() {
        let registry = Arc::new(PluginRegistry::empty().unwrap());
        let pipeline = ApiPipeline::new(registry, Some("missing".to_string()));

        let err = pipeline.map_request("{}").unwrap_err();
        assert!(
            err.to_string().contains("PLUGIN_NOT_LOADED"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn none_plugin_uses_passthrough() {
        let registry = Arc::new(PluginRegistry::empty().unwrap());
        let pipeline = ApiPipeline::try_new(registry, None).unwrap();

        assert_eq!(pipeline.map_request("{\"a\":1}").unwrap(), "{\"a\":1}");
        assert_eq!(pipeline.map_response("{\"b\":2}").unwrap(), "{\"b\":2}");
    }

    #[test]
    fn allow_passthrough_mode_keeps_compatibility() {
        let registry = Arc::new(PluginRegistry::empty().unwrap());
        let pipeline = ApiPipeline::try_with_mode(
            registry,
            Some("missing".to_string()),
            PipelineMode::AllowPassthrough,
        )
        .unwrap();

        assert_eq!(pipeline.map_stream_line("data: {}").unwrap(), "data: {}");
    }
}
