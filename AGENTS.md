# flowcloudai_client_core — AGENTS.md

> 本文件面向 AI 编程助手。如果你对该项目一无所知，请先阅读本文档再动手修改代码。

---

## 一、项目概览

**flowcloudai_client_core**（Rust crate 名：`flowcloudai_client`）是一个面向内容创作工具的 **AI 多模态编排引擎**。它以 Rust 库的形式提供统一接口，封装了 LLM 对话、图像生成、语音合成、音频解码播放等能力。

核心设计哲学：
- **统一接口**：无论后端是 OpenAI、千问、DeepSeek 还是其他供应商，上层调用方式一致。
- **WASM 插件隔离**：不同供应商的 API 协议差异通过 WASM 组件（`.fcplug` 插件）自动映射，核心代码零改动即可接入新供应商。
- **流式优先**：原生支持 SSE 流式解码，事件驱动架构让 UI 能秒级收到内容片段。
- **工具链与编排**：AI 可主动调用自定义工具（ToolRegistry），并支持通过 Sense + TaskOrchestrator 动态装配每轮对话的 Prompt、工具白名单和参数。
- **可选持久化**：对话历史可以按 JSON 文件形式落盘，支持树状分支、重说、回退。

---

## 二、技术栈与关键依赖

- **语言**：Rust（Edition 2024）
- **异步运行时**：Tokio（`full` feature）
- **WASM 引擎**：`wasmtime` + `wasmtime-wasi`（Component Model，版本 42）
- **HTTP 客户端**：`reqwest`（支持 JSON、stream、gzip）
- **序列化**：`serde` / `serde_json`
- **错误处理**：`anyhow`
- **音频**：`cpal`（播放）、`symphonia`（解码 MP3/WAV/FLAC/AAC/PCM）
- **其他**：`zip`（读取 `.fcplug`）、`futures-util`、`tokio-stream`、`tokio-util`、`chrono`、`base64`、`hex`

---

## 三、代码组织与模块划分

```
src/
├── lib.rs                 # 库入口，聚合所有 public mod 并 re-export 关键类型
├── client.rs              # FlowCloudAIClient：主入口、插件管理、Session 工厂、历史管理
├── http_poster.rs         # HTTP 请求封装（reqwest 之上）
├── storage.rs             # ConversationStore：基于 JSON 文件的对话持久化
│
├── llm/                   # LLM 会话核心
│   ├── mod.rs
│   ├── session.rs         # LLMSession：对话驱动循环、SSE 解码、工具调用
│   ├── handle.rs          # SessionHandle：外部句柄（异步读写参数、checkout、切换插件）
│   ├── types.rs           # ChatRequest / ChatResponse / Message / SessionEvent / TurnStatus 等
│   ├── config.rs          # SessionConfig（超时、buffer、max_tool_rounds 等）
│   ├── tree.rs            # ConversationTree：消息历史树（支持分支/回退/重说）
│   ├── accumulator.rs     # ToolCallAccumulator：流式工具调用片段聚合
│   └── stream_decoder.rs  # SSE 流解码器，将字节流转为 SessionEvent
│
├── image/                 # 图像生成
│   ├── mod.rs
│   ├── session.rs         # ImageSession（无状态，可复用）
│   └── types.rs           # ImageRequest / ImageResult
│
├── tts/                   # 语音合成
│   ├── mod.rs
│   ├── session.rs         # TTSSession（无状态，可复用）
│   └── types.rs           # TTSRequest / TTSResult / VoiceSetting
│
├── audio/                 # 音频解码与播放
│   ├── mod.rs
│   └── decoder.rs         # AudioDecoder / AudioSource / DecodedAudio
│
├── plugin/                # WASM 插件系统
│   ├── mod.rs
│   ├── types.rs           # PluginMeta / PluginKind / Manifest 等
│   ├── scanner.rs         # 扫描 `.fcplug` 并读取 manifest
│   ├── manager.rs         # PluginManager：旧版管理器（仍被 examples 引用）
│   ├── registry.rs        # PluginRegistry：编译/加载/实例池/引用计数管理
│   ├── pool.rs            # MapperPool：单个插件的 WASM 实例池（RAII 归还）
│   ├── pipeline.rs        # ApiPipeline：对 PluginRegistry 的封装，自动维护引用计数
│   ├── mapper.rs          # ApiMapper trait / WasmMapper 实现
│   ├── bindings.rs        # wit-bindgen 生成的绑定代码
│   ├── host.rs            # HostState（wasmtime Store 的上下文）
│   └── loaded.rs          # LoadedPlugin 旧兼容结构
│
├── tool/                  # 工具调用框架
│   ├── mod.rs
│   ├── registry.rs        # ToolRegistry：注册/启用/禁用/执行工具
│   ├── types.rs           # ToolSpec / ToolFunctionArg
│   └── executor.rs        # 工具执行器（超时控制等）
│
├── orchestrator/          # 任务编排
│   ├── mod.rs
│   ├── orchestrator.rs    # TaskOrchestrator：根据 TaskContext 装配 AssembledTurn
│   └── context.rs         # TaskContext / AssembledTurn
│
└── sense/                 # 模式预设（Sense trait）
    └── mod.rs             # Sense trait 定义
```

