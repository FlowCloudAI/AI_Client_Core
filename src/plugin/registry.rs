use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use wasmtime::Engine;

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

    /// 插件引用计数（id → 引用数）
    /// 用于追踪有多少 session 正在使用该插件
    ref_counts: Arc<Mutex<HashMap<String, usize>>>,
}

impl PluginRegistry {
    // ── 构建 ──

    /// 空 registry，无插件。所有 acquire 都会走 passthrough。
    pub fn empty() -> Result<Self> {
        let engine = Engine::default();
        let linker = Linker::new(&engine);
        Ok(Self {
            state: Mutex::new(RegistryState {
                plugins: HashMap::new(),
                modules: HashMap::new(),
                pools: HashMap::new(),
            }),
            engine,
            linker,
            max_idle_per_pool: 0,
            ref_counts: Arc::new(Mutex::new(HashMap::new())),
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
                let file = std::fs::File::open(&meta.fcplug_path)
                    .map_err(|e| anyhow!("cannot open plugin '{}': {}", id, e))?;
                let mut archive = zip::ZipArchive::new(file)
                    .map_err(|e| anyhow!("cannot read zip for plugin '{}': {}", id, e))?;
                let mut entry = archive.by_name("plugin.wasm")
                    .map_err(|_| anyhow!("plugin.wasm not found in '{}'", id))?;
                let mut buf = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut buf)?;
                buf
            };

            let module = Component::from_binary(&engine, &wasm_bytes)
                .map_err(|e| anyhow!("failed to compile wasm for plugin '{}': {}", id, e))?;
            modules.insert(id.clone(), module);
        }

        Ok(Self {
            state: Mutex::new(RegistryState {
                plugins: plugin_metas,
                modules,
                pools: HashMap::new(),
            }),
            engine,
            linker,
            max_idle_per_pool,
            ref_counts: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    // ── 插件加载 ──

    /// 激活指定插件，为其创建实例池。
    ///
    /// 只有 `load` 过的插件才能被 `acquire`。
    /// 可多次调用同一 id，幂等。
    pub fn load(&self, id: &str) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.pools.contains_key(id) {
            return Ok(()); // 已加载，幂等
        }

        let module = state
            .modules
            .get(id)
            .ok_or_else(|| anyhow!("plugin '{}' not found in registry", id))?
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
        // 检查引用计数
        let ref_counts = self.ref_counts.lock()
            .map_err(|e| anyhow!("failed to lock ref_counts: {}", e))?;

        if let Some(&count) = ref_counts.get(id) {
            if count > 0 {
                return Err(anyhow!(
                    "cannot unload plugin '{}': still in use by {} session(s)",
                    id, count
                ));
            }
        }
        drop(ref_counts);

        // 移除 pool、module 和 meta
        let mut state = self.state.lock().unwrap();
        state.pools.remove(id);
        state.modules.remove(id);
        state.plugins.remove(id);

        Ok(())
    }

    /// 插件是否已加载（有活跃的实例池）。
    pub fn is_loaded(&self, id: &str) -> bool {
        self.state.lock().unwrap().pools.contains_key(id)
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
        let state = self.state.lock().unwrap();
        let pool = state
            .pools
            .get(plugin_id)
            .ok_or_else(|| anyhow!("plugin '{}' not loaded", plugin_id))?
            .clone();
        drop(state);
        pool.acquire()
    }

    // ── 查询 ──

    /// 获取插件的 API 端点 URL。
    pub fn get_url(&self, plugin_id: &str) -> Result<String> {
        let state = self.state.lock().unwrap();
        state
            .plugins
            .get(plugin_id)
            .map(|meta| meta.url.clone())
            .ok_or_else(|| anyhow!("plugin '{}' not found", plugin_id))
    }

    /// 获取插件元数据。
    pub fn get_meta(&self, plugin_id: &str) -> Option<PluginMeta> {
        self.state.lock().unwrap().plugins.get(plugin_id).cloned()
    }

    /// 获取所有插件元数据列表。
    pub fn list_plugins(&self) -> Vec<PluginMeta> {
        self.state.lock().unwrap().plugins.values().cloned().collect()
    }

    /// 按类型筛选插件。
    pub fn list_by_kind(&self, kind: PluginKind) -> Vec<PluginMeta> {
        self.state
            .lock()
            .unwrap()
            .plugins
            .values()
            .filter(|meta| meta.kind == kind)
            .cloned()
            .collect()
    }

    /// 获取所有已加载插件的池状态（诊断用）。
    pub fn pool_stats(&self) -> HashMap<String, usize> {
        self.state
            .lock()
            .unwrap()
            .pools
            .iter()
            .map(|(id, pool)| (id.clone(), pool.idle_count()))
            .collect()
    }

    // ── 引用计数管理 ──

    /// 增加插件引用计数（session 创建时调用）。
    pub fn increment_ref(&self, plugin_id: &str) {
        if let Ok(mut counts) = self.ref_counts.lock() {
            *counts.entry(plugin_id.to_string()).or_insert(0) += 1;
        }
    }

    /// 减少插件引用计数（session 销毁时调用）。
    pub fn decrement_ref(&self, plugin_id: &str) {
        if let Ok(mut counts) = self.ref_counts.lock() {
            if let Some(count) = counts.get_mut(plugin_id) {
                if *count > 0 {
                    *count -= 1;
                }
            }
        }
    }

    /// 获取插件引用计数。
    pub fn get_ref_count(&self, plugin_id: &str) -> usize {
        self.ref_counts
            .lock()
            .ok()
            .and_then(|counts| counts.get(plugin_id).copied())
            .unwrap_or(0)
    }

    // ── 动态模块加载 ──

    /// 动态添加新插件模块（用于 install_plugin_from_path）。
    ///
    /// 此方法编译 wasm 并添加到 modules HashMap，但不创建 pool。
    /// 需要后续调用 `load()` 来激活插件。
    pub fn add_module(
        &self,
        id: String,
        meta: PluginMeta,
        wasm_bytes: &[u8],
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        // 检查 ID 唯一性
        if state.plugins.contains_key(&id) {
            return Err(anyhow!("plugin '{}' already exists", id));
        }

        // 编译 wasm 模块
        let module = Component::from_binary(&self.engine, wasm_bytes)
            .map_err(|e| anyhow!("failed to compile wasm for plugin '{}': {}", id, e))?;

        // 插入元数据和模块
        state.plugins.insert(id.clone(), meta);
        state.modules.insert(id, module);

        Ok(())
    }

    /// 获取引用计数的 Arc（供 Session 持有）。
    pub fn ref_counts(&self) -> Arc<Mutex<HashMap<String, usize>>> {
        Arc::clone(&self.ref_counts)
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