use std::fs;
use std::fs::File;
use std::path::{Path, PathBuf};
use zip::ZipArchive;
use anyhow::Result;
use crate::plugin::types::{PluginInfo, PluginMeta};

pub struct PluginScanner;

impl PluginScanner {
    pub fn read_plugin_info(fcplug: &Path) -> Result<PluginInfo> {
        let file = File::open(fcplug)?;

        let mut archive = ZipArchive::new(file)?;

        let mut manifest = archive.by_name("manifest.json")?;

        let mut buf = String::new();
        use std::io::Read;
        manifest.read_to_string(&mut buf)?;

        let info: PluginInfo = serde_json::from_str(&buf)?;

        Ok(info)
    }

    pub fn build_plugin_meta(info: PluginInfo, fcplug: &Path) -> PluginMeta {
        PluginMeta {
            id: info.id,
            name: info.name,
            description: info.description,
            author: info.author,
            version: info.version,
            kind: info.kind,
            url: info.url,
            model_list: info.model_list,
            fcplug_path: fcplug.to_path_buf(),
        }
    }

    pub fn scan_plugins(dir: &Path) -> Result<Vec<PathBuf>> {
        let mut result = Vec::new();

        match fs::read_dir(dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();

                    if path.extension().and_then(|s| s.to_str()) == Some("fcplug") {
                        println!("Found plugin: {}", path.display());
                        result.push(path);
                    }
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("Plugin directory not found, creating: {}", dir.display());
                fs::create_dir(dir)?;
            }
            Err(e) => {
                println!("Error reading plugin directory: {}", e);
                return Err(e.into());
            }
        }

        Ok(result)
    }
}