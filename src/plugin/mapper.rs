use anyhow::Result;
use wasmtime::Store;

// 假设你的 wasm 绑定模块路径，按实际项目调整
use crate::plugin::bindings::plugin_bindings::Api;
use crate::plugin::host::HostState;

// ─────────────────────────── 映射器接口 ────────────────────

/// 请求/响应映射器。
///
/// 每个实例内部持有独占的 wasmtime `Store`，
/// 因此 `&mut self` 不需要外部锁。
pub trait ApiMapper: Send {
    fn map_request(&mut self, json: &str) -> Result<String>;
    fn map_response(&mut self, json: &str) -> Result<String>;
    fn map_stream_line(&mut self, line: &str) -> Result<String>;
}

// ─────────────────────── 直通映射器 ─────────────────────────

/// 无插件时的直通实现：原样返回，零开销。
pub struct PassthroughMapper;

impl ApiMapper for PassthroughMapper {
    #[inline]
    fn map_request(&mut self, json: &str) -> Result<String> {
        Ok(json.to_string())
    }
    #[inline]
    fn map_response(&mut self, json: &str) -> Result<String> {
        Ok(json.to_string())
    }
    #[inline]
    fn map_stream_line(&mut self, line: &str) -> Result<String> {
        Ok(line.to_string())
    }
}

// ─────────────────────── Wasm 映射器 ────────────────────────

/// 基于 wasmtime 的插件映射器。
///
/// 每个实例持有独立的 `Store<HostState>`，
/// 因此多个 WasmMapper 可以在不同线程并发执行，互不干扰。
pub struct WasmMapper {
    pub(crate) store: Store<HostState>,
    pub(crate) api: Api,
}

impl ApiMapper for WasmMapper {
    fn map_request(&mut self, json: &str) -> Result<String> {
        Ok(self.api.mapper_plugin_mapper().call_map_request(&mut self.store, json)?)
    }

    fn map_response(&mut self, json: &str) -> Result<String> {
        Ok(self.api.mapper_plugin_mapper().call_map_response(&mut self.store, json)?)
    }

    fn map_stream_line(&mut self, line: &str) -> Result<String> {
        Ok(self.api.mapper_plugin_mapper().call_map_stream_line(&mut self.store, line)?)
    }
}