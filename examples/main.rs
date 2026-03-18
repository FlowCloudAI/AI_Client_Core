use anyhow::Result;
use flowcloudai_client::PluginManager;
use flowcloudai_client::plugin::types::PluginKind;
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut plugin_manager = PluginManager::new(PathBuf::from("plugins"))?;

    println!("Plugins: {}", plugin_manager.plugins.len());
    for (id, meta) in &plugin_manager.plugins {
        println!("Plugin: {} id: {}", id, meta.id);
    }
    plugin_manager.load_llm_plugin("demo")?;

    println!("{}", plugin_manager.map_request(PluginKind::LLM, "hello world")?);

    Ok(())
}