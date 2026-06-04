# core_ai_client — AGENTS.md

## 项目概览

`core_ai_client` 是 FlowCloudAI 的 AI 核心库，统一封装文本、图片、语音与工具编排能力，负责与 `.fcplug` 映射的一致性对齐。  
上层应用通过该库调用稳定接口，因此兼容性和错误语义边界是该仓库的核心约束。

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

## 代码风格与命名约定

- Rust 2024，类型 `PascalCase`，函数/变量 `snake_case`，常量 `SCREAMING_SNAKE_CASE`。  
- 错误处理优先显式传播上下文，避免吞掉模型/网络关键状态。  
- 与公开接口相关的结构体和枚举需保持语义字段命名稳定，避免无意义重构。  

## 目录结构与模块职责

```text
core_ai_client/
├── src/
│   ├── audio/       # 语音采集与解码
│   ├── image/       # 图像输入与格式处理
│   ├── llm/         # 文本与大模型能力
│   ├── orchestrator/# 多能力编排
│   ├── plugin/      # 插件调用映射
│   ├── sense/       # 感知/事件能力
│   └── tool/        # 工具调用接口
├── examples/        # 可复现示例
├── plugins/         # 插件公共约定
└── wit/             # WIT 接口映射
```

## 安全 / 禁止事项

- 不提交真实模型 API Key、测试密钥、签名密钥和用户隐感数据。  
- 示例输出、日志中不得保留会话明文、Token 或路径级凭据。  
- 外部服务调用需保留超时与失败回退边界。  

## 提交与 PR 规范

- 提交信息默认中文，单次变更聚焦单一能力域（如 LLM、TTS、插件映射等）。  
- PR 必须给出 `cargo test` 与关键 `cargo run --example ...` 的结果。  
- 修改公共接口时，附明示兼容性影响与回退方案。  

## 项目特有坑点

- 外部模型服务依赖较强，示例运行需具备网络与凭据条件。  
- 与上层世界观语义口径不一致时，常见症状是流式响应静默或上下文丢失。  
- WIT 字段变更会联动 `tool_fcplug` 与插件仓库，需统一同步验证。  

## 文档同步依据（本次核对）

- 同步时间：2026-06-03 21:04:46 +08:00
- 依据文件：`core_ai_client/Cargo.toml`、`core_ai_client/src`、`core_ai_client/examples`、`core_ai_client/plugins`、`core_ai_client/wit`
