# flowcloudai_client

`flowcloudai_client` 是 FlowCloudAI 的多模态 AI 客户端核心库，负责把 LLM、图像生成、TTS、音频播放、工具调用和 WASM 插件映射统一到一套 Rust API 中。它面向桌面端应用、示例程序和插件调试场景，解决“多个模型供应商、多个模态、统一会话事件流”的接入问题。

## 快速开始

### 环境要求

- Rust stable，Edition 2024。
- 运行远程模型示例需要合法 `.fcplug` 插件和对应供应商 API Key。
- 插件目录默认为当前工作目录下的 `plugins/`，示例依赖其中的 `.fcplug` 包。

### 构建与测试

```bash
cd core_ai_client

# 构建库
cargo build

# 运行库内测试
cargo test

# 发布构建
cargo build --release
```

### 运行示例

```bash
cd core_ai_client

# 查看和加载插件
cargo run --example main
cargo run --example plugin_management

# LLM / 图像 / TTS 示例；通常需要真实 API Key
cargo run --example llm
cargo run --example image
cargo run --example tts
cargo run --example llm_ai_dialogue
cargo run --example orchestrate
```

`examples/llm.rs` 默认从 `./plugins` 扫描插件，并在创建 Session 时传入 API Key。仓库示例会读取 `examples/support/apis.rs`；如果启用 `local-apis` feature，则改用本地未提交的 `examples/apis/mod.rs`。

### 最小示例

```rust
use anyhow::Result;
use flowcloudai_client::FlowCloudAIClient;
use std::path::PathBuf;

fn main() -> Result<()> {
    let client = FlowCloudAIClient::new(PathBuf::from("./plugins"))?;

    for meta in client.list_plugins() {
        println!("插件：{} / {}（已自动激活）", meta.id, meta.name);
    }

    Ok(())
}
```

## 主要功能 / 使用方式

- **插件管理**：扫描、校验、安装、卸载和加载 `.fcplug` 插件；`FlowCloudAIClient::new` 会默认激活扫描到的插件，当前协议常量为 `SUPPORTED_AGREEMENT_VERSION = 1`。
- **LLM 会话**：通过 `LLMSession` 和 `SessionHandle` 管理对话、流式事件、工具调用、模型参数和对话树。
- **图像生成**：`ImageSession` 将统一图像请求映射到插件供应商。
- **TTS**：`TTSSession` 负责文本转语音请求与响应转换。
- **音频播放**：`AudioDecoder` 使用 `symphonia` 解码 MP3 / WAV / FLAC / AAC / PCM，并通过 `cpal` 播放。
- **工具调用**：`ToolRegistry` 注册宿主工具，LLM 会话可在运行时执行工具调用。
- **任务编排**：`Orchestrate`、`TaskContext` 和 `DefaultOrchestrator` 用于把外部上下文装配成一次模型调用。
- **Sense 模式**：`sense` 模块定义可复用的模式预设，供上层应用注入系统提示和工具。

## 技术栈

- Rust Edition 2024。
- 异步运行时：`tokio`、`tokio-stream`、`futures-util`。
- WASM：`wasmtime` / `wasmtime-wasi` 42，使用 Component Model。
- HTTP：`reqwest`，启用 `json` / `stream` / `gzip`。
- 序列化与包格式：`serde`、`serde_json`、`zip`。
- 音频：`cpal`、`symphonia`。
- 其他：`anyhow`、`chrono`、`base64`、`hex`。

## 目录结构

```text
core_ai_client/
├── examples/          # LLM、图像、TTS、插件管理和编排示例
├── src/               # 库源码
├── Cargo.toml         # crate 配置、features 和 release profile
├── Cargo.lock         # 锁定依赖版本
├── AGENTS.md          # AI 编码助手维护指南
├── LICENSE            # MIT 许可证
└── Readme.md          # 当前文档
```

`src/` 顶层模块职责：

- `client.rs`：`FlowCloudAIClient` 主入口。
- `llm/`：会话、事件、对话树、参数配置和流式解析。
- `image/`、`tts/`、`audio/`：图像、语音合成和音频解码播放。
- `plugin/`：插件扫描、注册、加载、池化、映射和 WIT 绑定。
- `tool/`：工具注册与执行。
- `orchestrator/`：任务上下文装配。
- `sense/`：Sense trait 定义。

## 注意事项

- 当前库不负责持久化对话历史；`ConversationTree` 是内存结构，持久化由上层应用处理。
- `.fcplug` 必须包含 `manifest.json`、`plugin.wasm` 和 `icon.png`。
- 插件构建目标是 `wasm32-wasip2`，协议字段使用 `agreement-version = 1`。
- 运行时安装 / 卸载插件会更新 `PluginRegistry` 并检查引用计数；仍被 Session 使用的插件不能卸载。
- API Key 只应在运行时传入 Session，不要写进示例、插件包或提交历史。
- 修改插件协议时，要同步检查根仓库的 `plugins/*/wit/plugin.wit`、`tool_fcplug`（`cargo-fcplug`）和 App 调用方。

## 许可证

MIT License，详见 `LICENSE`。

## 贡献方式

提交改动前请至少运行 `cargo test`。涉及示例或插件加载行为时，再运行对应 `cargo run --example ...`；涉及公共类型或事件流时，需要同步更新上层 App 和相关插件文档。
