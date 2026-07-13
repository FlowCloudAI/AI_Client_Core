use anyhow::Result;
use flowcloudai_client::FlowCloudAIClient;
use std::path::PathBuf;

fn main() -> Result<()> {
    let client = FlowCloudAIClient::new(PathBuf::from("plugins"))?;

    for plugin in client.list_plugins() {
        println!("Plugin: {} id: {}", plugin.name, plugin.id);
    }

    Ok(())
}
