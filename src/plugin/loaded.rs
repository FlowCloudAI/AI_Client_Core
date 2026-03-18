use anyhow::anyhow;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::WasiCtxBuilder;
use zip::ZipArchive;
use crate::plugin::bindings::plugin_bindings;
use crate::plugin::host::HostState;
use crate::plugin::types::{PluginKind, PluginMeta};

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
    }
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
            PluginState::Unloaded => &[]
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
        id: &str
    ) -> anyhow::Result<()> {

        let meta = plugins
            .get(id)
            .ok_or_else(|| anyhow!("plugin {} not found", id))?;

        if meta.kind != self.kind {
            return Err(anyhow!("plugin {} kind mismatch", id));
        }

        let file = File::open(&meta.fcplug_path)?;
        let mut archive = ZipArchive::new(file)?;

        let wasm_bytes = LoadedPlugin::read_zip_file(&mut archive, "plugin.wasm")
            .ok_or_else(|| anyhow!("plugin.wasm not found"))?;

        let icon = LoadedPlugin::read_zip_file(&mut archive, "icon.png")
            .unwrap_or_default();

        let component = Component::from_binary(engine, &wasm_bytes)?;

        let state = HostState {
            table: ResourceTable::new(),
            wasi: WasiCtxBuilder::new()
                .inherit_stdout()
                .inherit_stderr()
                .build(),
        };

        let mut store = Store::new(&engine, state);

        let api = plugin_bindings::Api::instantiate(&mut store, &component, linker)?;

        self.state = PluginState::Loaded {
            store,
            api,
            icon
        };

        Ok(())
    }

    pub fn map_request(&mut self, json: &str) -> anyhow::Result<String> {
        match &mut self.state {
            PluginState::Loaded { store, api, .. } => {
                let mapper = api.mapper_plugin_mapper();

                let result = mapper.call_map_request(store, json)?;

                Ok(result)
            }
            PluginState::Unloaded => Err(anyhow!("plugin not loaded"))
        }
    }

    pub fn map_response(&mut self, json: &str) -> anyhow::Result<String> {
        match &mut self.state {
            PluginState::Loaded { store, api, .. } => {
                let mapper = api.mapper_plugin_mapper();

                let result = mapper.call_map_response(store, json)?;

                Ok(result)
            }
            PluginState::Unloaded => Err(anyhow!("plugin not loaded"))
        }
    }

    pub fn map_stream_line(&mut self, line: &str) -> anyhow::Result<String> {
        match &mut self.state {
            PluginState::Loaded { store, api, .. } => {
                let mapper = api.mapper_plugin_mapper();
                let result = mapper.call_map_stream_line(store, line)?;
                Ok(result)
            }
            PluginState::Unloaded => Err(anyhow!("plugin not loaded"))
        }
    }
}