// plugin/pipeline.rs——API 管道
use anyhow::Result;
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
}

impl ApiPipeline {
    /// 创建插件管道，指定插件时要求插件已经加载。
    pub fn try_new(registry: Arc<PluginRegistry>, plugin_id: Option<String>) -> Result<Self> {
        if let Some(id) = &plugin_id {
            registry.try_increment_loaded_ref(id)?;
        }
        Ok(Self {
            registry,
            plugin_id,
        })
    }

    /// 切换到另一个插件（正确维护引用计数）。
    pub fn try_set_plugin(&mut self, new_plugin_id: Option<String>) -> Result<()> {
        if self.plugin_id == new_plugin_id {
            return Ok(());
        }

        self.registry
            .try_switch_ref(self.plugin_id.as_deref(), new_plugin_id.as_deref(), true)?;
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

    /// 便捷方法：序列化 → map，返回可直接发送的请求体字符串。
    ///
    /// 无插件时 `to_string` 一次即返回；有插件时 `map_request` 后用
    /// `IgnoredAny` 校验产物为合法 JSON（不重建 Value 树）。
    /// 相比旧 `prepare_request_json`，省去「map 后 from_str 建树 → 上层再序列化」的往返。
    pub fn prepare_request_body<T: serde::Serialize>(&self, req: &T) -> Result<String> {
        let raw = serde_json::to_string(req).map_err(|e| {
            ClientError::new(ErrorCode::LlmRequestBadPayload, "请求 JSON 序列化失败")
                .with_kv("source", e.to_string())
        })?;
        if self.plugin_id.is_none() {
            return Ok(raw);
        }
        let mapped = self.map_request(&raw)?;
        serde_json::from_str::<serde::de::IgnoredAny>(&mapped).map_err(|e| {
            ClientError::new(ErrorCode::LlmRequestBadPayload, "映射后请求 JSON 非法")
                .with_kv("source", e.to_string())
        })?;
        Ok(mapped)
    }
}

impl Drop for ApiPipeline {
    fn drop(&mut self) {
        // Session 销毁时减少引用计数
        if let Some(id) = &self.plugin_id
            && let Err(error) = self.registry.try_decrement_ref(id)
        {
            log::error!("[plugin] failed to decrement ref-count for '{id}': {error:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_mode_rejects_selected_unloaded_plugin() {
        let registry = Arc::new(PluginRegistry::empty().unwrap());
        let error = ApiPipeline::try_new(Arc::clone(&registry), Some("missing".to_string()))
            .err()
            .expect("未加载插件应被拒绝");
        assert_eq!(registry.try_get_ref_count("missing").unwrap(), 0);
        assert!(
            error.to_string().contains("PLUGIN_NOT_LOADED"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn none_plugin_uses_passthrough() {
        let registry = Arc::new(PluginRegistry::empty().unwrap());
        let pipeline = ApiPipeline::try_new(registry, None).unwrap();

        assert_eq!(pipeline.map_request("{\"a\":1}").unwrap(), "{\"a\":1}");
        assert_eq!(pipeline.map_response("{\"b\":2}").unwrap(), "{\"b\":2}");
        assert_eq!(
            pipeline
                .prepare_request_body(&serde_json::json!({ "a": 1 }))
                .unwrap(),
            "{\"a\":1}"
        );
    }
}
