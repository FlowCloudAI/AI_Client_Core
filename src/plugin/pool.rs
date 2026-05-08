use std::sync::{Arc, Mutex};

use crate::plugin::bindings::plugin_bindings::Api;
use crate::plugin::host::HostState;
use crate::plugin::mapper::{ApiMapper, WasmMapper};
use anyhow::Result;
use wasmtime::component::ResourceTable;
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store};
use wasmtime_wasi::WasiCtxBuilder;

// ─────────────────────── 映射器池 ─────────────────────────

/// 单个插件的 WasmMapper 实例池。
///
/// 设计要点：
/// - `Engine` 和 `Module` 是重量级资源（JIT 编译缓存），只存一份。
/// - `Store` 是轻量级运行态（wasm 实例内存），按需创建，用完归还。
/// - `Mutex` 只保护 `Vec::push / pop`（纳秒级），wasm 计算本身无锁并行。
pub struct MapperPool {
    engine: Engine,
    component: Component,           // 改名 module → component
    linker: Linker<HostState>,      // component::Linker（组件链接器）
    idle: Mutex<Vec<Box<dyn ApiMapper + Send>>>,
    max_idle: usize,
}

impl MapperPool {
    /// 创建实例池。
    ///
    /// - `max_idle`: 池中最多保留的空闲实例数。
    ///   超出部分在归还时直接 drop，释放 Store 内存。
    ///   建议值 = 预期并发 session 数。
    pub fn new(
        engine: Engine,
        component: Component,
        linker: Linker<HostState>,
        max_idle: usize,
    ) -> Self {
        Self {
            engine,
            component,
            linker,
            idle: Mutex::new(Vec::with_capacity(max_idle)),
            max_idle,
        }
    }

    /// 从池中获取一个 mapper 实例。
    ///
    /// 池中有空闲实例则复用，否则即时创建。
    /// 返回的 `PooledMapper` 在 drop 时自动归还。
    pub fn acquire(self: &Arc<Self>) -> Result<PooledMapper> {
        let mapper = {
            match self.idle.lock() {
                Ok(mut guard) => guard.pop(),
                Err(poisoned) => {
                    poisoned.into_inner().pop()
                },
            }
        };

        let mapper = match mapper {
            Some(m) => m,
            None => self.instantiate()?,
        };

        Ok(PooledMapper {
            mapper: Some(mapper),
            pool: Arc::clone(self),
        })
    }

    /// 当前池中空闲实例数（诊断用）。
    pub fn idle_count(&self) -> usize {
        match self.idle.lock() {
            Ok(idle) => idle.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    // ── 内部方法 ──

    /// 创建一个全新的 wasm 实例。
    fn instantiate(&self) -> Result<Box<dyn ApiMapper + Send>> {
        let mut store = Store::new(&self.engine, HostState {
            table: ResourceTable::new(),
            wasi: WasiCtxBuilder::new().build(),
        });
        let api = Api::instantiate(&mut store, &self.component, &self.linker)?;

        Ok(Box::new(WasmMapper { store, api }))
    }

    /// 归还实例到池中（由 PooledMapper::drop 调用）。
    fn release(&self, mapper: Box<dyn ApiMapper + Send>) {
        let mut idle = match self.idle.lock() {
            Ok(idle) => idle,
            Err(poisoned) => poisoned.into_inner(),
        };
        if idle.len() < self.max_idle {
            idle.push(mapper);
        }
        // 超出 max_idle → mapper 直接 drop，释放 Store 内存
    }
}

// ─────────────────────── 池化映射器 ────────────────────────

/// RAII 守卫：持有从池中借出的 mapper，drop 时自动归还。
///
/// 通过 `DerefMut` 透明访问内部的 `dyn ApiMapper`。
pub struct PooledMapper {
    mapper: Option<Box<dyn ApiMapper + Send>>,
    pool: Arc<MapperPool>,
}

impl Drop for PooledMapper {
    fn drop(&mut self) {
        if let Some(mapper) = self.mapper.take() {
            self.pool.release(mapper);
        }
    }
}

impl std::ops::Deref for PooledMapper {
    type Target = dyn ApiMapper + Send;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.mapper
            .as_ref()
            .expect("PooledMapper 在 mapper 被取走后仍被使用")
            .as_ref()
    }
}

impl std::ops::DerefMut for PooledMapper {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.mapper
            .as_mut()
            .expect("PooledMapper 在 mapper 被取走后仍被使用")
            .as_mut()
    }
}

impl ApiMapper for PooledMapper {
    fn map_request(&mut self, json: &str) -> Result<String> {
        (**self).map_request(json)
    }
    fn map_response(&mut self, json: &str) -> Result<String> {
        (**self).map_response(json)
    }
    fn map_stream_line(&mut self, line: &str) -> Result<String> {
        (**self).map_stream_line(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    struct TestMapper;

    impl ApiMapper for TestMapper {
        fn map_request(&mut self, json: &str) -> Result<String> {
            Ok(json.to_string())
        }

        fn map_response(&mut self, json: &str) -> Result<String> {
            Ok(json.to_string())
        }

        fn map_stream_line(&mut self, line: &str) -> Result<String> {
            Ok(line.to_string())
        }
    }

    fn test_pool(max_idle: usize) -> MapperPool {
        let engine = Engine::default();
        let component = Component::new(&engine, "(component)").unwrap();
        let linker = Linker::new(&engine);
        MapperPool::new(engine, component, linker, max_idle)
    }

    #[test]
    fn idle_count_recovers_from_poison() {
        let pool = test_pool(1);
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _guard = pool.idle.lock().unwrap();
            panic!("制造 pool poison");
        }));

        assert_eq!(pool.idle_count(), 0);
    }

    #[test]
    fn release_recovers_from_poison() {
        let pool = test_pool(1);
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _guard = pool.idle.lock().unwrap();
            panic!("制造 pool poison");
        }));

        pool.release(Box::new(TestMapper));
        assert_eq!(pool.idle_count(), 1);
    }
}
