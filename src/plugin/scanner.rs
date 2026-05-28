use crate::error::{ClientError, ErrorCode};
use crate::plugin::types::{PluginManifest, PluginMeta};
use anyhow::Result;
use std::fs;
use std::fs::File;
use std::path::{Path, PathBuf};
use zip::ZipArchive;

pub struct PluginScanner;

impl PluginScanner {
    pub fn read_plugin_info(fcplug: &Path) -> Result<PluginManifest> {
        let path_str = fcplug.display().to_string();
        let file = File::open(fcplug).map_err(|e| {
            ClientError::new(
                ErrorCode::FsOpenFailed,
                format!("无法打开插件包: {}", path_str),
            )
            .with_kv("path", path_str.clone())
            .with_kv("source", e.to_string())
        })?;
        let mut archive = ZipArchive::new(file).map_err(|e| {
            ClientError::new(ErrorCode::PluginLoadFailed, "插件包不是合法 ZIP")
                .with_kv("path", path_str.clone())
                .with_kv("source", e.to_string())
        })?;
        let mut manifest = archive.by_name("manifest.json").map_err(|e| {
            ClientError::new(ErrorCode::PluginManifestInvalid, "插件包缺少 manifest.json")
                .with_kv("path", path_str.clone())
                .with_kv("source", e.to_string())
        })?;
        let mut buf = String::new();
        use std::io::Read;
        manifest.read_to_string(&mut buf).map_err(|e| {
            ClientError::new(ErrorCode::PluginManifestInvalid, "读取 manifest.json 失败")
                .with_kv("path", path_str.clone())
                .with_kv("source", e.to_string())
        })?;
        let info = PluginManifest::parse(&buf)?;
        Ok(info)
    }

    pub fn build_plugin_meta(manifest: PluginManifest, fcplug: &Path) -> Result<PluginMeta> {
        PluginMeta::from_manifest(manifest, fcplug.to_path_buf())
    }

    pub fn scan_plugins(dir: &Path) -> Result<Vec<PathBuf>> {
        let mut result = Vec::new();
        let dir_str = dir.display().to_string();

        match fs::read_dir(dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();

                    if path.extension().and_then(|s| s.to_str()) == Some("fcplug") {
                        log::info!("[plugin] found file: {}", path.display());
                        result.push(path);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!("[plugin] directory not found, creating: {}", dir.display());
                fs::create_dir(dir).map_err(|err| {
                    ClientError::new(ErrorCode::FsWriteFailed, "创建插件目录失败")
                        .with_kv("path", dir_str.clone())
                        .with_kv("source", err.to_string())
                })?;
            }
            Err(e) => {
                log::warn!("[plugin] failed to read directory: {}", e);
                return Err(
                    ClientError::new(ErrorCode::FsOpenFailed, "读取插件目录失败")
                        .with_kv("path", dir_str)
                        .with_kv("source", e.to_string())
                        .into(),
                );
            }
        }

        Ok(result)
    }
}
