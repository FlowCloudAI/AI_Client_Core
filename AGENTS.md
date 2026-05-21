# core_ai_client — AGENTS.md

> 本文档面向 AI 编码助手。修改本仓库前先确认当前任务是否影响插件协议、会话事件或上层 App。

## 项目概览

`core_ai_client`（crate 名：`flowcloudai_client`）是 FlowCloudAI 的多模态 AI 客户端核心库，统一封装 LLM、图像生成、TTS、音频播放、工具调用和 WASM 插件映射。它被桌面端 App、示例程序和插件调试流程共同使用。

## 构建 / 运行 / 测试 / lint

```bash
cd core_ai_client

# 构建库
cargo build

# 发布构建，Cargo.toml 已配置 thin LTO、strip symbols、panic=abort
cargo build --release

# 运行测试
cargo test

# 常用示例
cargo run --example main
cargo run --example plugin_management
cargo run --example llm
cargo run --example image
cargo run --example tts
cargo run --example llm_ai_dialogue
cargo run --example orchestrate
```

当前没有独立 lint 脚本；Rust 修改后至少运行 `cargo test` 或与改动相关的示例。远程模型示例通常需要合法 `.fcplug` 插件和 API Key。

## 代码风格与命名约定

- Rust 使用 Edition 2024，遵循标准命名：类型 `PascalCase`，函数 / 变量 / 模块 `snake_case`，常量 `SCREAMING_SNAKE_CASE`。
- 注释、文档和示例说明使用中文。
- 公共错误处理以 `anyhow::Result` 为主，不在公共路径使用 `unwrap()` / `expect()`。
- 会话和插件状态大量使用 `Arc<RwLock<...>>`、`Arc<...>` 和 Tokio 异步通道；不要在异步锁持有期间执行长时间 IO。
- 新增公共类型必须保持 `serde` 序列化字段稳定，尤其是 `llm/types.rs` 中的会话事件和请求 / 响应结构。
- 修改流式解析逻辑时，同步检查 `llm/stream_decoder.rs`、`llm/session.rs` 和示例输出。

## 目录结构与模块职责

```text
core_ai_client/
├── docs/              # 协议、调试和设计文档
├── examples/          # LLM、图像、TTS、插件管理和编排示例
├── plugins/           # 本地调试用插件目录
├── src/
│   ├── audio/         # 音频解码与播放
│   ├── image/         # 图像生成会话
│   ├── llm/           # LLM 会话、事件、对话树、流式解析
│   ├── orchestrator/  # 任务上下文装配与默认编排器
│   ├── plugin/        # 插件扫描、注册、加载、池化、WIT 绑定
│   ├── sense/         # Sense trait
│   ├── tool/          # 工具注册与执行
│   ├── tts/           # 文本转语音会话
│   ├── client.rs      # FlowCloudAIClient 主入口
│   └── lib.rs         # crate 导出和协议常量
├── wit/               # 插件 WIT 接口副本
├── Cargo.toml
├── LICENSE
└── Readme.md
```

重点文件：

- `src/lib.rs`：`SUPPORTED_AGREEMENT_VERSION = 1`，修改会影响所有插件。
- `src/plugin/types.rs`：manifest 校验和协议版本检查。
- `src/plugin/pipeline.rs` / `mapper.rs`：统一请求与供应商请求之间的映射。
- `src/llm/session.rs`：会话主循环、工具调用、事件派发。
- `src/llm/tree.rs`：`ConversationTree`，已有较完整单元测试。
- `examples/support/apis.rs`：提交安全的示例配置；启用 `local-apis` feature 时才读取未提交的本地 API 配置。

## 提交信息与 PR 规范

- 提交信息默认使用中文，格式建议为“动词 + 范围 + 目的”，例如 `修正流式工具调用事件`。
- 一个提交只包含一个明确任务，不混入格式化、示例密钥、构建产物或无关重构。
- PR 说明需写明是否影响插件协议、`SessionEvent`、公共类型、示例行为，以及运行过的 `cargo test` / `cargo run --example ...`。
- 涉及插件协议时，PR 中必须列出同步检查过的 App、`tool_fcplug` 和 `plugins/*/wit/plugin.wit`。

## 安全 / 禁止事项

- 不提交 API Key、供应商 URL 私有参数、真实用户对话或本地 `examples/apis/mod.rs`。
- 不绕过 manifest 的 `agreement-version` 校验；兼容旧协议应通过迁移工具或显式兼容层实现。
- 不把 `.fcplug` 调试包、`target/`、音频输出或临时下载文件作为源码提交。
- 不在工具调用中引入无超时、不可取消或非幂等的副作用。
- 不把对话持久化重新塞回本库；当前约定是内存 `ConversationTree`，持久化由上层应用负责。

## 项目特有坑点

- `.fcplug` 本质是 ZIP，必须包含 `manifest.json`、`plugin.wasm`、`icon.png`。
- 插件构建目标是 `wasm32-wasip2`，宿主使用 wasmtime Component Model 42。
- `FlowCloudAIClient::new` 会自动激活扫描到的插件；`load_plugin` 主要用于显式重新加载或兼容旧调用。
- `PluginRegistry` 有引用计数，活跃 Session 持有插件时不能随意卸载。
- `LLMSession` 会同时产生内容、推理、工具调用、工具结果和分支事件；新增事件要同步 App 前端监听逻辑。
- `reqwest` 启用了 `stream` / `gzip`，修改 HTTP 层时要保留流式场景。
- 远程示例通常依赖 `./plugins` 下的合法插件包；缺插件或缺 API Key 时示例失败不代表库构建失败。
