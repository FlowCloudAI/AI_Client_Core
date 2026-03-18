use std::collections::HashMap;

use anyhow::{anyhow, Result};
use wasmtime::Engine;

use crate::plugin::host::HostState;
use crate::plugin::pool::{MapperPool, PooledMapper};
use crate::plugin::types::{PluginKind, PluginMeta};
use wasmtime::component::{Component, Linker};

// ─────────────────────── PluginRegistry ─────────────────────

/// 插件注册中心。
///
/// 职责：
/// 1. 持有 wasmtime `Engine`（全局唯一，内部 Arc，clone 廉价）。
/// 2. 持有每个插件的编译好的 `Module` + 元数据。
/// 3. 管理 per-plugin 的 `MapperPool`，对外提供 `acquire` 接口。
///
/// 所有权模型：
/// - `FlowCloudAIClient` 通过 `Arc<PluginRegistry>` 持有。
/// - 各 Session 持有 `Arc<PluginRegistry>` 的 clone。
/// - Session 通过 `acquire()` 借出 `PooledMapper`，用完自动归还。
/// - `PluginRegistry` 本身是 `Sync`（内部只有 Mutex<Vec>），可安全跨线程共享。
pub struct PluginRegistry {
    /// 插件元数据（id → meta）
    plugins: HashMap<String, PluginMeta>,

    /// 按 plugin_id 索引的实例池
    /// 只有已加载（load）的插件才会有对应的 pool
    pools: HashMap<String, MapperPool>,

    /// wasmtime 引擎（共享，clone 廉价）
    engine: Engine,

    /// wasmtime 链接器（定义 host 函数）
    linker: Linker<HostState>,

    /// 已编译的 wasm 模块（编译一次，实例化多次）
    modules: HashMap<String, Component>,

    /// 每个池的最大空闲实例数
    max_idle_per_pool: usize,
}

impl PluginRegistry {
    // ── 构建 ──

    /// 空 registry，无插件。所有 acquire 都会走 passthrough。
    pub fn empty() -> Result<Self> {
        let engine = Engine::default();
        let linker = Linker::new(&engine);
        Ok(Self {
            plugins: HashMap::new(),
            pools: HashMap::new(),
            engine,
            linker,
            modules: HashMap::new(),
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
            plugins: plugin_metas,
            pools: HashMap::new(),
            engine,
            linker,
            modules,
            max_idle_per_pool,
        })
    }

    // ── 插件加载 ──

    /// 激活指定插件，为其创建实例池。
    ///
    /// 只有 `load` 过的插件才能被 `acquire`。
    /// 可多次调用同一 id，幂等。
    pub fn load(&mut self, id: &str) -> Result<()> {
        if self.pools.contains_key(id) {
            return Ok(()); // 已加载，幂等
        }

        let module = self
            .modules
            .get(id)
            .ok_or_else(|| anyhow!("plugin '{}' not found in registry", id))?
            .clone(); // Module clone 廉价（内部 Arc）

        let pool = MapperPool::new(
            self.engine.clone(),
            module,
            self.linker.clone(), // Linker clone — 如果你的版本不支持 clone，需改为重建
            self.max_idle_per_pool,
        );

        self.pools.insert(id.to_string(), pool);
        Ok(())
    }

    /// 卸载指定插件，销毁其实例池及所有空闲实例。
    ///
    /// 已借出的 `PooledMapper` 不受影响（它持有 pool 的引用，
    /// 但因为我们用 HashMap::remove，归还时 pool 已不存在，
    /// mapper 会直接 drop）。
    ///
    /// 注意：卸载后需确保没有活跃的 PooledMapper 引用该 pool，
    /// 否则会导致悬垂引用。实践中应先停止所有使用该插件的 session。
    pub fn unload(&mut self, id: &str) {
        self.pools.remove(id);
    }

    /// 插件是否已加载（有活跃的实例池）。
    pub fn is_loaded(&self, id: &str) -> bool {
        self.pools.contains_key(id)
    }

    // ── 实例借出 ──

    /// 从指定插件的池中借出一个 mapper。
    ///
    /// - 池中有空闲实例 → 复用（纳秒级）
    /// - 池为空 → 创建新 Store + 实例化 wasm（微秒级）
    /// - drop PooledMapper → 自动归还池中
    ///
    /// 如果插件未加载，返回 Err。
    /// 如果不需要插件映射，使用 `acquire_or_passthrough` 代替。
    pub fn acquire(&self, plugin_id: &str) -> Result<PooledMapper<'_>> {
        self.pools
            .get(plugin_id)
            .ok_or_else(|| anyhow!("plugin '{}' not loaded", plugin_id))?
            .acquire()
    }

    // ── 查询 ──

    /// 获取插件的 API 端点 URL。
    pub fn get_url(&self, plugin_id: &str) -> Result<&str> {
        self.plugins
            .get(plugin_id)
            .map(|meta| meta.url.as_str())
            .ok_or_else(|| anyhow!("plugin '{}' not found", plugin_id))
    }

    /// 获取插件元数据。
    pub fn get_meta(&self, plugin_id: &str) -> Option<&PluginMeta> {
        self.plugins.get(plugin_id)
    }

    /// 获取所有插件元数据。
    pub fn list_plugins(&self) -> &HashMap<String, PluginMeta> {
        &self.plugins
    }

    /// 按类型筛选插件。
    pub fn list_by_kind(&self, kind: PluginKind) -> Vec<(&String, &PluginMeta)> {
        self.plugins
            .iter()
            .filter(|(_, meta)| meta.kind == kind)
            .collect()
    }

    /// 获取所有已加载插件的池状态（诊断用）。
    pub fn pool_stats(&self) -> HashMap<&str, usize> {
        self.pools
            .iter()
            .map(|(id, pool)| (id.as_str(), pool.idle_count()))
            .collect()
    }
}

// ── Send + Sync 安全性说明 ──
//
// PluginRegistry 是 Sync：
// - HashMap<String, PluginMeta>: Send + Sync
// - HashMap<String, MapperPool>: MapperPool 内部用 std::sync::Mutex，是 Sync
// - Engine: 内部 Arc，Send + Sync
// - Linker: Send + Sync（如果你的 wasmtime 版本支持）
// - HashMap<String, Module>: Module 内部 Arc，Send + Sync
//
// 因此 Arc<PluginRegistry> 可安全地在多线程间共享。