`examples/` 目录包含大量可运行的示例：
- `main.rs`、`plugin_management.rs`、`llm.rs`、`llm_ai_dialogue.rs`、`image.rs`、`tts.rs`
- `senses/` 下有几个 Sense 实现示例（`llm_a.rs`、`llm_b.rs`、`militech_acs.rs`）

---

## 四、构建与运行命令

```bash
# 构建库
cargo build

# 发布构建（release profile 已配置 thin LTO、strip symbols、panic=abort）
cargo build --release

# 运行示例
cargo run --example main
cargo run --example llm
cargo run --example plugin_management

# 测试
cargo test
```

**注意**：
- 运行需要 `plugins/` 目录中有合法的 `.fcplug` 插件（项目仓库已附带 `deepseek-llm.fcplug`、`qwen-llm.fcplug` 等）。
- 部分示例需要真实 API key 才能实际调用远程服务，否则会在 HTTP 请求阶段失败。
- 目前项目中 **没有 CI/CD 配置**（没有 `.github/workflows` 等）。

---

## 五、代码风格与开发约定

### 5.1 注释与文档语言
- **所有代码注释、文档字符串、markdown 文档均使用中文**。新增代码请保持这一惯例。
- 模块级注释常用 `// ═════════════════════════════════════` 或 `// ── 标题 ──` 的分隔风格。

### 5.2 错误处理
- 统一使用 `anyhow::{Result, anyhow}`。公共 API 返回 `anyhow::Result<T>`，内部错误通过 `anyhow!("...")` 或 `.context(...)?` 传播。
- 不要滥用 `unwrap()`；在确实不可恢复的内部逻辑中可以使用，但公共路径必须返回 `Result`。

### 5.3 并发与同步
- 会话状态（`ChatRequest`、`ConversationTree`）被包装在 `Arc<RwLock<...>>` 中，供 `SessionHandle` 异步读写。
- `ToolRegistry` 和 `PluginRegistry` 也被包装在 `Arc` 中，供多个 Session 共享。
- 插件实例池使用 `Mutex` 仅保护 `Vec::pop/push`，WASM 计算本身在借出后无锁并行。

### 5.4 命名规范
- 遵循标准 Rust 命名：`PascalCase`（类型/模块）、`snake_case`（函数/变量）、`SCREAMING_SNAKE_CASE`（常量）。
- 事件/状态类型集中在对应 `types.rs` 中定义。

---

## 六、测试策略

### 6.1 现有测试
- `src/llm/tree.rs` 包含较完整的单元测试（`#[cfg(test)] mod tests`），覆盖空树、线性追加、checkout、分支、path_to、重说等场景。
- `src/plugin/pool.rs` 只有测试结构占位（需要真实 wasm 模块才能跑）。
- **其余模块目前缺少自动化单元测试**。

### 6.2 测试建议
- 对纯数据结构（如 `ConversationTree`、`ToolRegistry` 的工具列表操作、`AssembledTurn` 的装配逻辑）优先补充单元测试。
- 对依赖 HTTP 或 WASM 的模块（`LLMSession`、`ApiPipeline`、`StreamDecoder`），建议：
  1. 提取纯逻辑到不依赖 IO 的函数中并单独测试；
  2. 使用 mock HTTP server 或 mock mapper 做集成测试；
  3. 避免在单元测试中调用真实付费 API。

### 6.3 手动验证
- 修改核心后，务必编译并运行 `cargo run --example main` 或 `cargo run --example llm`，确认插件能正常加载、mapper 能正确实例化。

