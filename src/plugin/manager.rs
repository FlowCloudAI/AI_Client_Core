use crate::error::{ClientError, ErrorCode};
use crate::plugin::host::HostState;
use crate::plugin::types::{PluginKind, PluginManifest, PluginMeta};
use crate::{LoadedPlugin, PluginScanner};
use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use wasmtime::component::Linker;
use wasmtime::{Config, Engine};

pub struct PluginManager {
    plug_path: PathBuf,
    pub plugins: HashMap<String, PluginMeta>,
    pub load_report: PluginLoadReport,
    pub(crate) engine: Engine,
    pub(crate) linker: Linker<HostState>,

    llm_plugin: LoadedPlugin,
    image_plugin: LoadedPlugin,
    tts_plugin: LoadedPlugin,
}

/// 插件加载报告。
#[derive(Debug, Clone, Default)]
pub struct PluginLoadReport {
    pub loaded: Vec<PluginMeta>,
    pub skipped: Vec<PluginLoadError>,
}

/// 单个插件跳过原因。
#[derive(Debug, Clone)]
pub struct PluginLoadError {
    pub path: PathBuf,
    pub error: ClientError,
}

// ── 初始化 ──

impl PluginManager {
    pub fn new(plug_path: PathBuf) -> Result<Self> {
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

        let (plugins, load_report) = Self::load_plugins_report(Path::new(&plug_path))?;

        Ok(PluginManager {
            plug_path,
            plugins,
            load_report,
            engine,
            linker,
            llm_plugin: LoadedPlugin::new(PluginKind::LLM),
            image_plugin: LoadedPlugin::new(PluginKind::Image),
            tts_plugin: LoadedPlugin::new(PluginKind::TTS),
        })
    }

    fn load_plugins_report(path: &Path) -> Result<(HashMap<String, PluginMeta>, PluginLoadReport)> {
        let mut plugins: HashMap<String, PluginMeta> = HashMap::new();
        let mut report = PluginLoadReport::default();

        for fcplug in PluginScanner::scan_plugins(path).map_err(|e| {
            // 如果已经是 ClientError 就透传，否则包装一层
            if ClientError::from_anyhow(&e).is_some() {
                e
            } else {
                ClientError::new(ErrorCode::FsOpenFailed, "扫描插件目录失败")
                    .with_kv("source", e.to_string())
                    .into()
            }
        })? {
            match PluginScanner::read_plugin_info(&fcplug) {
                Ok(manifest) => {
                    if let Err(err) = Self::validate_plugin(&manifest, &plugins) {
                        report.skipped.push(PluginLoadError {
                            path: fcplug,
                            error: err,
                        });
                        continue;
                    }

                    let id = manifest.meta.id.clone();
                    match PluginScanner::build_plugin_meta(manifest, &fcplug) {
                        Ok(meta) => {
                            report.loaded.push(meta.clone());
                            plugins.insert(id, meta);
                        }
                        Err(e) => {
                            report.skipped.push(PluginLoadError {
                                path: fcplug,
                                error: ClientError::from_anyhow_owned(e),
                            });
                        }
                    }
                }
                Err(e) => {
                    report.skipped.push(PluginLoadError {
                        path: fcplug,
                        error: ClientError::from_anyhow_owned(e),
                    });
                }
            }
        }

        Ok((plugins, report))
    }

    fn validate_plugin(
        manifest: &PluginManifest,
        existing: &HashMap<String, PluginMeta>,
    ) -> std::result::Result<(), ClientError> {
        let info = &manifest.meta;

        if existing.contains_key(&info.id) {
            return Err(ClientError::new(
                ErrorCode::PluginAlreadyExists,
                format!("插件 ID 重复: {}", info.id),
            )
            .with_kv("plugin_id", info.id.clone()));
        }

        Ok(())
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
            .ok_or_else(|| {
                ClientError::new(ErrorCode::PluginNotFound, format!("插件 '{}' 不存在", id))
                    .with_kv("plugin_id", id.to_string())
                    .into()
            })
            .map(|meta| meta.url.as_str())
    }

    pub fn add_plugin(&mut self, plugin_path: &str) -> Result<()> {
        let manifest = PluginScanner::read_plugin_info(plugin_path.as_ref())?;

        let info = &manifest.meta;

        if self.plugins.contains_key(&info.id) {
            return Err(ClientError::new(
                ErrorCode::PluginAlreadyExists,
                format!("插件 '{}' 已存在", info.id),
            )
            .with_kv("plugin_id", info.id.clone())
            .into());
        }

        let filename = Path::new(plugin_path).file_name().ok_or_else(|| {
            ClientError::new(
                ErrorCode::ValidationFormatError,
                format!("无效的插件文件名: {}", plugin_path),
            )
            .with_kv("path", plugin_path.to_string())
        })?;

        let dst = Path::new(&self.plug_path).join(filename);

        fs::copy(plugin_path, &dst).map_err(|e| {
            ClientError::new(
                ErrorCode::FsWriteFailed,
                format!("复制插件 '{}' 到 {:?} 失败", info.id, dst),
            )
            .with_kv("plugin_id", info.id.clone())
            .with_kv("source", e.to_string())
        })?;

        let id = info.id.clone();
        let meta = PluginScanner::build_plugin_meta(manifest, &dst)?;
        self.plugins.insert(id, meta);

        Ok(())
    }
}

// ── 插件加载 ──

impl PluginManager {
    pub fn load_llm_plugin(&mut self, id: &str) -> Result<()> {
        self.llm_plugin
            .load(&self.plugins, &self.linker, &self.engine, id)
    }

    pub fn load_image_plugin(&mut self, id: &str) -> Result<()> {
        self.image_plugin
            .load(&self.plugins, &self.linker, &self.engine, id)
    }

    pub fn load_tts_plugin(&mut self, id: &str) -> Result<()> {
        self.tts_plugin
            .load(&self.plugins, &self.linker, &self.engine, id)
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
