use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use crate::image::ImageSession;
use crate::llm::config::SessionConfig;
use crate::llm::session::LLMSession;
use crate::orchestrator::Orchestrate;
use crate::plugin::manager::PluginManager;
use crate::plugin::pipeline::ApiPipeline;
use crate::plugin::registry::PluginRegistry;
use crate::plugin::types::{PluginKind, PluginMeta};
use crate::sense::Sense;
use crate::storage::{ConversationMeta, ConversationStore, StoredConversation};
use crate::tool::registry::ToolRegistry;
use crate::tts::TTSSession;
use crate::PluginScanner;
// ─────────────────────── FlowCloudAIClient ──────────────────

pub struct FlowCloudAIClient {
    plugin_registry: Arc<PluginRegistry>,
    tool_registry: Arc<ToolRegistry>,
    plugins_dir: PathBuf,
    storage: Option<Arc<ConversationStore>>,
}

impl FlowCloudAIClient {
    /// 初始化客户端。
    ///
    /// - `plugins_dir`: 插件目录，扫描 `.fcplug` 文件。
    /// - `storage_path`: 可选的对话存储目录。传 `Some(path)` 时启用本地持久化，
    ///   每次 turn_end 状态为 Ok 时自动将对话写盘。
    pub fn new(plugins_dir: PathBuf, storage_path: Option<PathBuf>) -> Result<Self> {
        let plugin_registry = match PluginManager::new(plugins_dir.clone()) {
            Ok(pm) => {
                for (id, meta) in &pm.plugins {
                    println!("[plugin] found: {} ({:?})", id, meta.kind);
                }
                let mut registry = PluginRegistry::build(
                    pm.engine.clone(),
                    pm.linker.clone(),
                    pm.plugins.clone(),
                    8,
                )?;

                // 扫描到的插件默认自动激活，避免 session 在未显式 load_plugin 时退回直通模式。
                let plugin_ids: Vec<String> = registry.list_plugins().keys().cloned().collect();
                for id in plugin_ids {
                    registry.load(&id)?;
                }

                registry
            }
            Err(e) => {
                println!("[plugin] no plugins loaded: {}", e);
                PluginRegistry::empty()?
            }
        };

        let storage = storage_path
            .map(|p| ConversationStore::new(p).map(Arc::new))
            .transpose()?;

        Ok(Self {
            plugin_registry: Arc::new(plugin_registry),
            tool_registry: Arc::new(ToolRegistry::new()),
            plugins_dir,
            storage,
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

    /// 返回所有已识别插件的完整元数据列表。
    ///
    /// 包括 id, name, version, description, author, kind, fcplug_path 等字段。
    pub fn list_all_plugins(&self) -> Vec<PluginMeta> {
        self.plugin_registry
            .list_plugins()
            .values()
            .cloned()
            .collect()
    }

    /// 获取插件的引用计数（用于诊断）。
    pub fn get_plugin_ref_count(&self, plugin_id: &str) -> usize {
        self.plugin_registry.get_ref_count(plugin_id)
    }

    /// 卸载插件：从运行时移除并删除 .fcplug 文件。
    ///
    /// # 逻辑步骤
    /// 1. 检查插件是否存在
    /// 2. 检查引用计数，如果仍被 session 使用则返回错误
    /// 3. 调用 PluginRegistry::unload(plugin_id) 销毁 WASM 实例池
    /// 4. 从 plugins HashMap 中移除元数据
    /// 5. 使用 std::fs::remove_file 删除磁盘上的 .fcplug 文件
    ///
    /// # 错误处理
    /// - 插件不存在：返回 anyhow::Error
    /// - 插件正在被使用：返回明确的错误提示
    /// - 文件操作失败：转换为 anyhow::Error 并附带上下文
    pub fn uninstall_plugin(&mut self, plugin_id: &str) -> Result<()> {
        // 1. 检查插件是否存在
        let meta = self
            .plugin_registry
            .get_meta(plugin_id)
            .ok_or_else(|| anyhow!("plugin '{}' not found", plugin_id))?;

        // 保存文件路径（因为后面要删除）
        let fcplug_path = meta.fcplug_path.clone();

        // 2. 检查引用计数
        let ref_count = self.plugin_registry.get_ref_count(plugin_id);
        if ref_count > 0 {
            return Err(anyhow!(
                "cannot uninstall plugin '{}': still in use by {} session(s). \
                 Please close all sessions using this plugin first.",
                plugin_id,
                ref_count
            ));
        }

        // 3. 获取可变引用并卸载
        let registry = Arc::get_mut(&mut self.plugin_registry).ok_or_else(|| {
            anyhow!(
                "cannot uninstall plugin while sessions hold a registry reference; \
                 call uninstall_plugin before creating any session or after all sessions are dropped"
            )
        })?;

        // 4. 卸载插件（销毁 pool 和 module）
        registry.unload(plugin_id)?;


        // 5. 删除磁盘文件
        std::fs::remove_file(&fcplug_path)
            .map_err(|e| {
                anyhow!(
                    "failed to remove plugin file '{}': {}",
                    fcplug_path.display(),
                    e
                )
            })?;

        println!("[plugin] uninstalled: {} ({})", plugin_id, fcplug_path.display());
        Ok(())
    }

    /// 从外部路径安装插件。
    ///
    /// # 逻辑步骤
    /// 1. 读取 manifest.json 校验 ABI 版本和 ID 唯一性
    /// 2. 将文件复制到内部插件目录（plugins_dir）
    /// 3. 更新 PluginRegistry（编译 WASM 模块）
    /// 4. 返回新插件的 PluginMeta
    ///
    /// # 错误处理
    /// - manifest.json 解析失败：返回 anyhow::Error
    /// - ABI 版本不匹配：返回明确错误
    /// - ID 已存在：返回重复错误
    /// - 文件复制失败：转换为 anyhow::Error 并附带上下文
    /// - WASM 编译失败：返回编译错误
    pub fn install_plugin_from_path(&mut self, source_path: &Path) -> Result<PluginMeta> {
        use crate::SUPPORTED_ABI_VERSION;

        // 1. 读取 manifest.json 校验
        let manifest = PluginScanner::read_plugin_info(source_path)
            .map_err(|e| anyhow!("failed to read plugin manifest from {:?}: {}", source_path, e))?;

        let info = &manifest.meta;

        // 校验 ABI 版本
        if info.abi_version != SUPPORTED_ABI_VERSION {
            return Err(anyhow!(
                "plugin '{}' ABI version mismatch: expected {}, got {}",
                info.id,
                SUPPORTED_ABI_VERSION,
                info.abi_version
            ));
        }

        // 校验 ID 唯一性
        if self.plugin_registry.get_meta(&info.id).is_some() {
            return Err(anyhow!("plugin '{}' already exists", info.id));
        }

        // 2. 复制文件到 plugins_dir，文件名固定为 {plugin_id}.fcplug
        let dest_path = self.plugins_dir.join(format!("{}.fcplug", info.id));

        // 如果文件已在目标位置（直接下载到 plugins_dir 的场景），跳过复制
        let same_file = source_path.canonicalize().ok() == dest_path.canonicalize().ok();
        if !same_file {
            std::fs::copy(source_path, &dest_path)
                .map_err(|e| {
                    anyhow!(
                        "failed to copy plugin from {:?} to {:?}: {}",
                        source_path,
                        dest_path,
                        e
                    )
                })?;
        }

        // 3. 构建 PluginMeta
        let meta = PluginScanner::build_plugin_meta(manifest.clone(), &dest_path)
            .map_err(|e| anyhow!("failed to build plugin meta: {}", e))?;

        // 4. 读取 wasm bytes 并添加到 registry
        let wasm_bytes = {
            let file = std::fs::File::open(&dest_path)
                .map_err(|e| anyhow!("cannot open plugin '{}': {}", info.id, e))?;
            let mut archive = zip::ZipArchive::new(file)
                .map_err(|e| anyhow!("cannot read zip for plugin '{}': {}", info.id, e))?;
            let mut entry = archive.by_name("plugin.wasm")
                .map_err(|_| anyhow!("plugin.wasm not found in '{}'", info.id))?;
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut buf)?;
            buf
        };

        // 获取可变引用并添加模块
        let registry = Arc::get_mut(&mut self.plugin_registry).ok_or_else(|| {
            anyhow!(
                "cannot install plugin while sessions hold a registry reference; \
                 call install_plugin_from_path before creating any session"
            )
        })?;

        registry.add_module(info.id.clone(), meta.clone(), &wasm_bytes)?;
        registry.load(&info.id)?;

        println!("[plugin] installed: {} ({})", info.id, dest_path.display());
        Ok(meta)
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

    /// 获取 ToolRegistry 的可变引用（用于运行时启用/禁用工具）。
    ///
    /// 必须在没有任何 Session 持有 `Arc<ToolRegistry>` 克隆时调用，
    /// 否则返回错误。这与 `install_sense` / `install_tools` 的约束一致。
    pub fn tool_registry_mut(&mut self) -> Result<&mut ToolRegistry> {
        Arc::get_mut(&mut self.tool_registry)
            .ok_or_else(|| {
                anyhow!(
                    "cannot mutate tool registry while sessions hold a reference; \
                     call tool_registry_mut before creating any session or after all sessions are dropped"
                )
            })
    }

    /// 安装自定义工具到全局 ToolRegistry。
    ///
    /// 必须在 `create_llm_session` 之前调用。
    /// 适用于非 Sense 模式的工具注册场景。
    pub fn install_tools<F>(&mut self, installer: F) -> Result<()>
    where
        F: FnOnce(&mut ToolRegistry) -> Result<()>,
    {
        let reg = Arc::get_mut(&mut self.tool_registry)
            .ok_or_else(|| {
                anyhow!(
                    "cannot install tools while sessions hold a tool registry reference; \
                     call install_tools before creating any session"
                )
            })?;
        installer(reg)
    }

    // ── Session 工厂 ──

    /// 创建 LLM 会话（简单模式）。
    ///
    /// - `config_override`: 可选的 `SessionConfig` 覆盖。传 `None` 时使用默认值：
    ///   - `event_buffer`: 256, `request_timeout`: 60s, `max_tool_rounds`: 10, `max_line_bytes`: 1MB
    pub fn create_llm_session(
        &self,
        plugin_id: &str,
        api_key: &str,
        config_override: Option<SessionConfig>,
    ) -> Result<LLMSession> {
        let url = self.plugin_registry.get_url(plugin_id)?;
        if !url.starts_with("http") {
            return Err(anyhow!("plugin '{}' has invalid URL: {}", plugin_id, url));
        }

        let mut config = config_override.unwrap_or_default();
        config.base_url = url.to_string();
        config.api_key = api_key.to_string();

        let pipeline = ApiPipeline::new(
            Arc::clone(&self.plugin_registry),
            Some(plugin_id.to_string()),
        );

        let mut session = LLMSession::new(
            config,
            pipeline,
            Arc::clone(&self.tool_registry),
        )?;

        if let Some(ref store) = self.storage {
            session.set_storage_ctx(plugin_id.to_string(), Arc::clone(store));
        }

        Ok(session)
    }

    /// 从已有对话 ID 恢复 LLM 会话（续聊模式）。
    ///
    /// 与 `create_llm_session` 相比，额外做了两件事：
    /// 1. 从 `ConversationStore` 加载历史消息并回放到 `ConversationTree`；
    /// 2. 将 `StorageCtx` 的 `conversation_id` 设为原对话 ID，确保续聊时
    ///    写盘覆盖原文件，而非创建新文件（避免重复对话条目）。
    ///
    /// - `config_override`: 可选的 `SessionConfig` 覆盖。传 `None` 时使用默认值。
    pub fn resume_llm_session(
        &self,
        plugin_id: &str,
        api_key: &str,
        conversation_id: &str,
        config_override: Option<SessionConfig>,
    ) -> Result<LLMSession> {
        let store = self
            .storage
            .as_ref()
            .ok_or_else(|| anyhow!("storage not configured"))?;

        let conv = store
            .get(conversation_id)
            .ok_or_else(|| anyhow!("conversation '{}' not found", conversation_id))?;

        let url = self.plugin_registry.get_url(plugin_id)?;
        if !url.starts_with("http") {
            return Err(anyhow!("plugin '{}' has invalid URL: {}", plugin_id, url));
        }

        let mut config = config_override.unwrap_or_default();
        config.base_url = url.to_string();
        config.api_key = api_key.to_string();

        let pipeline = ApiPipeline::new(
            Arc::clone(&self.plugin_registry),
            Some(plugin_id.to_string()),
        );

        let mut session = LLMSession::new(
            config,
            pipeline,
            Arc::clone(&self.tool_registry),
        )?;

        // 回放历史消息
        session.preload_history(conv.messages, conv.head);

        // 绑定存储上下文（复用原对话 ID，续聊写盘不创建新文件）
        session.resume_storage_ctx(
            conversation_id.to_string(),
            plugin_id.to_string(),
            conv.meta.created_at,
            Arc::clone(store),
        );

        Ok(session)
    }

    /// 创建 LLM 会话（编排模式）。
    ///
    /// 传入任意 `Box<dyn Orchestrate>` 实现，编排器将嵌入 Session 内部，
    /// 每轮对话开始前自动调用 `assemble`。
    ///
    /// 使用内置策略时，可通过 `DefaultOrchestrator::new(registry)` 构建：
    /// ```rust
    /// let orch = DefaultOrchestrator::new(client.tool_registry().clone())
    ///     .with_whitelist(my_sense.tool_whitelist());
    /// let session = client.create_orchestrated_session(plugin_id, api_key, Box::new(orch), None)?;
    /// ```
    ///
    /// 若需要同时加载 Sense，使用 `create_orchestrated_session_with_sense`（async）。
    ///
    /// - `config_override`: 可选的 `SessionConfig` 覆盖。传 `None` 时使用默认值。
    pub fn create_orchestrated_session(
        &self,
        plugin_id: &str,
        api_key: &str,
        orchestrator: Box<dyn Orchestrate>,
        config_override: Option<SessionConfig>,
    ) -> Result<LLMSession> {
        let url = self.plugin_registry.get_url(plugin_id)?;
        if !url.starts_with("http") {
            return Err(anyhow!("plugin '{}' has invalid URL: {}", plugin_id, url));
        }

        let mut config = config_override.unwrap_or_default();
        config.base_url = url.to_string();
        config.api_key = api_key.to_string();

        let pipeline = ApiPipeline::new(
            Arc::clone(&self.plugin_registry),
            Some(plugin_id.to_string()),
        );

        let mut session = LLMSession::new(
            config,
            pipeline,
            Arc::clone(&self.tool_registry),
        )?;

        session.set_orchestrator(orchestrator);

        if let Some(ref store) = self.storage {
            session.set_storage_ctx(plugin_id.to_string(), Arc::clone(store));
        }

        Ok(session)
    }

    /// 创建 LLM 会话（编排模式，同时加载 Sense）。
    ///
    /// 等价于：
    /// ```rust
    /// let mut session = client.create_orchestrated_session(plugin_id, api_key, orchestrator, config_override)?;
    /// session.load_sense(sense).await?;
    /// ```
    ///
    /// `Sense` 只进入 `load_sense()`（系统提示 + 工具安装），不进入编排器。
    /// 若需要编排器感知工具白名单，在构建 `DefaultOrchestrator` 时显式传入：
    /// ```rust
    /// let orch = DefaultOrchestrator::new(client.tool_registry().clone())
    ///     .with_whitelist(my_sense.tool_whitelist());
    /// client.create_orchestrated_session_with_sense(plugin_id, api_key, my_sense, Box::new(orch), None).await?;
    /// ```
    ///
    /// - `config_override`: 可选的 `SessionConfig` 覆盖。传 `None` 时使用默认值。
    pub async fn create_orchestrated_session_with_sense(
        &self,
        plugin_id: &str,
        api_key: &str,
        sense: impl crate::sense::Sense,
        orchestrator: Box<dyn Orchestrate>,
        config_override: Option<SessionConfig>,
    ) -> Result<LLMSession> {
        let mut session = self.create_orchestrated_session(plugin_id, api_key, orchestrator, config_override)?;
        session.load_sense(sense).await?;
        Ok(session)
    }

    /// 创建 TTS 语音合成会话。
    ///
    /// - `plugin_id`: TTS 插件 ID。
    /// - `api_key`: API 密钥。
    ///
    /// TTSSession 是无状态的，可反复调用 `synthesize`。
    ///
    /// - `config_override`: 可选的 `SessionConfig` 覆盖。传 `None` 时使用 TTS 专用默认值：
    ///   - `request_timeout`: 120s, 其余字段为 0
    pub fn create_tts_session(
        &self,
        plugin_id: &str,
        api_key: &str,
        config_override: Option<SessionConfig>,
    ) -> Result<TTSSession> {
        let url = self.plugin_registry.get_url(plugin_id)?;
        if !url.starts_with("http") {
            return Err(anyhow!("plugin '{}' has invalid URL: {}", plugin_id, url));
        }

        let mut config = config_override.unwrap_or_else(|| SessionConfig {
            base_url: url.to_string(),
            api_key: api_key.to_string(),
            event_buffer: 0,          // TTS 不用事件流
            request_timeout: 120,     // TTS 合成可能较慢
            max_tool_rounds: 0,
            max_line_bytes: 0,
        });
        // 如果调用方传入了自定义 config，仍需覆盖 url 和 api_key
        config.base_url = url.to_string();
        config.api_key = api_key.to_string();

        let pipeline = ApiPipeline::new(
            Arc::clone(&self.plugin_registry),
            Some(plugin_id.to_string()),
        );

        TTSSession::new(config, pipeline)
    }

    // ── 对话历史管理 ──

    /// 列出所有已保存对话的元信息（不含消息体），按 updated_at 降序。
    ///
    /// 未配置 `storage_path` 时返回空列表。
    pub fn ai_list_conversations(&self) -> Vec<ConversationMeta> {
        self.storage.as_ref().map_or_else(Vec::new, |s| s.list())
    }

    /// 获取指定对话的完整内容（含消息列表）。
    ///
    /// 未找到或未配置存储时返回 `None`。
    pub fn ai_get_conversation(&self, id: &str) -> Option<StoredConversation> {
        self.storage.as_ref()?.get(id)
    }

    /// 删除指定对话文件。
    pub fn ai_delete_conversation(&self, id: &str) -> Result<()> {
        self.storage
            .as_ref()
            .ok_or_else(|| anyhow!("storage not configured"))?
            .delete(id)
    }

    /// 重命名对话（修改标题），同时更新 updated_at。
    pub fn ai_rename_conversation(&self, id: &str, title: String) -> Result<()> {
        self.storage
            .as_ref()
            .ok_or_else(|| anyhow!("storage not configured"))?
            .rename(id, title)
    }

    /// 创建图像生成会话。
    ///
    /// - `config_override`: 可选的 `SessionConfig` 覆盖。传 `None` 时使用图像生成专用默认值：
    ///   - `request_timeout`: 180s, 其余字段为 0
    pub fn create_image_session(
        &self,
        plugin_id: &str,
        api_key: &str,
        config_override: Option<SessionConfig>,
    ) -> Result<ImageSession> {
        let url = self.plugin_registry.get_url(plugin_id)?;
        if !url.starts_with("http") {
            return Err(anyhow!("plugin '{}' has invalid URL: {}", plugin_id, url));
        }

        let mut config = config_override.unwrap_or_else(|| SessionConfig {
            base_url: url.to_string(),
            api_key: api_key.to_string(),
            event_buffer: 0,
            request_timeout: 180,  // 图像生成较慢
            max_tool_rounds: 0,
            max_line_bytes: 0,
        });
        // 如果调用方传入了自定义 config，仍需覆盖 url 和 api_key
        config.base_url = url.to_string();
        config.api_key = api_key.to_string();

        let pipeline = ApiPipeline::new(
            Arc::clone(&self.plugin_registry),
            Some(plugin_id.to_string()),
        );

        ImageSession::new(config, pipeline)
    }
}
