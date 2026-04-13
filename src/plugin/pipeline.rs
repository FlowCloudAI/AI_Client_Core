// plugin/pipeline.rs
use std::sync::Arc;
use anyhow::Result;
use serde_json::Value;

use crate::plugin::mapper::{ApiMapper, PassthroughMapper};
use crate::plugin::registry::PluginRegistry;

/// 可复用的 mapper 管道。
/// LLM / Image / TTS session 各自组合持有一个。
pub struct ApiPipeline {
    registry: Arc<PluginRegistry>,
    plugin_id: Option<String>,
}

impl ApiPipeline {
    pub fn new(registry: Arc<PluginRegistry>, plugin_id: Option<String>) -> Self {
        // 如果指定了 plugin_id，增加引用计数
        if let Some(id) = &plugin_id {
            registry.increment_ref(id);
        }
        Self { registry, plugin_id }
    }

    /// 切换到另一个插件（正确维护引用计数）。
    pub fn set_plugin(&mut self, new_plugin_id: Option<String>) {
        if let Some(id) = &self.plugin_id {
            self.registry.decrement_ref(id);
        }
        if let Some(id) = &new_plugin_id {
            self.registry.increment_ref(id);
        }
        self.plugin_id = new_plugin_id;
    }

    /// 查询指定插件的 API 端点 URL。
    pub fn get_url(&self, plugin_id: &str) -> Result<&str> {
        self.registry.get_url(plugin_id)
    }

    fn acquire_mapper(&self) -> Result<Box<dyn ApiMapper + Send + '_>> {
        match &self.plugin_id {
            Some(id) if self.registry.is_loaded(id) => {
                let pooled = self.registry.acquire(id)?;
                Ok(Box::new(pooled))
            }
            _ => Ok(Box::new(PassthroughMapper)),
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
}

impl Drop for ApiPipeline {
    fn drop(&mut self) {
        // Session 销毁时减少引用计数
        if let Some(id) = &self.plugin_id {
            self.registry.decrement_ref(id);
        }
    }
}