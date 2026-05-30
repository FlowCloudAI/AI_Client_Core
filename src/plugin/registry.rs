use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::Result;
use wasmtime::{Config, Engine};

use crate::error::{ClientError, ErrorCode};
use crate::plugin::host::HostState;
use crate::plugin::pool::{MapperPool, PooledMapper};
use crate::plugin::types::{PluginKind, PluginMeta};
use wasmtime::component::{Component, Linker};

// ─────────────────────── 注册表状态 ────────────────────────

/// PluginRegistry 的可变内部状态，由 Mutex 保护。
struct RegistryState {
    plugins: HashMap<String, PluginMeta>,
    modules: HashMap<String, Component>,
    pools: HashMap<String, Arc<MapperPool>>,
    ref_counts: HashMap<String, usize>,
}

// ─────────────────────── 插件注册中心 ──────────────────────

/// 插件注册中心。
///
/// 职责：
/// 1. 持有 wasmtime `Engine`（全局唯一，内部 Arc，clone 廉价）。
/// 2. 持有每个插件的编译好的 `Module` + 元数据。
/// 3. 管理 per-plugin 的 `MapperPool`，对外提供 `acquire` 接口。
/// 4. 跟踪插件引用计数，支持安全卸载。
///
/// 所有权模型：
/// - `FlowCloudAIClient` 通过 `Arc<PluginRegistry>` 持有。
/// - 各 Session 持有 `Arc<PluginRegistry>` 的 clone。
/// - Session 通过 `acquire()` 借出 `PooledMapper`，用完自动归还。
/// - `PluginRegistry` 本身是 `Sync`（内部由 Mutex 保护），可安全跨线程共享。
pub struct PluginRegistry {
    /// 可变内部状态（plugins / modules / pools），由 Mutex 保护。
    state: Mutex<RegistryState>,

    /// wasmtime 引擎（共享，clone 廉价）
    engine: Engine,

    /// wasmtime 链接器（定义 host 函数）
    linker: Linker<HostState>,

    /// 每个池的最大空闲实例数
    max_idle_per_pool: usize,
}

impl PluginRegistry {
    // ── 构建 ──

