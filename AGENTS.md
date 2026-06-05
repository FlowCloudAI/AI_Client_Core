# core_ai_client — AGENTS.md

## 项目概览

`core_ai_client` 是 FlowCloudAI 的 AI 核心库，统一封装文本、图片、语音与工具编排接口，并负责 `.fcplug` 运行时能力映射稳定性。  
上层应用（桌面端、站点、插件）通过该库消费能力能力，故错误语义和兼容边界是高优先级约束。

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

- Rust 使用 2024 Edition，类型 `PascalCase`，函数/变量 `snake_case`，常量 `SCREAMING_SNAKE_CASE`。  
- 异步调用与错误传播优先保留上下文，便于跨层排障。  
- 面向公共 API 的结构体/枚举保持语义稳定，避免无序重构。  

## 目录结构与模块职责

```text
core_ai_client/
├── src/
│   ├── audio/       # 音频能力与解码/回放
│   ├── image/       # 图片输入与格式处理
│   ├── llm/         # 大模型文本能力
│   ├── orchestrator/# 能力编排
│   ├── plugin/      # 插件映射与适配
│   ├── sense/       # 感知与事件能力
│   └── tool/        # 工具调用接口
├── examples/        # 可复现示例
├── plugins/         # 插件公共约定
└── wit/             # WIT 接口映射
```

## 安全 / 禁止事项

- 禁止提交真实模型 API Key、测试密钥、签名密钥及会话明文。  
- 示例日志不得包含用户敏感内容或可追踪凭据。  
- 外部模型调用需显式配置超时、重试与失败回退。  

## 提交与 PR 规范

- 提交信息默认中文，单次变更聚焦单一能力域（如 LLM、TTS、图片、工具）。  
- PR 必须附 `cargo test` 与关键 `cargo run --example ...` 的结果与参数。  
- 修改公共接口时同步补充兼容性说明与回退策略。  

## 项目特有坑点

- 模型和工具链依赖较重，示例与测试通常需要网络与凭据条件。  
- 能力语义漂移会直接影响会话流式返回、上下文与错误恢复。  
- WIT 定义变更需与 `tool_fcplug` 与插件仓库联调。  

文档同步时间：2026-06-05 12:44:21 +08:00
