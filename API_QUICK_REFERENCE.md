# 插件管理 API 快速参考

## 核心方法

### 1. 列出所有插件
```rust
pub fn list_all_plugins(&self) -> Vec<PluginMeta>
```
**返回**: 包含完整元数据的插件列表  
**用途**: UI 展示、插件信息查询

---

### 2. 获取引用计数
```rust
pub fn get_plugin_ref_count(&self, plugin_id: &str) -> usize
```
**返回**: 当前使用该插件的活跃 session 数量  
**用途**: 诊断、卸载前检查

---

### 3. 安装插件
```rust
pub fn install_plugin_from_path(&mut self, source_path: &Path) -> Result<PluginMeta>
```
**参数**: 
- `source_path`: .fcplug 文件的绝对或相对路径

**返回**: 新插件的元数据

**校验**:
- ✅ ABI 版本匹配
- ✅ ID 唯一性
- ✅ 包含 plugin.wasm

**错误场景**:
- ❌ 文件不存在 → IO 错误
- ❌ ABI 不匹配 → `"ABI version mismatch"`
- ❌ ID 重复 → `"plugin 'X' already exists"`
- ❌ 有活跃 session → `"cannot install while sessions hold a registry reference"`

---

### 4. 卸载插件
```rust
pub fn uninstall_plugin(&mut self, plugin_id: &str) -> Result<()>
```
**参数**: 
- `plugin_id`: 要卸载的插件 ID

**执行步骤**:
1. 检查插件存在性
2. 检查引用计数（必须为 0）
3. 销毁 WASM 实例池
4. 删除 .fcplug 文件

**错误场景**:
- ❌ 插件不存在 → `"plugin 'X' not found"`
- ❌ 仍在使用 → `"still in use by N session(s)"`
- ❌ 有活跃 session → `"cannot uninstall while sessions hold a registry reference"`
- ❌ 文件删除失败 → IO 错误附带路径

---

## 使用流程

### 典型工作流
```rust
// 1. 初始化
let mut client = FlowCloudAIClient::new(PathBuf::from("./plugins"))?;

// 2. 查看现有插件
for plugin in client.list_all_plugins() {
    println!("{} v{}", plugin.name, plugin.version);
}

// 3. 安装新插件
let meta = client.install_plugin_from_path(&PathBuf::from("download.qwen.fcplug"))?;

// 4. 创建 session（自动增加引用计数）
let session = client.create_llm_session(&meta.id, "api-key")?;

// 5. 尝试卸载（会失败，因为引用计数 > 0）
assert!(client.uninstall_plugin(&meta.id).is_err());

// 6. 销毁 session（自动减少引用计数）
drop(session);

// 7. 再次卸载（成功）
client.uninstall_plugin(&meta.id)?;
```

---

## 线程安全保证

| 操作 | 线程安全 | 说明 |
|------|---------|------|
| `list_all_plugins()` | ✅ | 只读操作 |
| `get_plugin_ref_count()` | ✅ | 通过 Mutex 保护 |
| `create_*_session()` | ✅ | Arc clone，原子增加引用 |
| `install_plugin_from_path()` | ⚠️ | 需要 `&mut self`，确保无活跃 session |
| `uninstall_plugin()` | ⚠️ | 需要 `&mut self`，检查引用计数 |

---

## 常见错误处理

```rust
use anyhow::Result;

fn safe_uninstall(client: &mut FlowCloudAIClient, plugin_id: &str) -> Result<()> {
    // 检查是否存在
    if client.get_plugin_ref_count(plugin_id) > 0 {
        return Err(anyhow::anyhow!(
            "Plugin {} is still in use. Close all sessions first.",
            plugin_id
        ));
    }
    
    // 执行卸载
    client.uninstall_plugin(plugin_id)?;
    Ok(())
}

fn safe_install(client: &mut FlowCloudAIClient, path: &Path) -> Result<PluginMeta> {
    match client.install_plugin_from_path(path) {
        Ok(meta) => Ok(meta),
        Err(e) if e.to_string().contains("already exists") => {
            // 插件已存在，可以选择跳过或更新
            eprintln!("Plugin already installed, skipping...");
            Err(e)
        }
        Err(e) => Err(e),
    }
}
```

---

## 性能考虑

| 操作 | 耗时 | 说明 |
|------|------|------|
| `list_all_plugins()` | < 1ms | 仅克隆 HashMap |
| `get_plugin_ref_count()` | < 1μs | Mutex 锁定 + HashMap 查找 |
| `install_plugin_from_path()` | 10-100ms | 文件复制 + WASM 编译 |
| `uninstall_plugin()` | < 1ms | HashMap 移除 + 文件删除 |

---

## 调试技巧

### 查看所有插件状态
```rust
println!("=== Plugin Status ===");
for plugin in client.list_all_plugins() {
    let ref_count = client.get_plugin_ref_count(&plugin.id);
    let pool_stats = client.pool_stats();
    let idle = pool_stats.get(plugin.id.as_str()).unwrap_or(&0);
    println!(
        "{}: refs={}, idle_instances={}",
        plugin.id, ref_count, idle
    );
}
```

### 强制清理（开发环境）
```rust
// ⚠️ 仅用于测试，生产环境不应这样做
fn force_cleanup_all(client: &mut FlowCloudAIClient) -> Result<()> {
    let plugin_ids: Vec<String> = client.list_all_plugins()
        .iter()
        .map(|p| p.id.clone())
        .collect();
    
    for id in plugin_ids {
        // 忽略错误（可能有活跃 session）
        let _ = client.uninstall_plugin(&id);
    }
    Ok(())
}
```