    fn state(&self) -> Result<MutexGuard<'_, RegistryState>> {
        self.state.lock().map_err(|e| {
            ClientError::new(ErrorCode::CoreClientInternalError, "插件注册表状态锁中毒")
                .with_kv("source", e.to_string())
                .into()
        })
    }

    fn plugin_not_loaded(plugin_id: &str) -> ClientError {
        ClientError::new(
            ErrorCode::PluginNotLoaded,
            format!("已选择插件 '{}' 但未加载", plugin_id),
        )
        .with_kv("plugin_id", plugin_id.to_string())
    }

    fn increment_ref_in_state(state: &mut RegistryState, plugin_id: &str) {
        *state.ref_counts.entry(plugin_id.to_string()).or_insert(0) += 1;
    }

    fn decrement_ref_in_state(state: &mut RegistryState, plugin_id: &str) {
        if let Some(count) = state.ref_counts.get_mut(plugin_id) {
            if *count > 1 {
                *count -= 1;
            } else {
                state.ref_counts.remove(plugin_id);
            }
        }
    }

    /// 空 registry，无插件。仅未指定插件的管道会走 passthrough。
    pub fn empty() -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).map_err(|e| {
            ClientError::new(ErrorCode::CoreClientInitFailed, "创建 WebAssembly 引擎失败")
                .with_kv("source", e.to_string())
        })?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|e| {
            ClientError::new(ErrorCode::CoreClientInitFailed, "向 linker 注册 WASI 失败")
                .with_kv("source", e.to_string())
        })?;
        Ok(Self {
            state: Mutex::new(RegistryState {
                plugins: HashMap::new(),
                modules: HashMap::new(),
                pools: HashMap::new(),
                ref_counts: HashMap::new(),
            }),
            engine,
            linker,
            max_idle_per_pool: 0,
        })
    }

    /// 从 PluginManager 的扫描结果构建 Registry。
    ///
    /// 此阶段只编译 wasm module，不创建任何 Store 实例。
    /// 实例在首次 `acquire` 时按需创建。
    pub fn build(
        engine: Engine,
        linker: Linker<HostState>,
        plugin_metas: HashMap<String, PluginMeta>,
        max_idle_per_pool: usize,
    ) -> Result<Self> {
        let mut modules = HashMap::new();

        for (id, meta) in &plugin_metas {
            let wasm_bytes = {
                let file = std::fs::File::open(&meta.fcplug_path).map_err(|e| {
                    ClientError::new(ErrorCode::FsOpenFailed, "无法打开插件包")
                        .with_kv("plugin_id", id.clone())
                        .with_kv("source", e.to_string())
                })?;
                let mut archive = zip::ZipArchive::new(file).map_err(|e| {
                    ClientError::new(ErrorCode::PluginLoadFailed, "插件包不是合法 ZIP")
                        .with_kv("plugin_id", id.clone())
                        .with_kv("source", e.to_string())
                })?;
                let mut entry = archive.by_name("plugin.wasm").map_err(|_| {
                    ClientError::new(ErrorCode::PluginLoadFailed, "插件包缺少 plugin.wasm")
                        .with_kv("plugin_id", id.clone())
                })?;
                let mut buf = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut buf).map_err(|e| {
                    ClientError::new(ErrorCode::PluginLoadFailed, "读取 plugin.wasm 失败")
                        .with_kv("plugin_id", id.clone())
                        .with_kv("source", e.to_string())
                })?;
                buf
            };

            let module = Component::from_binary(&engine, &wasm_bytes).map_err(|e| {
                ClientError::new(ErrorCode::PluginLoadFailed, "Wasm 组件编译失败")
                    .with_kv("plugin_id", id.clone())
                    .with_kv("source", e.to_string())
            })?;
            modules.insert(id.clone(), module);
        }

        Ok(Self {
            state: Mutex::new(RegistryState {
                plugins: plugin_metas,
                modules,
                pools: HashMap::new(),
                ref_counts: HashMap::new(),
            }),
            engine,
            linker,
            max_idle_per_pool,
        })
    }

    // ── 插件加载 ──

    /// 激活指定插件，为其创建实例池。
    ///
    /// 只有 `load` 过的插件才能被 `acquire`。
    /// 可多次调用同一 id，幂等。
    pub fn load(&self, id: &str) -> Result<()> {
        let mut state = self.state()?;
        if state.pools.contains_key(id) {
            return Ok(()); // 已加载，幂等
        }

        let module = state
            .modules
            .get(id)
            .ok_or_else(|| {
                ClientError::new(
                    ErrorCode::PluginNotFound,
                    format!("插件 '{}' 不在注册表中", id),
                )
                .with_kv("plugin_id", id.to_string())
            })?
            .clone();

        let pool = Arc::new(MapperPool::new(
            self.engine.clone(),
            module,
            self.linker.clone(),
            self.max_idle_per_pool,
        ));

        state.pools.insert(id.to_string(), pool);
        Ok(())
    }

    /// 卸载指定插件，销毁其实例池及所有空闲实例。
    ///
    /// 如果插件仍被 session 引用（ref_count > 0），返回错误。
    ///
    /// 注意：调用方应确保没有活跃的 PooledMapper 引用该 pool，
    /// 否则会导致悬垂引用。实践中应先停止所有使用该插件的 session。
    pub fn unload(&self, id: &str) -> Result<()> {
        let mut state = self.state()?;

        let count = state.ref_counts.get(id).copied().unwrap_or(0);
        if count > 0 {
            return Err(ClientError::new(
                ErrorCode::PluginUnloadForbidden,
                format!("插件 '{}' 仍被 {} 个会话引用，无法卸载", id, count),
            )
            .with_kv("plugin_id", id.to_string())
            .with_kv("ref_count", count)
            .into());
        }

        // 移除 pool、module 和 meta
        state.pools.remove(id);
        state.modules.remove(id);
        state.plugins.remove(id);
        state.ref_counts.remove(id);

        Ok(())
    }

    /// 插件是否已加载（有活跃的实例池）。
    pub fn try_is_loaded(&self, id: &str) -> Result<bool> {
        Ok(self.state()?.pools.contains_key(id))
    }

    /// 插件是否已加载（有活跃的实例池）。
    ///
    /// 兼容旧 API：锁异常时保守返回 false，严格路径请使用 `try_is_loaded`。
    pub fn is_loaded(&self, id: &str) -> bool {
        self.try_is_loaded(id).unwrap_or(false)
    }

    // ── 实例借出 ──

    /// 从指定插件的池中借出一个 mapper。
    ///
    /// - 池中有空闲实例 → 复用（纳秒级）
    /// - 池为空 → 创建新 Store + 实例化 wasm（微秒级）
    /// - drop PooledMapper → 自动归还池中
    ///
    /// 如果插件未加载，返回 Err。
    pub fn acquire(&self, plugin_id: &str) -> Result<PooledMapper> {
        let state = self.state()?;
        let pool = state
            .pools
            .get(plugin_id)
            .ok_or_else(|| {
                ClientError::new(
                    ErrorCode::PluginNotLoaded,
                    format!("插件 '{}' 未加载", plugin_id),
                )
                .with_kv("plugin_id", plugin_id.to_string())
            })?
            .clone();
        drop(state);
        pool.acquire()
    }

    // ── 查询 ──

    /// 获取插件的 API 端点 URL。
    pub fn get_url(&self, plugin_id: &str) -> Result<String> {
        let state = self.state()?;
        state
            .plugins
            .get(plugin_id)
            .map(|meta| meta.url.clone())
            .ok_or_else(|| {
                ClientError::new(
                    ErrorCode::PluginNotFound,
                    format!("插件 '{}' 不存在", plugin_id),
                )
                .with_kv("plugin_id", plugin_id.to_string())
                .into()
            })
    }

    /// 获取插件元数据。
    pub fn try_get_meta(&self, plugin_id: &str) -> Result<Option<PluginMeta>> {
        Ok(self.state()?.plugins.get(plugin_id).cloned())
    }

    /// 获取插件元数据。
    ///
    /// 兼容旧 API：锁异常时返回 None，严格路径请使用 `try_get_meta`。
    pub fn get_meta(&self, plugin_id: &str) -> Option<PluginMeta> {
        self.try_get_meta(plugin_id).ok().flatten()
    }

    /// 获取所有插件元数据列表。
    pub fn try_list_plugins(&self) -> Result<Vec<PluginMeta>> {
        Ok(self.state()?.plugins.values().cloned().collect())
    }

    /// 获取所有插件元数据列表。
    ///
    /// 兼容旧 API：锁异常时返回空列表，严格路径请使用 `try_list_plugins`。
    pub fn list_plugins(&self) -> Vec<PluginMeta> {
        self.try_list_plugins().unwrap_or_default()
    }

    /// 按类型筛选插件。
    pub fn try_list_by_kind(&self, kind: PluginKind) -> Result<Vec<PluginMeta>> {
        Ok(self
            .state()?
            .plugins
            .values()
            .filter(|meta| meta.kind == kind)
            .cloned()
            .collect())
    }

    /// 按类型筛选插件。
    ///
    /// 兼容旧 API：锁异常时返回空列表，严格路径请使用 `try_list_by_kind`。
    pub fn list_by_kind(&self, kind: PluginKind) -> Vec<PluginMeta> {
        self.try_list_by_kind(kind).unwrap_or_default()
    }

    /// 获取所有已加载插件的池状态（诊断用）。
    pub fn try_pool_stats(&self) -> Result<HashMap<String, usize>> {
        Ok(self
            .state()?
            .pools
            .iter()
            .map(|(id, pool)| (id.clone(), pool.idle_count()))
            .collect())
    }

    /// 获取所有已加载插件的池状态（诊断用）。
    ///
    /// 兼容旧 API：锁异常时返回空 map，严格路径请使用 `try_pool_stats`。
    pub fn pool_stats(&self) -> HashMap<String, usize> {
        self.try_pool_stats().unwrap_or_default()
    }

    // ── 引用计数管理 ──

    /// 增加插件引用计数（session 创建时调用）。
    pub fn try_increment_ref(&self, plugin_id: &str) -> Result<()> {
        let mut state = self.state()?;
        Self::increment_ref_in_state(&mut state, plugin_id);
        Ok(())
    }

    /// 仅当插件当前已加载时增加引用计数。
    pub fn try_increment_loaded_ref(&self, plugin_id: &str) -> Result<()> {
        let mut state = self.state()?;
        if !state.pools.contains_key(plugin_id) {
            return Err(Self::plugin_not_loaded(plugin_id).into());
        }
        Self::increment_ref_in_state(&mut state, plugin_id);
        Ok(())
    }

    /// 在同一把 registry state 锁下切换引用计数，并可要求新插件已加载。
    pub fn try_switch_ref(
        &self,
        old_plugin_id: Option<&str>,
        new_plugin_id: Option<&str>,
        require_new_loaded: bool,
    ) -> Result<()> {
        let mut state = self.state()?;

        if let Some(new_id) = new_plugin_id {
            if require_new_loaded && !state.pools.contains_key(new_id) {
                return Err(Self::plugin_not_loaded(new_id).into());
            }
        }

        if let Some(old_id) = old_plugin_id {
            Self::decrement_ref_in_state(&mut state, old_id);
        }
        if let Some(new_id) = new_plugin_id {
            Self::increment_ref_in_state(&mut state, new_id);
        }

        Ok(())
    }

    /// 增加插件引用计数（session 创建时调用）。
    ///
    /// 兼容旧 API：锁异常时忽略，严格路径请使用 `try_increment_ref`。
    pub fn increment_ref(&self, plugin_id: &str) {
        let _ = self.try_increment_ref(plugin_id);
    }

    /// 减少插件引用计数（session 销毁时调用）。
    pub fn try_decrement_ref(&self, plugin_id: &str) -> Result<()> {
        let mut state = self.state()?;
        Self::decrement_ref_in_state(&mut state, plugin_id);
        Ok(())
    }

    /// 减少插件引用计数（session 销毁时调用）。
    ///
    /// 兼容旧 API：锁异常时忽略，严格路径请使用 `try_decrement_ref`。
    pub fn decrement_ref(&self, plugin_id: &str) {
        let _ = self.try_decrement_ref(plugin_id);
    }

    /// 获取插件引用计数。
    pub fn try_get_ref_count(&self, plugin_id: &str) -> Result<usize> {
        Ok(self
            .state()?
            .ref_counts
            .get(plugin_id)
            .copied()
            .unwrap_or(0))
    }

    /// 获取插件引用计数。
    ///
    /// 兼容旧 API：锁异常时返回 `usize::MAX`，代表未知且应视为 busy。
    pub fn get_ref_count(&self, plugin_id: &str) -> usize {
        self.try_get_ref_count(plugin_id).unwrap_or(usize::MAX)
    }

    // ── 动态模块加载 ──

    /// 动态添加新插件模块（用于 install_plugin_from_path）。
    ///
    /// 此方法编译 wasm 并添加到 modules HashMap，但不创建 pool。
    /// 需要后续调用 `load()` 来激活插件。
    pub fn add_module(&self, id: String, meta: PluginMeta, wasm_bytes: &[u8]) -> Result<()> {
        let mut state = self.state()?;
        // 检查 ID 唯一性
        if state.plugins.contains_key(&id) {
            return Err(ClientError::new(
                ErrorCode::PluginAlreadyExists,
                format!("插件 '{}' 已存在", id),
            )
            .with_kv("plugin_id", id.clone())
            .into());
        }

        // 编译 wasm 模块
        let module = Component::from_binary(&self.engine, wasm_bytes).map_err(|e| {
            ClientError::new(ErrorCode::PluginLoadFailed, "Wasm 组件编译失败")
                .with_kv("plugin_id", id.clone())
                .with_kv("source", e.to_string())
        })?;

        // 插入元数据和模块
        state.plugins.insert(id.clone(), meta);
        state.modules.insert(id, module);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    #[test]
    fn state_poison_try_api_returns_error_legacy_api_is_safe() {
        let registry = PluginRegistry::empty().unwrap();
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _guard = registry.state.lock().unwrap();
            panic!("制造 registry state poison");
        }));

        assert!(registry.try_list_plugins().is_err());
        assert!(registry.try_get_ref_count("p").is_err());
        assert!(registry.list_plugins().is_empty());
        assert!(!registry.is_loaded("p"));
        assert_eq!(registry.get_ref_count("p"), usize::MAX);
    }

    #[test]
    fn loaded_ref_increment_rejects_unloaded_plugin() {
        let registry = PluginRegistry::empty().unwrap();

        assert!(registry.try_increment_loaded_ref("missing").is_err());
        assert_eq!(registry.try_get_ref_count("missing").unwrap(), 0);
    }

    #[test]
    fn unload_observes_ref_count_under_registry_state_lock() {
        let registry = PluginRegistry::empty().unwrap();
        registry.try_increment_ref("p").unwrap();

        let err = registry.unload("p").unwrap_err();
        assert!(
            err.to_string().contains("PLUGIN_UNLOAD_FORBIDDEN"),
            "unexpected error: {err:#}"
        );
    }
}

// ── Send + Sync 安全性说明 ──
//
// PluginRegistry 是 Sync：
// - HashMap<String, PluginMeta>: Send + Sync（线程安全）
// - HashMap<String, MapperPool>: MapperPool 内部用 std::sync::Mutex，是 Sync
// - Engine: 内部 Arc，Send + Sync
// - Linker: Send + Sync（如果你的 wasmtime 版本支持）
// - HashMap<String, Module>: Module 内部 Arc，Send + Sync
//
// 因此 Arc<PluginRegistry> 可安全地在多线程间共享。
