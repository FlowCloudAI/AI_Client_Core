use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use crate::image::ImageSession;
use crate::llm::config::SessionConfig;
use crate::llm::session::LLMSession;
use crate::orchestrator::TaskOrchestrator;
use crate::plugin::manager::PluginManager;
use crate::plugin::pipeline::ApiPipeline;
use crate::plugin::registry::PluginRegistry;
use crate::plugin::types::{PluginKind, PluginMeta};
use crate::sense::Sense;
use crate::tool::registry::ToolRegistry;
use crate::tts::TTSSession;
// ─────────────────────── FlowCloudAIClient ──────────────────

pub struct FlowCloudAIClient {
    plugin_registry: Arc<PluginRegistry>,
    tool_registry: Arc<ToolRegistry>,
}

impl FlowCloudAIClient {
    pub fn new(plugins_dir: PathBuf) -> Result<Self> {
        let plugin_registry = match PluginManager::new(plugins_dir) {
            Ok(pm) => {
                for (id, meta) in &pm.plugins {
                    println!("[plugin] found: {} ({:?})", id, meta.kind);
                }
                PluginRegistry::build(
                    pm.engine.clone(),
                    pm.linker.clone(),
                    pm.plugins.clone(),
                    8,
                )?
            }
            Err(e) => {
                println!("[plugin] no plugins loaded: {}", e);
                PluginRegistry::empty()?
            }
        };

        Ok(Self {
            plugin_registry: Arc::new(plugin_registry),
            tool_registry: Arc::new(ToolRegistry::new()),
        })
    }

    // ── 插件管理 ──

    pub fn load_plugin(&mut self, id: &str) -> Result<()> {
        Arc::get_mut(&mut self.plugin_registry)
            .ok_or_else(|| {
                anyhow!(
                    "cannot load plugin while sessions hold a registry reference; \
                     call load_plugin before creating any session"
                )
            })?
            .load(id)
    }

    pub fn list_plugins(&self) -> &HashMap<String, PluginMeta> {
        self.plugin_registry.list_plugins()
    }

    pub fn list_by_kind(&self, kind: PluginKind) -> Vec<(&String, &PluginMeta)> {
        self.plugin_registry.list_by_kind(kind)
    }

    pub fn pool_stats(&self) -> HashMap<&str, usize> {
        self.plugin_registry.pool_stats()
    }

    // ── Sense / 工具管理 ──

    /// 安装一个 Sense 的工具到全局 ToolRegistry。
    ///
    /// 必须在 `create_llm_session` 之前调用。
    /// 多个 Sense 可以叠加安装（工具名不冲突即可）。
    pub fn install_sense(&mut self, sense: &dyn Sense) -> Result<()> {
        let reg = Arc::get_mut(&mut self.tool_registry)
            .ok_or_else(|| {
                anyhow!(
                    "cannot install sense while sessions hold a tool registry reference; \
                     call install_sense before creating any session"
                )
            })?;
        sense.install_tools(reg)
    }

    /// 获取 ToolRegistry 引用。
    pub fn tool_registry(&self) -> &Arc<ToolRegistry> {
        &self.tool_registry
    }

    // ── Session 工厂 ──

    /// 创建 LLM 会话（简单模式，兼容旧 API）。
    pub fn create_llm_session(&self, plugin_id: &str, api_key: &str) -> Result<LLMSession> {
        let url = self.plugin_registry.get_url(plugin_id)?;
        if !url.starts_with("http") {
            return Err(anyhow!("plugin '{}' has invalid URL: {}", plugin_id, url));
        }

        let config = SessionConfig {
            base_url: url.to_string(),
            api_key: api_key.to_string(),
            event_buffer: 256,
            request_timeout: 60,
            max_tool_rounds: 10,
            max_line_bytes: 1024 * 1024,
        };

        // create_llm_session 里
        let pipeline = ApiPipeline::new(
            Arc::clone(&self.plugin_registry),
            Some(plugin_id.to_string()),
        );

        LLMSession::new(
            config,
            pipeline,
            Arc::clone(&self.tool_registry),
        )
    }

    /// 创建 LLM 会话（编排模式）。
    ///
    /// 返回 (Session, Orchestrator)。
    /// Orchestrator 可在运行时 `assemble` 每轮配置。
    pub fn create_orchestrated_session(
        &self,
        plugin_id: &str,
        api_key: &str,
        sense: Box<dyn Sense>,
    ) -> Result<(LLMSession, TaskOrchestrator)> {
        let url = self.plugin_registry.get_url(plugin_id)?;
        if !url.starts_with("http") {
            return Err(anyhow!("plugin '{}' has invalid URL: {}", plugin_id, url));
        }

        let config = SessionConfig {
            base_url: url.to_string(),
            api_key: api_key.to_string(),
            event_buffer: 256,
            request_timeout: 60,
            max_tool_rounds: 10,
            max_line_bytes: 1024 * 1024,
        };

        let orchestrator = TaskOrchestrator::new(
            sense,
            Arc::clone(&self.tool_registry),
        );

        let pipeline = ApiPipeline::new(
            Arc::clone(&self.plugin_registry),
            Some(plugin_id.to_string()),
        );

        let session = LLMSession::new(
            config,
            pipeline,
            Arc::clone(&self.tool_registry),
        )?;

        Ok((session, orchestrator))
    }

    /// 创建 TTS 语音合成会话。
    ///
    /// - `plugin_id`: TTS 插件 ID。
    /// - `api_key`: API 密钥。
    ///
    /// TTSSession 是无状态的，可反复调用 `synthesize`。
    pub fn create_tts_session(&self, plugin_id: &str, api_key: &str) -> Result<TTSSession> {
        let url = self.plugin_registry.get_url(plugin_id)?;
        if !url.starts_with("http") {
            return Err(anyhow!("plugin '{}' has invalid URL: {}", plugin_id, url));
        }

        let config = SessionConfig {
            base_url: url.to_string(),
            api_key: api_key.to_string(),
            event_buffer: 0,          // TTS 不用事件流
            request_timeout: 120,     // TTS 合成可能较慢
            max_tool_rounds: 0,
            max_line_bytes: 0,
        };

        let pipeline = ApiPipeline::new(
            Arc::clone(&self.plugin_registry),
            Some(plugin_id.to_string()),
        );

        TTSSession::new(config, pipeline)
    }

    /// 创建图像生成会话。
    pub fn create_image_session(&self, plugin_id: &str, api_key: &str) -> Result<ImageSession> {
        let url = self.plugin_registry.get_url(plugin_id)?;
        if !url.starts_with("http") {
            return Err(anyhow!("plugin '{}' has invalid URL: {}", plugin_id, url));
        }

        let config = SessionConfig {
            base_url: url.to_string(),
            api_key: api_key.to_string(),
            event_buffer: 0,
            request_timeout: 180,  // 图像生成较慢
            max_tool_rounds: 0,
            max_line_bytes: 0,
        };

        let pipeline = ApiPipeline::new(
            Arc::clone(&self.plugin_registry),
            Some(plugin_id.to_string()),
        );

        ImageSession::new(config, pipeline)
    }
}