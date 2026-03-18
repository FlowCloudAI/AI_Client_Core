use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::sync::Arc;

use crate::llm::config::SessionConfig;
use crate::llm::session::LLMSession;
use crate::plugin::manager::PluginManager;
use crate::plugin::registry::PluginRegistry;
use crate::plugin::types::{PluginKind, PluginMeta};

// ─────────────────────── FlowCloudAIClient ──────────────────

/// 顶层门面。
///
/// 职责：
/// 1. 初始化插件系统（扫描 → 编译 → 注册）
/// 2. 创建各类 Session（LLM / Image / TTS）
/// 3. 不持有任何 Session —— Session 的生命周期由调用方管理
pub struct FlowCloudAIClient {
    registry: Arc<PluginRegistry>,
}

impl FlowCloudAIClient {
    /// 初始化客户端，扫描 `./plugins` 目录。
    ///
    /// 即使没有找到任何插件，也会成功返回（registry 为空）。
    pub fn new() -> Result<Self> {
        let registry = match PluginManager::new("./plugins".into()) {
            Ok(pm) => {
                for (id, meta) in &pm.plugins {
                    println!("[plugin] found: {} ({:?})", id, meta.kind);
                }
                PluginRegistry::build(
                    pm.engine.clone(),
                    pm.linker.clone(),
                    pm.plugins.clone(),
                    8, // 每个插件最多 8 个空闲实例
                )?
            }
            Err(e) => {
                println!("[plugin] no plugins loaded: {}", e);
                PluginRegistry::empty()?
            }
        };

        Ok(Self {
            registry: Arc::new(registry),
        })
    }

    // ── 插件管理 ──

    /// 激活指定插件（创建实例池）。
    ///
    /// 必须在 `create_*_session` 之前调用。
    /// 一旦有 session 持有 registry 引用，此方法会失败。
    pub fn load_plugin(&mut self, id: &str) -> Result<()> {
        Arc::get_mut(&mut self.registry)
            .ok_or_else(|| {
                anyhow!(
                    "cannot load plugin while sessions hold a registry reference; \
                 call load_plugin before creating any session"
                )
            })?
            .load(id)
    }

    /// 获取所有插件元数据。
    pub fn list_plugins(&self) -> &HashMap<String, PluginMeta> {
        self.registry.list_plugins()
    }

    /// 按类型筛选插件。
    pub fn list_by_kind(&self, kind: PluginKind) -> Vec<(&String, &PluginMeta)> {
        self.registry.list_by_kind(kind)
    }

    /// 获取池状态（诊断用）。
    pub fn pool_stats(&self) -> HashMap<&str, usize> {
        self.registry.pool_stats()
    }

    // ── Session 工厂 ──

    /// 创建 LLM 会话。
    ///
    /// - `plugin_id`: 使用哪个插件做请求/响应映射。None = 直通模式。
    /// - `api_key`: API 密钥。
    ///
    /// 返回的 Session 持有 `Arc<PluginRegistry>` 的 clone，
    /// 通过 `acquire()` 按需借出 mapper，用完自动归还。
    pub fn create_llm_session(&self, plugin_id: &str, api_key: &str) -> Result<LLMSession> {
        let url = self.registry.get_url(plugin_id)?;
        if !url.starts_with("http") {
            return Err(anyhow!("plugin '{}' has invalid URL: {}", plugin_id, url));
        }
        let base_url = url.to_string();

        let config = SessionConfig {
            base_url,
            api_key: api_key.to_string(),
            event_buffer: 256,
            request_timeout: 60,
            max_tool_rounds: 10,
            max_line_bytes: 1024 * 1024,
        };

        LLMSession::new(
            config,
            Arc::clone(&self.registry),
            Some(plugin_id.to_string()),
        )
    }

    // 将来扩展：
    // pub fn create_image_session(...) -> Result<ImageSession> { ... }
    // pub fn create_tts_session(...) -> Result<TTSSession> { ... }
}
