# 流云AI 客户端核心库（core_ai_client）

`core_ai_client` 是 FlowCloudAI 的 Rust 核心 AI 能力库，统一封装文本、图片与语音能力的调用和会话状态管理。  
仓库通过统一接口对外输出 LLM、工具编排和插件映射能力，供桌面端与服务端复用。

## 快速开始

### 安装与运行

```bash
cd core_ai_client
cargo build
cargo test
cargo run --example main
```

### 最小示例

1. 运行 `cargo run --example llm_ai_dialogue`，验证流式 LLM 输出。  
2. 运行 `cargo run --example orchestrate`，验证工具链编排与恢复逻辑。  
3. 运行 `cargo run --example image` 或 `cargo run --example tts`，验证多模态能力入口。

## 主要功能 / 使用方式

- 统一会话管理与会话状态机。  
- 文本、图片、语音、工具链统一调度。  
- 与 `.fcplug` 插件能力映射对接。  
- 提供可运行示例作为接口回归入口。

## 技术栈

- Rust 2024、Tokio、策略化会话抽象。  
- `anyhow`、`serde`、`reqwest`、`wasmtime` 与 WIT 约定。  

## 目录结构（仅顶层）

```text
core_ai_client/
├── src/
├── examples/
├── plugins/
└── wit/
```

## 许可证与贡献方式

许可证以子仓库与上级声明为准。  
提交前补充 `cargo test`、示例命令输出和兼容影响说明。
