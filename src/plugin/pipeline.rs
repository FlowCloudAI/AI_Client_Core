// plugin/pipeline.rs——API 管道
use std::sync::Arc;
use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::plugin::mapper::{ApiMapper, PassthroughMapper};
use crate::plugin::registry::PluginRegistry;
use crate::plugin::types::PluginKind;

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
            Self::validate_plugin_available(&registry, id, mode)?;
            registry.try_increment_ref(id)?;
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

        if let Some(id) = &new_plugin_id {
            Self::validate_plugin_available(&self.registry, id, self.mode)?;
        }

        if let Some(id) = &self.plugin_id {
            self.registry.try_decrement_ref(id)?;
        }
        self.plugin_id = None;

        if let Some(id) = new_plugin_id {
            self.registry.try_increment_ref(&id)?;
            self.plugin_id = Some(id);
        }
        Ok(())
    }

    /// 查询指定插件的 API 端点 URL。
    pub fn get_url(&self, plugin_id: &str) -> Result<String> {
        self.registry.get_url(plugin_id)
    }

    /// 校验插件类型。
    pub fn ensure_plugin_kind(&self, plugin_id: &str, expected: PluginKind) -> Result<()> {
        let meta = self
            .registry
            .try_get_meta(plugin_id)?
            .ok_or_else(|| anyhow!("plugin '{}' not found", plugin_id))?;
        if meta.kind != expected {
            return Err(anyhow!(
                "plugin '{}' kind mismatch: expected {:?}, got {:?}",
                plugin_id,
                expected,
                meta.kind
            ));
        }
        Ok(())
    }

    fn acquire_mapper(&self) -> Result<Box<dyn ApiMapper + Send + '_>> {
        match &self.plugin_id {
            None => Ok(Box::new(PassthroughMapper)),
            Some(id) if self.registry.try_is_loaded(id)? => {
                let pooled = self.registry.acquire(id)?;
                Ok(Box::new(pooled))
            }
            Some(_) if self.mode == PipelineMode::AllowPassthrough => Ok(Box::new(PassthroughMapper)),
            Some(id) => Err(anyhow!("plugin '{}' is selected but not loaded", id)),
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
        let raw = serde_json::to_string(json)?;
        let mapped = self.map_request(&raw)?;
        Ok(serde_json::from_str(&mapped)?)
    }

    fn validate_plugin_available(
        registry: &PluginRegistry,
        plugin_id: &str,
        mode: PipelineMode,
    ) -> Result<()> {
        if mode == PipelineMode::AllowPassthrough {
            return Ok(());
        }
        if registry.try_is_loaded(plugin_id)? {
            Ok(())
        } else {
            Err(anyhow!("plugin '{}' is selected but not loaded", plugin_id))
        }
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
            err.to_string().contains("selected but not loaded"),
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
