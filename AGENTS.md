# core_ai_client — AGENTS.md

## 项目概览

`core_ai_client` 是 FlowCloudAI 的 AI 会话核心库，统一封装文本、多模态与工具编排能力，承担桌面端、网站端与插件协议的能力一致性。

## 构建 / 运行 / 测试 / lint

```bash
cd core_ai_client
cargo build
cargo build --release
cargo test
```

```bash
cargo run --example main
cargo run --example plugin_management
cargo run --example llm
cargo run --example llm_ai_dialogue
cargo run --example image
cargo run --example tts
cargo run --example orchestrate
```

该仓库未提供统一 lint 命令，依赖 `cargo test` 与人工 review 作质量关口。

## 代码风格与命名约定

- Rust 使用 2024 Edition。  
- 类型 `PascalCase`，函数与变量 `snake_case`，常量 `SCREAMING_SNAKE_CASE`。  
- 异步与错误传播保留上下文，禁止吞掉模型调用异常细节。  
- WIT/插件能力入口保持语义稳定，避免不必要的公共 API 变更。  

## 目录结构与模块职责

```text
core_ai_client/
├── src/              # 会话、LLM、工具与能力路由
├── examples/         # 可复现示例入口（main/plugin_management/llm/...）
├── plugins/          # 插件公共约定
├── wit/              # WIT 类型与接口映射
└── docs/             # 文档与维护说明
```

## 安全 / 禁止事项

- 禁止提交真实模型 API Key、测试密钥、签名密钥及会话明文。  
- 示例与日志不得包含用户敏感内容与可追踪凭证。  
- 模型调用必须保留超时、重试和降级路径。  

## 提交与 PR 规范

- 提交信息默认中文，单次变更聚焦单一能力域。  
- PR 需附 `cargo test` 与关键 `example` 运行命令，包含参数和异常复现。  
- 修改公共接口时补充兼容性评估和回退策略。  

## 项目特有坑点

- 模型和工具链依赖较重，示例运行通常需要外部凭据或本地代理环境。  
- 能力语义漂移会直接影响会话流式返回、上下文管理与错误恢复。  
- WIT/插件协议变更需同步 `tool_fcplug` 与 `plugins` 进行联调。  

文档同步时间：2026-06-08 13:20:10 +08:00