---

## 七、WASM 插件系统（关键机制）

### 7.1 接口定义
`wit/plugin.wit` 定义了插件必须实现的三个函数：
```wit
interface mapper {
    map-request: func(json: string) -> string;
    map-response: func(json: string) -> string;
    map-stream-line: func(line: string) -> string;
}
```

### 7.2 插件包格式
`.fcplug` 本质是一个 ZIP 文件，内部必须包含：
- `manifest.json` — 元数据（id、kind、version、abi-version、url 等）
- `plugin.wasm` — 编译好的 WASM 组件（target: `wasm32-wasip1`）
- `icon.png` — 可选图标

当前 ABI 版本：`SUPPORTED_ABI_VERSION = 2`（定义在 `src/lib.rs`）。

### 7.3 生命周期与引用计数
- `PluginRegistry` 维护每个插件的引用计数（`ref_counts: Arc<Mutex<HashMap<String, usize>>>`）。
- `ApiPipeline` 在创建时自动 `+1`，在 `Drop` 时自动 `-1`。
- 因此：**安装/卸载插件需要 `&mut self` 且当前没有活跃 Session 持有 `Arc<PluginRegistry>`**，否则返回明确错误。
- 卸载插件前若引用计数 `> 0`，会报错 `"still in use by N session(s)"`。

---

## 八、安全与风险注意事项

1. **WASM 沙箱**：插件在 wasmtime 组件模型中运行，与宿主进程内存隔离。但不要假设插件代码完全可信，ABI 版本检查和 manifest 校验已作为第一道防线。
2. **API Key 管理**：`SessionConfig` 中 `api_key` 以纯 `String` 形式存储在内存中。目前未做加密或安全擦除，上层应用如需高安全等级应自行处理。
3. **工具执行超时**：`ToolRegistry::conduct` 支持传入 `Duration` 做超时控制，默认 LLMSession 中设置为 60 秒。新增工具时请确保其幂等性，避免重复调用产生副作用。
4. **文件系统安全**：`ConversationStore` 直接在指定目录读写 `{id}.json`。ID 由内部生成（毫秒级时间戳），但如果暴露给外部输入，需防范路径遍历（当前实现已使用 `PathBuf::join` 拼接，不直接信任外部 ID 作为文件名）。
5. **HTTP 流控**：SSE 流式解码设置了 `max_line_bytes`（默认 1MB），防止异常流导致内存无限增长。

---

## 九、常用类型速查

如果你需要快速定位关键类型，以下是 re-export 列表（来自 `src/lib.rs`）：

| 类型 | 来源路径 |
|------|----------|
| `FlowCloudAIClient` | `src/client.rs` |
| `LLMSession` | `src/llm/session.rs` |
| `SessionHandle` | `src/llm/handle.rs` |
| `SessionEvent`、`TurnStatus`、`ThinkingType` | `src/llm/types.rs` |
| `ToolRegistry` | `src/tool/registry.rs` |
| `ImageSession` | `src/image/session.rs` |
| `TTSSession` | `src/tts/session.rs` |
| `AudioDecoder`、`AudioSource` | `src/audio/decoder.rs` |
| `ConversationStore`、`ConversationMeta`、`StoredConversation`、`StoredMessage` | `src/storage.rs` |
| `PluginKind`、`PluginManager`、`PluginScanner`、`LoadedPlugin` | `src/plugin/` 下各模块 |
| `SUPPORTED_ABI_VERSION` | `src/lib.rs`（常量 `2`） |

---

## 十、修改代码前的检查清单

- [ ] 是否阅读了相关模块的 `types.rs`？新增字段通常需要同步修改序列化/反序列化逻辑。
- [ ] 是否影响了 `SessionEvent`？如果是，请检查 `stream_decoder.rs` 和 `session.rs` 中的事件分发逻辑。
- [ ] 是否修改了插件相关接口？如果是，请同步检查 `wit/plugin.wit` 和所有 `.fcplug` 插件的兼容性。
- [ ] 是否新增了公共 API？请用中文编写文档注释，并考虑在 `examples/` 增加使用示例。
- [ ] 是否对纯数据结构做了修改？尽量补充单元测试（参考 `src/llm/tree.rs` 的测试风格）。
- [ ] 运行 `cargo build` 和 `cargo test` 确认无编译错误、无测试失败。
