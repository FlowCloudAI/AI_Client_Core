pub mod mapper;
pub mod pool;
pub mod registry;
pub mod types;

// 以下模块保留原有实现，按你的项目实际路径调整
pub mod bindings;  // wasm 绑定（wit-bindgen 生成）
pub mod host;      // HostState 定义
pub mod manager;
pub mod scanner;

pub mod loaded;
// PluginManager（扫描 ./plugins 目录，编译 wasm）