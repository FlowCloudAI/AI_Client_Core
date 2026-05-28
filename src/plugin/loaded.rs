use crate::error::{ClientError, ErrorCode};
use crate::plugin::bindings::plugin_bindings;
use crate::plugin::host::HostState;
use crate::plugin::types::{PluginKind, PluginMeta};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::WasiCtxBuilder;
use zip::ZipArchive;

pub struct LoadedPlugin {
    pub kind: PluginKind,
    state: PluginState,
}

enum PluginState {
    Unloaded,
    Loaded {
        store: Store<HostState>,
        api: plugin_bindings::Api,
        icon: Vec<u8>,
    },
}

impl LoadedPlugin {
    pub fn new(kind: PluginKind) -> Self {
        Self {
            kind,
            state: PluginState::Unloaded,
        }
    }

    pub fn is_loaded(&self) -> bool {
        matches!(self.state, PluginState::Loaded { .. })
    }

    pub fn icon(&self) -> &[u8] {
        match &self.state {
            PluginState::Loaded { icon, .. } => icon,
            PluginState::Unloaded => &[],
        }
    }

    fn read_zip_file(archive: &mut ZipArchive<File>, name: &str) -> Option<Vec<u8>> {
        if let Ok(mut file) = archive.by_name(name) {
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).ok()?;
            Some(buf)
        } else {
            None
        }
    }

    pub fn load(
        &mut self,
        plugins: &HashMap<String, PluginMeta>,
        linker: &Linker<HostState>,
        engine: &Engine,
        id: &str,
    ) -> anyhow::Result<()> {
        let meta = plugins.get(id).ok_or_else(|| {
            ClientError::new(ErrorCode::PluginNotFound, format!("插件 '{}' 不存在", id))
                .with_kv("plugin_id", id.to_string())
        })?;

        if meta.kind != self.kind {
            return Err(ClientError::new(
                ErrorCode::PluginKindMismatch,
                format!("插件 '{}' 类型不匹配", id),
            )
            .with_kv("plugin_id", id.to_string())
            .with_kv("expected_kind", format!("{:?}", self.kind))
            .with_kv("actual_kind", format!("{:?}", meta.kind))
            .into());
        }

        let path_str = meta.fcplug_path.display().to_string();
        let file = File::open(&meta.fcplug_path).map_err(|e| {
            ClientError::new(ErrorCode::FsOpenFailed, "无法打开插件包")
                .with_kv("plugin_id", id.to_string())
                .with_kv("path", path_str.clone())
                .with_kv("source", e.to_string())
        })?;
        let mut archive = ZipArchive::new(file).map_err(|e| {
            ClientError::new(ErrorCode::PluginLoadFailed, "插件包不是合法 ZIP")
                .with_kv("plugin_id", id.to_string())
                .with_kv("source", e.to_string())
        })?;

        let wasm_bytes =
            LoadedPlugin::read_zip_file(&mut archive, "plugin.wasm").ok_or_else(|| {
                ClientError::new(ErrorCode::PluginLoadFailed, "插件包缺少 plugin.wasm")
                    .with_kv("plugin_id", id.to_string())
            })?;

        let icon = LoadedPlugin::read_zip_file(&mut archive, "icon.png").unwrap_or_default();

        let component = Component::from_binary(engine, &wasm_bytes).map_err(|e| {
            ClientError::new(ErrorCode::PluginLoadFailed, "Wasm 组件编译失败")
                .with_kv("plugin_id", id.to_string())
                .with_kv("source", e.to_string())
        })?;

        let state = HostState {
            table: ResourceTable::new(),
            wasi: WasiCtxBuilder::new()
                .inherit_stdout()
                .inherit_stderr()
                .build(),
        };

        let mut store = Store::new(&engine, state);

        let api =
            plugin_bindings::Api::instantiate(&mut store, &component, linker).map_err(|e| {
                ClientError::new(ErrorCode::PluginLoadFailed, "Wasm 组件实例化失败")
                    .with_kv("plugin_id", id.to_string())
                    .with_kv("source", e.to_string())
            })?;

        self.state = PluginState::Loaded { store, api, icon };

        Ok(())
    }

    pub fn map_request(&mut self, json: &str) -> anyhow::Result<String> {
        match &mut self.state {
            PluginState::Loaded { store, api, .. } => {
                let mapper = api.mapper_plugin_mapper();
                let result = mapper.call_map_request(store, json).map_err(|e| {
                    ClientError::new(ErrorCode::PluginRuntimeError, "插件 map_request 执行失败")
                        .with_kv("source", e.to_string())
                })?;
                Ok(result)
            }
            PluginState::Unloaded => Err(ClientError::new(
                ErrorCode::PluginNotLoaded,
                "插件未加载，无法 map_request",
            )
            .into()),
        }
    }

    pub fn map_response(&mut self, json: &str) -> anyhow::Result<String> {
        match &mut self.state {
            PluginState::Loaded { store, api, .. } => {
                let mapper = api.mapper_plugin_mapper();
                let result = mapper.call_map_response(store, json).map_err(|e| {
                    ClientError::new(ErrorCode::PluginRuntimeError, "插件 map_response 执行失败")
                        .with_kv("source", e.to_string())
                })?;
                Ok(result)
            }
            PluginState::Unloaded => Err(ClientError::new(
                ErrorCode::PluginNotLoaded,
                "插件未加载，无法 map_response",
            )
            .into()),
        }
    }

    pub fn map_stream_line(&mut self, line: &str) -> anyhow::Result<String> {
        match &mut self.state {
            PluginState::Loaded { store, api, .. } => {
                let mapper = api.mapper_plugin_mapper();
                let result = mapper.call_map_stream_line(store, line).map_err(|e| {
                    ClientError::new(
                        ErrorCode::PluginRuntimeError,
                        "插件 map_stream_line 执行失败",
                    )
                    .with_kv("source", e.to_string())
                })?;
                Ok(result)
            }
            PluginState::Unloaded => Err(ClientError::new(
                ErrorCode::PluginNotLoaded,
                "插件未加载，无法 map_stream_line",
            )
            .into()),
        }
    }
}
