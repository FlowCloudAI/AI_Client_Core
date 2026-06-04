# FlowCloudAI 客户端核心库（core_ai_client）

`core_ai_client` 是 FlowCloudAI 的 Rust AI 核心客户端，提供统一会话入口与能力编排，对接文本、图片、语音和工具调用能力，并对外输出稳定的 `.fcplug` 映射。

## 项目简介

仓库以“能力库 + 可复现示例”组织，核心目标是把多模态模型和插件能力统一成同一调用风格，避免上层应用重复适配。  
在修改能力口径时，需要同步关注上下文状态、错误语义和兼容边界。

## 快速开始

### 安装与编译

```bash
cd core_ai_client
cargo build
cargo build --release
cargo test
```

### 体验示例

```bash
cargo run --example main
cargo run --example llm_ai_dialogue
cargo run --example orchestrate
cargo run --example image
cargo run --example tts
```

## 主要功能 / 使用方式

- 会话管理、状态转发与错误边界定义。  
- 文本、图片、语音等多模态能力统一入口。  
- `.fcplug` 插件映射与版本兼容检查。  
- 示例程序作为 API 行为回归与复现脚本。  

## 技术栈

- Rust 2024、Tokio、serde、reqwest、wasmtime、WIT  

## 目录结构（仅顶层）

```text
core_ai_client/
├── src/
├── examples/
├── plugins/
└── wit/
```

## 许可证与贡献方式

- 许可证：`core_ai_client/LICENSE`。  
- 贡献前请补充 `cargo test` 与关键示例命令的复现结果（含示例名与关键日志）。  
- PR 需说明兼容影响、凭据条件和回退场景。  

文档同步时间：2026-06-04 17:03:10 +08:00
