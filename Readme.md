# FlowCloudAI 客户端核心库（core_ai_client）

`core_ai_client` 是 FlowCloudAI 的 Rust AI 核心客户端，统一提供会话入口、多模态能力与工具编排，并输出与 `.fcplug` 一致的能力映射。

## 项目简介

仓库通过示例和可复用接口将模型、插件和上层应用行为对齐，降低桌面端与站点端重复适配成本。  
修改能力模型时需要同步关注上下文管理、错误语义和兼容边界。

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

- 会话管理、状态转发与错误边界统一定义。  
- 文本、图片、语音等多模态能力统一入口。  
- `.fcplug` 插件映射、兼容性与版本验证。  
- 示例用于 API 行为回归与复现场景。  

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
- PR 建议先跑通 `cargo test` 与关键示例，补充复现步骤与结果。  
- 兼容性变更需附回退策略与影响范围。  

文档同步时间：2026-06-05 12:44:21 +08:00
