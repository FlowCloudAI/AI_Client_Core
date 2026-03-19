use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use wasmtime::component::Linker;
use wasmtime::{Config, Engine};
use crate::{LoadedPlugin, PluginScanner, SUPPORTED_ABI_VERSION};
use crate::plugin::host::HostState;
use crate::plugin::types::{PluginKind, PluginManifest, PluginMeta};


pub struct PluginManager {
    plug_path: PathBuf,
    pub plugins: HashMap<String, PluginMeta>,
    pub(crate) engine: Engine,
    pub(crate) linker: Linker<HostState>,

    llm_plugin: LoadedPlugin,
    image_plugin: LoadedPlugin,
    tts_plugin: LoadedPlugin,
}

// ── 初始化 ──

impl PluginManager {
    pub fn new(plug_path: PathBuf) -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config)
            .map_err(|e| anyhow!("Failed to create WebAssembly engine: {}", e))?;
        let mut linker = Linker::new(&engine);

        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| anyhow!("Failed to add WASI to linker: {}", e))?;

        let plugins = Self::load_plugins(Path::new(&plug_path))?;

        Ok(PluginManager {
            plug_path,
            plugins,
            engine,
            linker,
            llm_plugin: LoadedPlugin::new(PluginKind::LLM),
            image_plugin: LoadedPlugin::new(PluginKind::Image),
            tts_plugin: LoadedPlugin::new(PluginKind::TTS),
        })
    }

    fn load_plugins(path: &Path) -> Result<HashMap<String, PluginMeta>> {
        let mut plugins: HashMap<String, PluginMeta> = HashMap::new();

        for fcplug in PluginScanner::scan_plugins(path)
            .context("Failed to scan plugins directory")? {
            match PluginScanner::read_plugin_info(&fcplug) {
                Ok(manifest) => {
                    if !Self::validate_plugin(&manifest, &plugins) {
                        continue;
                    }

                    let id = manifest.meta.id.clone();
                    match PluginScanner::build_plugin_meta(manifest, &fcplug) {
                        Ok(meta) => { plugins.insert(id, meta); }
                        Err(e) => { eprintln!("⚠️  Failed to build meta for {:?}: {}", fcplug, e); }
                    }
                }
                Err(e) => {
                    eprintln!("⚠️  Invalid plugin {:?}: {}", fcplug, e);
                }
            }
        }

        Ok(plugins)
    }

    fn validate_plugin(
        manifest: &PluginManifest,
        existing: &HashMap<String, PluginMeta>,
    ) -> bool {
        let info = &manifest.meta;

        if existing.contains_key(&info.id) {
            eprintln!("⚠️  Duplicate plugin ID: {}", info.id);
            return false;
        }

        if info.abi_version != SUPPORTED_ABI_VERSION {
            eprintln!(
                "⚠️  Skip plugin '{}': ABI version mismatch (expected: {}, got: {})",
                info.id, SUPPORTED_ABI_VERSION, info.abi_version
            );
            return false;
        }

        if info.url.is_empty() {
            eprintln!("⚠️  Skip plugin '{}': empty URL", info.id);
            return false;
        }

        true
    }
}

// ── 插件管理 ──

impl PluginManager {
    pub fn is_loaded(&self, kind: &PluginKind) -> bool {
        self.get_plugin(kind).is_loaded()
    }

    pub fn get_url(&self, id: &str) -> Result<&str> {
        self.plugins
            .get(id)
            .ok_or_else(|| anyhow!("Plugin not found: {}", id))
            .map(|meta| meta.url.as_str())
    }

    pub fn add_plugin(&mut self, plugin_path: &str) -> Result<()> {
        let manifest = PluginScanner::read_plugin_info(plugin_path.as_ref())
            .context("Failed to read plugin metadata")?;

        let info = &manifest.meta;

        if self.plugins.contains_key(&info.id) {
            return Err(anyhow!("Plugin already exists: {}", info.id));
        }

        if info.abi_version != SUPPORTED_ABI_VERSION {
            return Err(anyhow!(
                "Plugin '{}' ABI version mismatch: expected {}, got {}",
                info.id,
                SUPPORTED_ABI_VERSION,
                info.abi_version
            ));
        }

        let filename = Path::new(plugin_path)
            .file_name()
            .ok_or_else(|| anyhow!("Invalid plugin filename: {}", plugin_path))?;

        let dst = Path::new(&self.plug_path).join(filename);

        fs::copy(plugin_path, &dst)
            .context(format!("Failed to copy plugin '{}' to {:?}", info.id, dst))?;

        let id = info.id.clone();
        let meta = PluginScanner::build_plugin_meta(manifest, &dst)?;
        self.plugins.insert(id, meta);

        Ok(())
    }
}

// ── 插件加载 ──

impl PluginManager {
    pub fn load_llm_plugin(&mut self, id: &str) -> Result<()> {
        self.llm_plugin.load(&self.plugins, &self.linker, &self.engine, id)
            .context(format!("Failed to load LLM plugin '{}'", id))
    }

    pub fn load_image_plugin(&mut self, id: &str) -> Result<()> {
        self.image_plugin
            .load(&self.plugins, &self.linker, &self.engine, id)
            .context(format!("Failed to load Image plugin '{}'", id))
    }

    pub fn load_tts_plugin(&mut self, id: &str) -> Result<()> {
        self.tts_plugin
            .load(&self.plugins, &self.linker, &self.engine, id)
            .context(format!("Failed to load TTS plugin '{}'", id))
    }
}

// ── 插件操作 ──

impl PluginManager {
    pub fn map_request(&mut self, kind: PluginKind, json: &str) -> Result<String> {
        self.get_plugin_mut(&kind).map_request(json)
    }

    pub fn map_response(&mut self, kind: PluginKind, json: &str) -> Result<String> {
        self.get_plugin_mut(&kind).map_response(json)
    }

    pub fn map_stream_line(&mut self, kind: PluginKind, line: &str) -> Result<String> {
        self.get_plugin_mut(&kind).map_stream_line(line)
    }
}

// ── 内部 ──

impl PluginManager {
    #[inline]
    fn get_plugin(&self, kind: &PluginKind) -> &LoadedPlugin {
        match kind {
            PluginKind::LLM => &self.llm_plugin,
            PluginKind::Image => &self.image_plugin,
            PluginKind::TTS => &self.tts_plugin,
        }
    }

    #[inline]
    fn get_plugin_mut(&mut self, kind: &PluginKind) -> &mut LoadedPlugin {
        match kind {
            PluginKind::LLM => &mut self.llm_plugin,
            PluginKind::Image => &mut self.image_plugin,
            PluginKind::TTS => &mut self.tts_plugin,
        }
    }
}