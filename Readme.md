# FlowCloudAI 客户端核心库（core_ai_client）

## 项目简介

`core_ai_client` 是 FlowCloudAI 的 Rust AI 核心库，统一提供会话管理、多模态处理与工具编排能力。  
公共接口对齐桌面端、网站端和插件系统，保证语义一致性。

## 快速开始

### 安装与构建

```bash
cd core_ai_client
cargo build
cargo build --release
cargo test
```

### 体验示例

```bash
cargo run --example main
cargo run --example plugin_management
cargo run --example llm
cargo run --example llm_ai_dialogue
cargo run --example image
cargo run --example tts
cargo run --example orchestrate
```

## 主要功能 / 使用方式

- 会话管理、流式消息和错误边界。  
- 文本、图片、语音等多模态能力统一入口。  
- `.fcplug` 插件映射与兼容性验证。  
- 示例用于 API 行为回归与可复现实验。  

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
- PR 建议先补齐 `cargo test` 与关键示例输出，标注参数与复现实验。  
- 改动公共能力需附兼容性影响和回退策略。  

文档同步时间：2026-06-08 13:20:10 +08:00
