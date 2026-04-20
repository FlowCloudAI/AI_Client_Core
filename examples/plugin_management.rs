/// 插件管理功能测试示例
/// 
/// 演示如何使用 FlowCloudAIClient 的插件管理 API：
/// - list_all_plugins: 列出所有已识别插件
/// - install_plugin_from_path: 从外部路径安装插件
/// - uninstall_plugin: 卸载插件（含引用计数检查）

use anyhow::Result;
use flowcloudai_client::FlowCloudAIClient;
use std::path::PathBuf;

fn main() -> Result<()> {
    // 初始化客户端
    let plugins_dir = PathBuf::from("./plugins");
    let client = FlowCloudAIClient::new(plugins_dir.clone(), None)?;

    println!("=== 1. 列出所有已识别插件 ===");
    let plugins = client.list_all_plugins();
    println!("找到 {} 个插件:", plugins.len());
    for plugin in &plugins {
        println!(
            "  - {} (v{}) [{}]\n    作者: {}\n    描述: {}\n    路径: {:?}",
            plugin.name,
            plugin.version,
            format!("{:?}", plugin.kind),
            plugin.author,
            plugin.description,
            plugin.fcplug_path
        );
    }

    println!("\n=== 2. 查看插件引用计数 ===");
    for plugin in &plugins {
        let ref_count = client.get_plugin_ref_count(&plugin.id);
        println!("  {}: {} 个活跃 session", plugin.id, ref_count);
    }

    // 注意：以下代码需要实际的 .fcplug 文件才能运行
    // println!("\n=== 3. 安装新插件 ===");
    // let new_plugin_path = PathBuf::from("./downloads/qwen-llm-v2.fcplug");
    // match client.install_plugin_from_path(&new_plugin_path) {
    //     Ok(meta) => {
    //         println!("成功安装插件: {} (v{})", meta.name, meta.version);
    //     }
    //     Err(e) => {
    //         eprintln!("安装失败: {}", e);
    //     }
    // }

    // println!("\n=== 4. 尝试卸载正在使用的插件 ===");
    // if let Some(first_plugin) = plugins.first() {
    //     // 先创建一个 session（会增加引用计数）
    //     let _session = client.create_llm_session(&first_plugin.id, "test-key", None)?;
    //     
    //     // 尝试卸载（应该失败，因为引用计数 > 0）
    //     match client.uninstall_plugin(&first_plugin.id) {
    //         Ok(_) => println!("意外成功卸载"),
    //         Err(e) => println!("预期错误: {}", e),
    //     }
    //     
    //     // session drop 后引用计数归零，可以卸载
    //     drop(_session);
    //     match client.uninstall_plugin(&first_plugin.id) {
    //         Ok(_) => println!("成功卸载插件: {}", first_plugin.id),
    //         Err(e) => eprintln!("卸载失败: {}", e),
    //     }
    // }

    println!("\n=== 测试完成 ===");
    Ok(())
}
