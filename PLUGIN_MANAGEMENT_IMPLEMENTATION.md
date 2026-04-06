# 插件管理功能实现总结

## 概述

本次实现在 `FlowCloudAIClient` 中添加了完整的插件生命周期管理能力，包括元数据查询、动态安装和安全卸载功能。

## 核心改动

### 1. PluginRegistry 增强 (`src/plugin/registry.rs`)

#### 新增字段
- `ref_counts: Arc<Mutex<HashMap<String, usize>>>`: 跟踪每个插件的活跃 session 数量

#### 新增方法
- `increment_ref(plugin_id)`: 增加引用计数（session 创建时调用）
- `decrement_ref(plugin_id)`: 减少引用计数（session 销毁时调用）
- `get_ref_count(plugin_id)`: 获取当前引用计数
- `add_module(id, meta, wasm_bytes)`: 动态添加新插件模块
- `unload(plugin_id) -> Result<()>`: 安全卸载插件（检查引用计数）

#### 修改的方法
- `unload()`: 从 void 改为返回 `Result<()>`，增加引用计数检查

### 2. ApiPipeline 引用计数集成 (`src/plugin/pipeline.rs`)

#### 自动管理
- `ApiPipeline::new()`: 创建时自动增加插件引用计数
- `impl Drop for ApiPipeline`: 销毁时自动减少引用计数

这确保了所有 Session（LLM、TTS、Image）的生命周期与插件引用计数同步。

### 3. FlowCloudAIClient 公共 API (`src/client.rs`)

#### 新增字段
- `plugins_dir: PathBuf`: 保存插件目录路径，支持文件操作

#### 新增方法

##### `list_all_plugins(&self) -> Vec<PluginMeta>`
返回所有已识别插件的完整元数据列表，包含：
- id, name, version, description, author
- kind (LLM/Image/TTS)
- fcplug_path (文件路径)

##### `get_plugin_ref_count(&self, plugin_id: &str) -> usize`
获取指定插件的当前引用计数（用于诊断）。

##### `uninstall_plugin(&mut self, plugin_id: &str) -> Result<()>`
安全卸载插件，执行以下步骤：
1. 检查插件是否存在
2. 检查引用计数，如果 > 0 则返回错误
3. 调用 `PluginRegistry::unload()` 销毁 WASM 实例池
4. 使用 `std::fs::remove_file` 删除磁盘上的 .fcplug 文件

**错误处理**：
- 插件不存在：返回明确错误
- 插件正在被使用：返回 `"still in use by N session(s)"` 错误
- 文件操作失败：附带上下文信息

##### `install_plugin_from_path(&mut self, source_path: &Path) -> Result<PluginMeta>`
从外部路径安装插件，执行以下步骤：
1. 读取 manifest.json 校验 ABI 版本和 ID 唯一性
2. 将 .fcplug 文件复制到内部插件目录
3. 编译 WASM 模块并添加到 PluginRegistry
4. 返回新插件的 PluginMeta

**校验规则**：
- ABI 版本必须匹配 `SUPPORTED_ABI_VERSION`
- 插件 ID 不能与现有插件重复
- 必须包含有效的 `plugin.wasm` 文件

## 线程安全设计

### 引用计数机制
- 使用 `Arc<Mutex<HashMap>>` 确保跨线程安全
- Session 创建时自动 +1，销毁时自动 -1
- 卸载前检查计数，防止悬垂引用

### Arc 可变借用
- `uninstall_plugin` 和 `install_plugin_from_path` 需要 `&mut self`
- 使用 `Arc::get_mut()` 确保没有其他线程持有引用
- 如果存在活跃 session，返回明确错误提示

## 使用示例

参见 `examples/plugin_management.rs`：

```rust
use flowcloudai_client::FlowCloudAIClient;
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let plugins_dir = PathBuf::from("./plugins");
    let mut client = FlowCloudAIClient::new(plugins_dir)?;

    // 1. 列出所有插件
    let plugins = client.list_all_plugins();
    for plugin in &plugins {
        println!("{} v{} - {}", plugin.name, plugin.version, plugin.description);
    }

    // 2. 查看引用计数
    for plugin in &plugins {
        let count = client.get_plugin_ref_count(&plugin.id);
        println!("{}: {} active sessions", plugin.id, count);
    }

    // 3. 安装新插件
    let meta = client.install_plugin_from_path(&PathBuf::from("./downloads/new-plugin.fcplug"))?;
    println!("Installed: {}", meta.name);

    // 4. 卸载插件（需确保无活跃 session）
    client.uninstall_plugin(&meta.id)?;

    Ok(())
}
```

## 错误场景处理

| 场景 | 行为 |
|------|------|
| 卸载正在使用的插件 | 返回错误："still in use by N session(s)" |
| 卸载不存在的插件 | 返回错误："plugin 'X' not found" |
| 安装有重复 ID 的插件 | 返回错误："plugin 'X' already exists" |
| ABI 版本不匹配 | 返回错误："ABI version mismatch: expected X, got Y" |
| 有活跃 session 时安装/卸载 | 返回错误："cannot ... while sessions hold a registry reference" |
| 文件复制失败 | 返回错误并附带源/目标路径 |
| WASM 编译失败 | 返回错误并附带编译错误详情 |

## 技术细节

### 所有权模型
```
FlowCloudAIClient
  ├─ Arc<PluginRegistry> (共享)
  │   ├─ plugins: HashMap<String, PluginMeta>
  │   ├─ modules: HashMap<String, Component>
  │   ├─ pools: HashMap<String, MapperPool>
  │   └─ ref_counts: Arc<Mutex<HashMap<String, usize>>>
  │
  └─ plugins_dir: PathBuf (独占)

LLMSession / TTSSession / ImageSession
  └─ ApiPipeline
      └─ Arc<PluginRegistry> (clone)
          └─ (Drop 时自动 decrement_ref)
```

### 编译验证
```bash
cargo build --lib          # ✅ 库编译成功
cargo build --example plugin_management  # ✅ 示例编译成功
```

## 后续优化建议

1. **批量操作**: 支持 `install_plugins_from_dir()` 批量安装
2. **版本管理**: 支持同一插件的多版本共存
3. **热更新**: 实现无损插件替换（等待旧 session 自然消亡）
4. **插件缓存**: 缓存已编译的 Module，加速重启加载
5. **依赖检查**: 支持插件间依赖关系声明

## 相关文件清单

- `src/plugin/registry.rs`: 引用计数和动态加载逻辑
- `src/plugin/pipeline.rs`: Session 生命周期集成
- `src/client.rs`: 公共 API 暴露
- `examples/plugin_management.rs`: 使用示例
