# core_ai_client — AGENTS.md

## 项目概览

`core_ai_client` 是 FlowCloudAI 的 Rust 核心 AI 库，负责统一接入 LLM、图片、语音等能力，并提供会话与工具编排抽象。  
它承接桌面端与网站端的能力调用，并与插件协议保持兼容。

## 构建 / 运行 / 测试 / lint

```bash
cd core_ai_client
cargo build
cargo build --release
cargo test
cargo run --example main
cargo run --example plugin_management
cargo run --example llm
cargo run --example llm_ai_dialogue
cargo run --example image
cargo run --example tts
cargo run --example orchestrate
```

该仓库未单独声明 `cargo fmt` 脚本，但通常以 `cargo test` 与示例结果作为最低回归线。

## 代码风格与命名约定

- Rust `Edition 2024`，类型 `PascalCase`，函数与变量 `snake_case`。  
- `anyhow::Result` 或显式错误类型统一错误语义。  
- 示例与库代码的行为边界应保持一致，变更公开接口前需同步 `examples` 与调用方约束。

## 目录结构与职责

```text
core_ai_client/
├── src/          # 会话、适配层、能力抽象
├── examples/     # 可执行示例（main/orchestrate/image/tts 等）
├── plugins/      # 插件协作相关定义与能力桥
└── wit/          # WIT 交互约定
```

## 安全 / 禁止事项

- 不提交真实模型 API Key、密钥和内部测试账号。  
- 不在仓库提交环境变量文件或生产凭据。  
- 工具调用与模型输出需避免直接记录敏感 prompt 与用户原始输入。

## 贡献方式与 PR 规范

- 变更接口时同步补齐最小可运行示例，至少给出受影响示例输出预期。  
- PR 说明需包含变更路径、兼容性影响与回归命令。  
- 提交信息默认中文。

## 项目特有坑点

- 示例运行依赖模型和网络环境，离线环境需在 PR 说明中标注。  
- 与调用端协议约定不一致会导致上层会话链路静默降级，需同步更新映射测试。

## 文档同步依据（本次核对）

- 同步时间：2026-05-28 18:02:58 +08:00  
- 依据文件：`core_ai_client/Cargo.toml`、`core_ai_client/examples`、`core_ai_client/src`
