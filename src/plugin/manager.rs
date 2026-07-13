use crate::error::{ClientError, ErrorCode};
use crate::plugin::host::HostState;
use crate::plugin::scanner::PluginScanner;
use crate::plugin::types::{PluginManifest, PluginMeta};
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use wasmtime::Engine;
use wasmtime::component::Linker;

pub(crate) struct PluginManager {
    pub plugins: HashMap<String, PluginMeta>,
    pub load_report: PluginLoadReport,
    pub(crate) engine: Engine,
    pub(crate) linker: Linker<HostState>,
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
    pub(crate) fn new(plug_path: PathBuf) -> Result<Self> {
        let engine = super::engine::build_plugin_engine()?;
        let mut linker = Linker::new(&engine);

        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|e| {
            ClientError::new(ErrorCode::CoreClientInitFailed, "向 linker 注册 WASI 失败")
                .with_kv("source", e.to_string())
        })?;

        let (plugins, load_report) = Self::load_plugins_report(Path::new(&plug_path))?;

        Ok(PluginManager {
            plugins,
            load_report,
            engine,
            linker,
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
                    match PluginMeta::from_manifest(manifest, fcplug.clone()) {
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
