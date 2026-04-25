# flowcloudai_client_core 完整文档

## 🚀 快速预览

### 是什么？
**flowcloudai_client_core** 是内容创作工具的 **AI 驱动引擎**。提供：

- 🧠 **LLM 智能对话**：支持多个 AI 供应商（通过 WASM 插件适配）
- 🎨 **图像生成**：文生图、图文编辑、多图融合
- 🎤 **语音合成**：文本转语音，支持多种音色和语言
- 🎵 **音频播放**：MP3/WAV/FLAC 解码和播放
- 🛠️ **工具调用**：AI 能主动调用自定义工具（搜索、计算等）
- 🎯 **任务编排**：动态装配每轮对话的上下文和工具权限
- 🔌 **WASM 插件**：无代码适配新的 AI 供应商

### 核心特性
| 特性 | 说明 |
|------|------|
| **多模态** | LLM + 图像 + 语音 + 音频 |
| **流式处理** | SSE 流式响应解码，秒级反馈 |
| **工具链** | 工具库 + 执行引擎 + 结果回源 |
| **插件适配** | WASM 组件模型，请求/响应自动映射 |
| **会话管理** | 对话消息树、分支/回退/重说、外部句柄 |
| **编排灵活** | Sense 预设 + `Orchestrate` trait 动态装配，完全可自定义 |

### 五分钟上手

```rust
use flowcloudai_client::FlowCloudAIClient;
use flowcloudai_client::plugin::types::PluginKind;
use std::path::PathBuf;
use tokio::sync::mpsc;
use futures_util::StreamExt;

#[tokio::main]
async fn main() -> Result<()> {
    // 1. 初始化客户端（插件目录 + 可选的对话存储目录）
    let client = FlowCloudAIClient::new(
        PathBuf::from("./plugins"),
        Some(PathBuf::from("./conversations")), // None = 不持久化
    )?;

    // 2. 列出可用的 LLM 插件
    for (id, meta) in client.list_by_kind(PluginKind::LLM) {
        println!("LLM: {} ({})", id, meta.name);
    }

    // 3. 创建 LLM 会话
    let mut session = client.create_llm_session("openai", "sk-...")?;

    // 4. 启动会话（通过 SessionHandle 设置模型和参数）
    let (input_tx, input_rx) = mpsc::channel::<String>(32);
    let (mut event_stream, handle) = session.run(input_rx);

    // 5. 通过 handle 异步设置模型和参数
    handle.set_model("gpt-4").await?;
    handle.set_temperature(0.7).await?;
    handle.set_max_tokens(2000).await?;

    // 发送消息
    input_tx.send("用 Markdown 写一篇关于 AI 的短文".to_string()).await?;

    // 处理事件流
    while let Some(event) = event_stream.next().await {
        match event {
            SessionEvent::ContentDelta(delta) => print!("{}", delta),
            SessionEvent::TurnEnd { .. } => println!("\n[完成]"),
            SessionEvent::ToolCall { name, .. } => {
                println!("\n[调用工具: {}]", name);
            }
            _ => {}
        }
    }

    Ok(())
}
```

### 架构速查表

```
FlowCloudAIClient (主入口)
  ├── PluginRegistry (插件注册中心)
  │   ├── LLM Plugin (OpenAI/千问/等)
  │   ├── Image Plugin (火山方舟/等)
  │   └── TTS Plugin (MiniMax/等)
  │
  ├── ToolRegistry (全局工具库)
  │   ├── Tool #1
  │   ├── Tool #2
  │   └── ...
  │
  ├── ConversationStore (对话持久化，可选)
  │   └── {conversation_id}.json × N
  │
  └── Session 工厂
      ├── LLMSession (对话，TurnEnd Ok 时自动写盘)
      ├── ImageSession (图像)
      ├── TTSSession (语音)
      └── AudioDecoder (音频)
```

---

# 📖 完整文档

## 一、项目简介

### 1.1 背景与目标

**flowcloudai_client_core** 是一个高效、灵活的 **AI 多模态编排框架**，为内容创作工具提供统一的 AI 能力接入。

**核心目标：**
- 🎯 **统一接口**：为多个 AI 供应商（OpenAI、千问、火山方舟等）提供统一 API
- 🚀 **低集成成本**：通过 WASM 插件自动适配新供应商，无需修改核心代码
- ⚡ **流式响应**：原生支持 SSE 流式解码，秒级内容反馈
- 🛠️ **工具生态**：AI 可主动调用自定义工具（搜索、数据查询、代码执行等）
- 🎭 **动态编排**：根据任务类型自动装配 Prompt、工具权限、参数配置
- 🔒 **会话管理**：完整的对话历史、状态机、外部访问句柄

### 1.2 适用场景

1. **内容创作工具**
    - 智能文案生成、编辑建议、格式转换
    - 配图、配音自动化

2. **AI Agent 系统**
    - 多步骤任务自动化（搜索 → 分析 → 生成）
    - 工具链编排

3. **多模态应用**
    - 文生图、图文混排、配音视频生成

4. **供应商聚合**
    - 在多个 LLM API 间灵活切换
    - 成本优化（便宜的用来生成，贵的用来改进）

---

## 二、核心架构

### 2.1 系统分层

```
┌─────────────────────────────────────────────────┐
│         应用层（UI / API Gateway）              │
└────────────────────┬────────────────────────────┘
                     │
┌────────────────────▼────────────────────────────┐
│        flowcloudai_client_core 核心             │
│  ┌──────────┐  ┌──────────┐  ┌──────────────┐  │
│  │LLMSession│  │ImageSes..│  │TTSSession...│  │
│  │          │  │          │  │              │  │
│  └──────────┘  └──────────┘  └──────────────┘  │
│       ▲              ▲               ▲          │
│       │              │               │          │
│  ┌────┴──────────────┴───────────────┴────┐   │
│  │     ApiPipeline（WASM 插件映射）      │   │
│  └────────────────────────────────────────┘   │
│       ▲                                        │
│  ┌────┴────────────────────────────────────┐  │
│  │  PluginRegistry（注册中心 + 实例池）    │  │
│  │  ToolRegistry（工具库）                 │  │
│  │  Orchestrate trait / DefaultOrchestrator │  │
│  └────────────────────────────────────────┘  │
└────────────────────┬────────────────────────────┘
                     │
┌────────────────────▼────────────────────────────┐
│      HTTP / WASM / 本地操作系统                 │
│  OpenAI / 千问 / 火山方舟 / ... / 音频设备     │
└─────────────────────────────────────────────────┘
```

### 2.2 核心概念

| 概念 | 作用 | 例子 |
|------|------|------|
| **Plugin** | WASM 组件，自动适配 API 协议差异 | `openai` 插件将标准 ChatRequest 映射到 OpenAI API 格式 |
| **Session** | 无状态的单次请求处理器 | LLMSession、ImageSession、TTSSession |
| **ToolRegistry** | 全局工具库，AI 可调用 | search_web, execute_code, query_database |
| **Sense** | 模式预设（提示词 + 工具白名单） | creative_writing, proofreading, code_assistant |
| **Orchestrate** | 动态装配接口（trait） | 实现 `assemble(ctx)` 即可完全自定义每轮配置 |
| **DefaultOrchestrator** | 内置编排实现 | 基于 Sense 白名单 + task_type 的标准策略 |
| **ConversationTree** | 消息历史树 | 链式父节点结构，checkout 任意节点实现分支/重说/回退 |
| **SessionHandle** | 会话外部句柄 | 异步读写对话参数、切换插件、历史 checkout |

---

## 三、核心组件详解

### 3.1 FlowCloudAIClient（主入口）

**职责**：初始化系统、加载插件、创建各类 Session、管理对话历史

```rust
pub struct FlowCloudAIClient {
    plugin_registry: Arc<PluginRegistry>,
    tool_registry: Arc<ToolRegistry>,
    storage: Option<Arc<ConversationStore>>,  // 新增
}

impl FlowCloudAIClient {
    // 初始化。storage_path = Some(dir) 时启用本地持久化，None 时关闭
    pub fn new(plugins_dir: PathBuf, storage_path: Option<PathBuf>) -> Result<Self>;

    // 列出指定类型的插件（LLM / Image / TTS）
    pub fn list_by_kind(&self, kind: PluginKind) -> Vec<(&String, &PluginMeta)>;

    // 创建 LLM 会话（已启用 storage 时自动注入存储上下文）
    pub fn create_llm_session(&self, plugin_id: &str, api_key: &str) -> Result<LLMSession>;

    // 创建编排模式 LLM 会话（编排器嵌入 session，每轮自动 assemble）
    pub fn create_orchestrated_session(
        &self, plugin_id: &str, api_key: &str,
        orchestrator: Box<dyn Orchestrate>,
    ) -> Result<LLMSession>;

    // 同上 + 同时加载 Sense（async，内部顺序执行 load_sense）
    pub async fn create_orchestrated_session_with_sense(
        &self, plugin_id: &str, api_key: &str,
        sense: impl Sense,
        orchestrator: Box<dyn Orchestrate>,
    ) -> Result<LLMSession>;

    // 创建图像会话
    pub fn create_image_session(&self, plugin_id: &str, api_key: &str) -> Result<ImageSession>;

    // 创建语音会话
    pub fn create_tts_session(&self, plugin_id: &str, api_key: &str) -> Result<TTSSession>;

    // 安装工具到全局库
    pub fn install_sense(&mut self, sense: &dyn Sense) -> Result<()>;

    // ── 对话历史管理（需 storage_path 已配置）──────────────────

    // 列出所有已保存对话的元信息，按 updated_at 降序
    pub fn ai_list_conversations(&self) -> Vec<ConversationMeta>;

    // 返回完整对话（含消息列表），未找到返回 None
    pub fn ai_get_conversation(&self, id: &str) -> Option<StoredConversation>;

    // 删除对话文件
    pub fn ai_delete_conversation(&self, id: &str) -> Result<()>;

    // 重命名对话（修改 title + 更新 updated_at）
    pub fn ai_rename_conversation(&self, id: &str, title: String) -> Result<()>;
}
```

**使用示例**：

```rust
// 启用持久化
let client = FlowCloudAIClient::new(
    PathBuf::from("./plugins"),
    Some(PathBuf::from("./conversations")),
)?;

// 创建会话（自动在每轮成功结束后写盘）
let mut session = client.create_llm_session("openai", "sk-xxx")?;

// 查询历史
let list = client.ai_list_conversations();
for meta in &list {
    println!("{} — {} ({})", meta.id, meta.title, meta.updated_at);
}

// 读取完整对话
if let Some(conv) = client.ai_get_conversation(&list[0].id) {
    for msg in &conv.messages {
        println!("[{}] {}", msg.role, msg.content.as_deref().unwrap_or(""));
    }
}
```

### 3.2 PluginRegistry（插件注册中心）

**职责**：管理 WASM 插件的加载、编译、实例化

**关键设计**：
- 每个插件的 WASM 模块只编译一次（重）
- 每个请求创建一个独立的 Store 实例（轻）
- 实例池回收利用，减少创建开销

```rust
pub struct PluginRegistry {
    plugins: HashMap<String, PluginMeta>,
    pools: HashMap<String, MapperPool>,
    engine: Engine,                    // 全局 WASM 引擎
    modules: HashMap<String, Component>, // 编译好的模块
    max_idle_per_pool: usize,          // 每个池的最大空闲实例数
}

impl PluginRegistry {
    // 构建 Registry（编译所有 wasm 模块）
    pub fn build(
        engine: Engine,
        linker: Linker<HostState>,
        plugin_metas: HashMap<String, PluginMeta>,
        max_idle_per_pool: usize,
    ) -> Result<Self>;

    // 激活插件（创建实例池）
    pub fn load(&mut self, id: &str) -> Result<()>;

    // 从池中借出 mapper（自动归还）
    pub fn acquire(&self, plugin_id: &str) -> Result<PooledMapper<'_>>;

    // 获取插件元数据和配置
    pub fn get_meta(&self, plugin_id: &str) -> Option<&PluginMeta>;
}
```

**插件格式** (`.fcplug` = ZIP 文件)：
```
plugin.fcplug
├── manifest.json       # 元数据：id, version, kind, url 等
├── plugin.wasm         # 编译后的 WASM 组件
└── icon.png           # 插件图标（可选）
```

**manifest.json 示例**：
```json
{
  "meta": {
    "id": "openai",
    "name": "OpenAI Plugin",
    "kind": "kind/llm",
    "abi-version": 2,
    "version": "1.0.0",
    "author": "FlowCloud",
    "url": "https://api.openai.com/v1/chat/completions"
  },
  "llm": {
    "models": ["gpt-4", "gpt-3.5-turbo"],
    "default_model": "gpt-4",
    "supports_thinking": true,
    "supports_tools": true
  }
}
```

### 3.3 LLMSession（核心会话）

**职责**：管理对话历史、处理请求/响应、执行工具调用

```rust
pub struct LLMSession {
    client: HttpPoster,
    conversation: Arc<RwLock<ChatRequest>>,
    tool_registry: Arc<ToolRegistry>,
    config: SessionConfig,
    pipeline: ApiPipeline,
    turn_id: u64,
    orchestrator: Option<Box<dyn Orchestrate>>,  // trait object，可自定义
}
```

**生命周期**：

```
1. 创建         → client.create_llm_session()
                  或 client.create_orchestrated_session(..., Box::new(orch))
2. 配置         → session.set_model().await, set_temperature().await, etc.
3. 加载 Sense   → session.load_sense(my_sense).await?
4. 设置编排器   → session.set_orchestrator(orch) 或 session.with_orchestrator(orch)
                  （create_orchestrated_session 已自动完成此步）
5. 启动会话     → session.run(input_rx) 返回事件流 + 句柄
6. 发送消息     → 通过 input_tx 发送用户输入
7. 推送上下文   → handle.set_task_context(ctx).await（可选，每轮前更新）
8. 处理事件     → 处理 ContentDelta、ToolCall、ToolResult 等
9. 关闭会话     → 事件流结束时自动关闭
```

**完整使用示例**：

```rust
let mut session = client.create_llm_session("openai", "sk-xxx")?;

// 加载工作流模式
session.load_sense(CreativeWritingSense).await?;

// 创建消息通道，启动会话
let (input_tx, input_rx) = mpsc::channel::<String>(32);
let (mut event_stream, handle) = session.run(input_rx);

// 通过 handle 设置模型和参数
handle.set_model("gpt-4").await?;
handle.set_temperature(0.8).await?;
handle.set_max_tokens(2000).await?;
let (mut event_stream, _handle) = session.run(input_rx);

// 发送用户消息
input_tx.send("帮我写一段产品文案".to_string()).await?;

// 处理事件流（流式响应）
while let Some(event) = event_stream.next().await {
    match event {
        SessionEvent::ContentDelta(text) => {
            println!("{}", text);
        }
        SessionEvent::ToolCall { index, name } => {
            println!("调用工具: {}", name);
            // 工具执行结果会自动作为 ToolResult 事件返回
        }
        SessionEvent::ToolResult { index, output, is_error } => {
            if is_error {
                eprintln!("工具执行出错: {}", output);
            } else {
                println!("工具结果: {}", output);
            }
        }
        SessionEvent::TurnEnd { status } => {
            println!("对话结束: {:?}", status);
        }
        _ => {}
    }
}
```

**流程图**：

```
session.run(input_rx) 启动驱动循环，返回 (event_stream, handle)
   ↓
后台线程（独立 tokio runtime）
   │
   ├─ 发送 NeedInput 事件（通知 UI 可以输入了）
   │
   ├─ [ctrl_rx.try_recv()] 处理所有待处理的控制指令
   │   └─ SwitchPlugin { plugin_id, api_key }
   │        → 更新 config.base_url / api_key
   │        → pipeline.set_plugin()（维护引用计数）
   │
   └─ 接收用户消息 (via input_tx.send(...))
        ↓
     ┌─────────────────────────────┐
     │  处理每轮对话               │
     ├─────────────────────────────┤
     │ 1. 追加用户消息到 conversation
     │ 2. conversation → JSON      │
     │ 3. 插件映射 (map_request)   │
     │ 4. HTTP POST to LLM API     │
     │ 5. SSE 流解码               │
     │ 6. 发出事件                 │
     │    ├─ TurnBegin             │
     │    ├─ ContentDelta          │
     │    ├─ ReasoningDelta        │
     │    ├─ ToolCall              │
     │    ├─ ToolResult            │
     │    └─ TurnEnd               │
     │ 7. 追加 assistant 消息      │
     │    到 conversation          │
     └─────────────────────────────┘
   ↓
[工具调用？]
  ├─ Yes → [ToolRegistry::conduct] 自动执行工具
  │        → [ToolResult] 事件返回结果
  │        → 重回流程继续对话
  └─ No  → [TurnEnd] 等待下一条 NeedInput
```

### 3.4 ToolRegistry（工具库）

**职责**：注册和管理可被 AI 调用的工具

```rust
pub struct ToolRegistry {
    tools: HashMap<String, ToolSpec>,
    state: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl ToolRegistry {
    pub fn new() -> Self;

    // 注册同步工具
    pub fn register<T, F>(
        &mut self,
        name: &str,
        description: &str,
        properties: impl Into<Option<Vec<ToolFunctionArg>>>,
        handler: F,
    ) where
        F: Fn(&mut T, &Value) -> Result<String> + Send + Sync + 'static;

    // 注册异步工具
    pub fn register_async<T, F>(
        &mut self,
        name: &str,
        description: &str,
        properties: impl Into<Option<Vec<ToolFunctionArg>>>,
        handler: F,
    ) where
        F: for<'a> Fn(&mut T, &'a Value) -> BoxFuture<'a, Result<String>> + Send + Sync + 'static;

    // 获取所有已启用工具的 JSON Schema
    pub fn schemas(&self) -> Option<Vec<Value>>;

    // 只获取指定工具名的 Schema（白名单筛选），且仅返回启用的工具
    pub fn schemas_filtered(&self, whitelist: &[String]) -> Option<Vec<Value>>;

    // 启用/禁用指定工具（返回 true 表示工具存在且操作成功）
    pub fn enable_tool(&mut self, name: &str) -> bool;
    pub fn disable_tool(&mut self, name: &str) -> bool;

    // 查询指定工具是否启用（工具不存在视为 false）
    pub fn is_enabled(&self, name: &str) -> bool;

    // 执行工具调用（含超时控制）
    /// - `timeout`: 工具执行的最大允许时间；超出时间则返回超时错误
    /// - 若工具已禁用，返回错误 "工具已禁用: {name}"
    pub async fn conduct(
        &self,
        func_name: &str,
        args: Option<&Value>,
        timeout: Duration,
    ) -> Result<String>;
}
```

**工具注册示例**：

```rust
let mut registry = ToolRegistry::new();

// 工具状态结构
#[derive(Default)]
struct SearchState {
    api_key: String,
}

// 注册工具
registry.register::<SearchState, _>(
    "search_web",
    "在网络上搜索信息",
    Some(vec![
        ToolFunctionArg::new("query", "string")
            .required(true)
            .desc("搜索关键词"),
    ]),
    |state, args| {
        let query = arg_str(args, "query")?;
        let results = state.search(query)?;
        Ok(serde_json::to_string(&results)?)
    },
);
```

**AI 工具调用流程**：

```
LLM 响应包含 tool_calls
   ↓
[StreamDecoder] 解析，发出 ToolCall 事件
   ↓
[事件流通知应用] SessionEvent::ToolCall { index, name }
   ↓
[应用处理] 可选地获取工具调用详情
   ↓
[LLMSession 内部] 自动通过 ToolRegistry::conduct 执行工具（60s 超时）
   ↓
[发出 ToolResult 事件] SessionEvent::ToolResult { index, output, is_error }
   ↓
[LLMSession] 追加 tool message 到 conversation
   ↓
[内部循环] 继续与 LLM 交互直到 TurnEnd
```

### 3.5 Sense（模式预设）

**职责**：为特定工作流预设系统提示词、默认参数、工具白名单

```rust
pub trait Sense: Send + Sync {
    /// 系统提示词列表
    fn prompts(&self) -> Vec<String>;

    /// 默认的 ChatRequest 配置
    fn default_request(&self) -> Option<ChatRequest> {
        None
    }

    /// 向全局 ToolRegistry 注册本模式的工具
    fn install_tools(&self, registry: &mut ToolRegistry) -> Result<()>;

    /// 本模式的工具白名单（None = 全部可用）
    fn tool_whitelist(&self) -> Option<Vec<String>> {
        None
    }
}
```

**实现示例**：

```rust
struct CreativeWritingSense;

impl Sense for CreativeWritingSense {
    fn prompts(&self) -> Vec<String> {
        vec![
            "你是一位资深的创意文案写手。".to_string(),
            "你的目标是生成引人入胜、富有感染力的内容。".to_string(),
        ]
    }

    fn default_request(&self) -> Option<ChatRequest> {
        let mut req = ChatRequest::default();
        req.temperature = Some(0.85);  // 提高创意性
        req.top_p = Some(0.95);
        Some(req)
    }

    fn install_tools(&self, registry: &mut ToolRegistry) -> Result<()> {
        // 注册 search_references, generate_ideas 等工具
        Ok(())
    }

    fn tool_whitelist(&self) -> Option<Vec<String>> {
        Some(vec![
            "search_references".to_string(),
            "generate_ideas".to_string(),
        ])
    }
}

// 使用
session.load_sense(CreativeWritingSense).await?;
```

### 3.6 Orchestrate（任务编排接口）

**职责**：每轮对话开始前，根据 `TaskContext` 动态装配 Prompt、工具权限、参数覆盖。

`Orchestrate` 是 **trait**，调用方可完全自定义装配逻辑；`DefaultOrchestrator` 是库内置的标准实现。

#### Orchestrate trait

```rust
pub trait Orchestrate: Send + Sync {
    /// 根据当前上下文装配本轮配置。返回值是最终裁决，优先级高于 Sense 默认值。
    fn assemble(&self, ctx: &TaskContext) -> Result<AssembledTurn>;
}
```

#### TaskContext（上下文输入）

```rust
pub struct TaskContext {
    // ── 推荐：中性扩展字段 ──
    pub attributes: HashMap<String, String>,  // 任意字符串键值对
    pub flags: HashMap<String, bool>,         // 任意布尔标志
    pub payload: Option<serde_json::Value>,   // 非结构化附加数据

    // ── 遗留字段（DefaultOrchestrator 使用） ──
    pub task_type: String,         // "creative_writing" / "proofreading" / "code_generation"
    pub project_id: Option<String>,
    pub selection: Option<String>, // 当前编辑的文本
    pub entities: Vec<String>,     // 相关实体
    pub read_only: bool,           // DefaultOrchestrator 传播到 AssembledTurn::read_only
}
```

#### AssembledTurn（装配输出）

```rust
pub struct AssembledTurn {
    pub context_messages: Vec<String>,    // 额外注入的 system 片段
    pub tool_schemas: Option<Vec<Value>>, // 三态：None=不干预 / Some([])=禁用全部 / Some(v)=覆盖
    pub enabled_tools: Vec<String>,       // 执行时白名单校验用
    pub read_only: bool,                  // 禁止写入类工具（Session 执行前检查）
    pub model_override: Option<String>,
    pub temperature_override: Option<f64>,
    pub max_tokens_override: Option<i64>,
}
```

**`tool_schemas` 三态语义**（严格区分，不要混用）：

| 值 | 含义 |
|---|---|
| `None` | 不干预，沿用 Session 当前工具配置（snapshot 阶段的 ToolRegistry） |
| `Some(vec![])` | 显式禁用全部工具（LLM 本轮不可调用任何工具） |
| `Some(schemas)` | 显式覆盖为给定工具集，替换 Session 当前配置 |

#### DefaultOrchestrator（内置实现）

不持有 `Sense`——`Sense` 只通过 `session.load_sense()` 进入静态层，两者职责不重叠。

```rust
pub struct DefaultOrchestrator { /* registry + whitelist */ }

impl DefaultOrchestrator {
    // 构建，whitelist = None 时启用全量工具
    pub fn new(registry: Arc<ToolRegistry>) -> Self;

    // 设置工具白名单（builder 风格）
    // 通常从 Sense::tool_whitelist() 取值传入
    pub fn with_whitelist(self, whitelist: Option<Vec<String>>) -> Self;
}
```

**工具决策优先级**：
```
Sense::tool_whitelist()（初始化时显式传入 DefaultOrchestrator）
    → DefaultOrchestrator::assemble()（每轮最终裁决）
        → AssembledTurn.tool_schemas（Session 实际发出的值）
```

#### LLMSession 注入与启动接口

```rust
// 注入编排器（装箱类型）
pub fn set_orchestrator(&mut self, orch: Box<dyn Orchestrate>) -> &mut Self;

// 注入编排器（泛型便捷版，自动装箱）
pub fn with_orchestrator<T: Orchestrate + 'static>(&mut self, orch: T) -> &mut Self;

// 标准启动：ctx channel 由 Session 内部创建，通过 SessionHandle::set_task_context 推送
pub fn run(self, input_rx: mpsc::Receiver<String>) -> (ReceiverStream<SessionEvent>, SessionHandle);

// 底层启动：调用方自持 ctx_tx，两路来源在内部合并（handle.set_task_context 仍可用）
pub fn run_with_context_channel(
    self,
    input_rx: mpsc::Receiver<String>,
    ext_ctx_rx: mpsc::Receiver<TaskContext>,
) -> (ReceiverStream<SessionEvent>, SessionHandle);
```

#### 装配流程示意

```
TaskContext { task_type: "creative_writing", selection: "...", flags: {"read_only": false} }
       ↓
  [Orchestrate::assemble]
       ↓
AssembledTurn {
  context_messages: ["[Task type: creative_writing]", "[Current selection]\n..."],
  tool_schemas:     [search, generate_ideas, ...],     ← Sense 白名单筛选结果
  enabled_tools:    ["search", "generate_ideas"],
  read_only:        false,
  temperature_override: Some(0.85),                    ← task_type 策略
}
```

#### 自定义 Orchestrate 示例

```rust
use flowcloudai_client::{Orchestrate, TaskContext, AssembledTurn};
use anyhow::Result;

struct MyOrchestrator;

impl Orchestrate for MyOrchestrator {
    fn assemble(&self, ctx: &TaskContext) -> Result<AssembledTurn> {
        let scene = ctx.attr("scene").unwrap_or("default");
        Ok(AssembledTurn {
            read_only: ctx.flag("read_only"),
            temperature_override: match scene {
                "editor" => Some(0.3),
                "chat"   => Some(0.7),
                _        => None,
            },
            context_messages: vec![format!("当前场景：{}", scene)],
            ..Default::default()  // tool_schemas = None → 不干预工具选择
        })
    }
}

// 注入到 session
session.with_orchestrator(MyOrchestrator);
```

### 3.7 WASM 插件系统

**接口定义** (`plugin.wit`)：

```wasm
package mapper:plugin;

interface mapper {
    map-request: func(json: string) -> string;
    map-response: func(json: string) -> string;
    map-stream-line: func(line: string) -> string;
}

world api {
    export mapper;
}
```

**插件职责**：将标准格式映射到特定 API

```
标准 ChatRequest（flowcloudai 格式）
         ↓
   [插件::map-request]
         ↓
OpenAI / 千问 / ... 的原生格式
         ↓
调用 API 得到原生响应
         ↓
   [插件::map-response]
         ↓
标准 ChatResponse（flowcloudai 格式）
```

**插件示例**（伪代码）：

```rust
// openai 插件的 map-request
pub fn map_request(json: &str) -> String {
    let request: ChatRequest = serde_json::from_str(json).unwrap();
    
    // 转换为 OpenAI 格式
    let openai_req = serde_json::json!({
        "model": request.model,
        "messages": request.messages,
        "temperature": request.temperature.unwrap_or(0.7),
        "tools": request.tools.map(|tools| {
            tools.into_iter().map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": t
                })
            }).collect::<Vec<_>>()
        }),
    });
    
    serde_json::to_string(&openai_req).unwrap()
}
```

---

## 四、ImageSession（图像生成）

```rust
pub struct ImageSession {
    client: HttpPoster,
    config: SessionConfig,
    pipeline: ApiPipeline,
}

impl ImageSession {
    // 通用生成接口
    pub async fn generate(&self, req: &ImageRequest) -> Result<ImageResult>;

    // 便捷方法：文生图
    pub async fn text_to_image(&self, model: &str, prompt: &str) -> Result<ImageResult>;

    // 便捷方法：图文编辑
    pub async fn edit_image(&self, model: &str, prompt: &str, image_url: &str) -> Result<ImageResult>;

    // 便捷方法：多图融合
    pub async fn merge_images(
        &self,
        model: &str,
        prompt: &str,
        image_urls: Vec<String>,
    ) -> Result<ImageResult>;
}
```

**使用示例**：

```rust
let image_session = client.create_image_session("seedream", "api_key")?;

// 文生图
let result = image_session
    .text_to_image("seedream-v1", "一只红色的狐狸在雪地里")
    .await?;

for img in result.images {
    println!("Image URL: {:?}", img.url);
    println!("Size: {:?}", img.size);
}
```

---

## 五、TTSSession（文本转语音）

```rust
pub struct TTSSession {
    client: HttpPoster,
    config: SessionConfig,
    pipeline: ApiPipeline,
}

impl TTSSession {
    pub async fn synthesize(&self, req: &TTSRequest) -> Result<TTSResult>;

    pub async fn speak(&self, model: &str, text: &str, voice_id: &str) -> Result<TTSResult>;
}

pub struct TTSRequest {
    pub model: String,
    pub text: String,
    pub voice_setting: Option<VoiceSetting>,
    pub audio_setting: Option<AudioSetting>,
    // ... 更多参数
}

pub struct TTSResult {
    pub audio: Vec<u8>,           // PCM 音频原始字节
    pub format: String,           // "mp3", "wav" 等
    pub duration_ms: Option<u64>,
    pub usage_characters: Option<u64>,
}
```

**使用示例**：

```rust
let tts_session = client.create_tts_session("minimax", "api_key")?;

let result = tts_session
    .speak("speech-01", "这是一段文本转语音的演示", "female_1")
    .await?;

// 保存音频文件
std::fs::write("output.mp3", &result.audio)?;
```

---

## 六、AudioDecoder（音频解码播放）

```rust
pub enum AudioSource {
    Hex(String),           // MiniMax 格式
    Base64(String),        // 千问格式
    Url(String),           // HTTP(S) URL
    Raw(Vec<u8>),          // 原始字节
}

pub struct DecodedAudio {
    pub samples: Vec<f32>,     // 交错 PCM 采样
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioDecoder {
    /// 从 AudioSource 获取原始字节
    pub async fn resolve(source: &AudioSource) -> Result<Vec<u8>>;

    /// 解码为 PCM f32（支持 mp3, wav, flac, ogg）
    pub fn decode(data: &[u8], format_hint: Option<&str>) -> Result<DecodedAudio>;

    /// 使用 cpal 播放到默认输出设备
    pub fn play(audio: &DecodedAudio) -> Result<()>;

    /// 一站式：从 AudioSource 解码并播放
    pub async fn play_source(
        source: &AudioSource,
        format_hint: Option<&str>,
    ) -> Result<()>;
}
```

**使用示例**：

```rust
// TTS 生成后的音频
let tts_result = tts_session.speak("model", "Hello", "voice_id").await?;

// 自动检测格式并播放
let source = AudioSource::Hex(tts_result.audio);
AudioDecoder::play_source(&source, Some("mp3")).await?;

// 或保存后再播放
let audio = AudioDecoder::decode_source(&source, Some("mp3")).await?;
AudioDecoder::play(&audio)?;
```

---

## 七、会话管理

### 7.1 SessionHandle（外部句柄）

允许 UI 层无需持有 Session 的所有权，就能异步访问、修改对话，或向后台驱动循环发送控制指令。

```rust
impl SessionHandle {
    // ── 读取 ──────────────────────────────────────────────────
    // 返回完整快照：对话参数 + 系统消息 + 树的线性化历史
    pub async fn get_conversation(&self) -> ChatRequest;

    // ── 修改对话参数（立即生效，下一轮请求使用新值）──────────
    pub async fn set_model(&self, model: &str);
    pub async fn set_temperature(&self, v: f64);
    pub async fn set_stream(&self, v: bool);
    pub async fn set_max_tokens(&self, v: i64);
    pub async fn set_thinking(&self, enabled: bool);
    pub async fn set_frequency_penalty(&self, v: f64);
    pub async fn set_presence_penalty(&self, v: f64);
    pub async fn set_top_p(&self, v: f64);
    pub async fn set_stop(&self, stop: Vec<String>);
    pub async fn set_response_format(&self, format: Value);
    pub async fn set_n(&self, n: i32);
    pub async fn set_tool_choice(&self, choice: &str);
    pub async fn set_logprobs(&self, v: bool);
    pub async fn set_top_logprobs(&self, n: i64);
    // 批量更新（单次加锁，适合同时改多个字段）
    pub async fn update<F: FnOnce(&mut ChatRequest)>(&self, f: F);

    // ── 编排上下文（发往 orchestrate channel）────────────────
    // 更新当前任务上下文（下一轮 assemble 时生效，多次调用取最后一个值）
    pub async fn set_task_context(&self, ctx: TaskContext) -> Result<(), String>;

    // ── 控制指令（通过 ctrl channel 发往 drive loop）─────────
    // 切换插件（下一轮对话生效）
    pub async fn switch_plugin(&self, plugin_id: &str, api_key: &str) -> Result<(), String>;
    // 将消息树 head 移动到指定节点（重说 / 分支 / 历史回退）
    pub async fn checkout(&self, node_id: u64) -> Result<(), String>;
}
```

**`switch_plugin` 说明**：

切换插件会同时更新三项：`base_url`、`api_key`、以及 WASM 映射器（`plugin_id`）。
指令通过内部 ctrl channel 发送，drive loop 在下一次等待用户输入时处理，因此：

- 若当前没有流式响应进行中 → 立即生效
- 若流式响应正在输出 → 等当前轮结束后生效

**`checkout` 说明**：

将消息树 head 移动到任意已有节点，支持三种操作模式：

| 场景 | 操作 | 说明 |
|------|------|------|
| **重说** | checkout 到 user 节点 → drive loop 立即继续 | 跳过等待，用同一条用户消息重新生成回答 |
| **分支** | checkout 到 user 节点 → 发新消息 | 从同一个提问点出发产生新的对话支路 |
| **历史回退** | checkout 到 assistant 节点 → 等待新输入 | 回到某个历史状态，继续对话 |

`node_id` 来自 `TurnBegin` / `TurnEnd` 事件中的 `node_id` 字段。

**使用示例**：

```rust
let (input_tx, input_rx) = mpsc::channel(32);
let (mut event_stream, handle) = session.run(input_rx);

// 动态修改参数（下一轮生效）
handle.set_temperature(0.5).await;
handle.set_max_tokens(1000).await;

// 读取当前对话快照（参数 + 完整历史）
let conv = handle.get_conversation().await;
println!("当前模型: {}", conv.model);

// 切换插件（下一轮生效，不影响历史）
handle.switch_plugin("qwen", "your-qwen-api-key").await?;

// 重说：checkout 到上一条用户消息，drive loop 立即以该 user 节点重新生成
handle.checkout(last_user_node_id).await?;
```

### 7.2 事件流

```rust
pub enum SessionEvent {
    NeedInput,
    TurnBegin {
        turn_id: u64,
        /// 本轮开始时的 head 节点 ID（通常是刚追加的 user 消息节点）
        /// 用于前端记录"从哪个节点触发了这轮对话"，以便后续 checkout
        node_id: u64,
    },
    ReasoningDelta(String),        // 思考过程
    ContentDelta(String),          // AI 生成内容
    ToolCall { index: usize, name: String },
    ToolResult { index: usize, output: String, is_error: bool },
    TurnEnd {
        status: TurnStatus,
        /// 本轮助手消息的节点 ID
        /// 保存此 ID 用于重说：handle.checkout(node_id 的父节点) 即可回到 user 节点
        node_id: u64,
    },
    Error(String),
}

pub enum TurnStatus {
    Ok,
    Cancelled,
    Interrupted,
    Error(String),
}
```

### 7.3 ConversationTree（对话消息树）

对话历史以**树**而非线性列表存储，每条消息是一个节点，记录其父节点 ID。树的"当前路径"（root → head）即发往 API 的消息序列。

```
root
 └── [user] 解释量子纠缠          ← n1
      ├── [assistant] 回答 A      ← n2  （旧分支，已被 checkout 离开）
      └── [assistant] 回答 B      ← n3  ← head（当前路径）
```

**关键操作**：

| 方法 | 说明 |
|------|------|
| `append(msg, turn_id)` | 在 head 之后追加消息，推进 head，返回新节点 ID |
| `checkout(node_id)` | 移动 head（不删除任何节点，原路径完整保留） |
| `linearize()` | 输出 root → head 路径的有序消息列表，供 API 调用 |
| `path_to_head()` | 返回当前路径的节点 ID 列表 |
| `head_role()` | 当前 head 节点的 role（用于判断是否需要等待用户输入） |

**重要规则**：
- `checkout` 永远不删除节点，所有分支始终可恢复
- 系统消息（system prompts）存储在树外的独立列表，跨所有分支保持一致
- `linearize()` 的结果 = `system_messages` + 当前路径消息，组合后发给 API

---

## 八、使用场景与最佳实践

### 场景 1：简单对话

```rust
let mut session = client.create_llm_session("openai", "sk-xxx")?;
let (input_tx, input_rx) = mpsc::channel(32);
let (mut event_stream, handle) = session.run(input_rx);
handle.set_model("gpt-4").await?;

let (input_tx, input_rx) = mpsc::channel(32);
let (mut event_stream, _handle) = session.run(input_rx);

input_tx.send("你好，请介绍一下 Rust".to_string()).await?;

while let Some(event) = event_stream.next().await {
    if let SessionEvent::ContentDelta(text) = event {
        print!("{}", text);
    }
}
```

### 场景 2：带工具调用的对话

```rust
// 1. 先注册工具到全局 ToolRegistry
let mut client = FlowCloudAIClient::new(PathBuf::from("./plugins"), None)?;

client.tool_registry_mut()?.register::<WebSearchState, _>(
    "search_web",
    "搜索网络信息",
    Some(vec![ToolFunctionArg::new("query", "string").required(true)]),
    |state, args| {
        let query = arg_str(args, "query")?;
        let results = perform_search(query)?;
        Ok(serde_json::to_string(&results)?)
    },
)?;

// 2. 创建 session（会自动使用 client.tool_registry）
let mut session = client.create_llm_session("openai", "sk-xxx")?;

// 3. 启动会话
let (input_tx, input_rx) = mpsc::channel(32);
let (mut event_stream, _handle) = session.run(input_rx);

// 发送包含工具需求的问题
input_tx.send("2024年中国经济增长率是多少？".to_string()).await?;

// 4. 处理事件流，包括工具执行结果
while let Some(event) = event_stream.next().await {
    match event {
        SessionEvent::ToolCall { name, index } => {
            println!("[工具调用] {}", name);
        }
        SessionEvent::ToolResult { index, output, is_error } => {
            if is_error {
                eprintln!("[工具结果-错误] {}", output);
            } else {
                println!("[工具结果] {}", output);
            }
        }
        SessionEvent::ContentDelta(text) => print!("{}", text),
        SessionEvent::TurnEnd { .. } => break,
        _ => {}
    }
}
```

### 场景 3：使用 Sense 工作流模式

```rust
// 1. 定义工作流模式
struct CodeReviewSense;
impl Sense for CodeReviewSense {
    fn prompts(&self) -> Vec<String> {
        vec!["你是一位资深的代码审查员，请逐行审查代码".to_string()]
    }
    fn tool_whitelist(&self) -> Option<Vec<String>> {
        Some(vec!["lint_code".to_string(), "check_security".to_string()])
    }
    fn install_tools(&self, registry: &mut ToolRegistry) -> Result<()> {
        // 在这里注册特定工具
        Ok(())
    }
}

// 2. 创建 session 并加载 Sense
let mut session = client.create_llm_session("openai", "sk-xxx")?;
session.load_sense(CodeReviewSense).await?;

// 3. 启动会话，开始代码审查
let (input_tx, input_rx) = mpsc::channel(32);
let (mut event_stream, _) = session.run(input_rx);

// 4. 发送代码进行审查
input_tx.send("请审查以下代码: fn my_func() { ... }".to_string()).await?;

// 处理事件流
while let Some(event) = event_stream.next().await {
    if let SessionEvent::ContentDelta(text) = event {
        print!("{}", text);
    }
}
```

### 场景 4：内容创作工作流

```rust
// 获取用户输入
let user_prompt = "写一篇关于 AI 伦理的文章（500字）";

// 创建 session + Sense
let mut session = client.create_llm_session("openai", "sk-xxx")?;
session.load_sense(CreativeWritingSense).await?;

// 启动会话
let (input_tx, input_rx) = mpsc::channel(32);
let (mut event_stream, _handle) = session.run(input_rx);

// 发送消息并处理事件
input_tx.send(user_prompt.to_string()).await?;
let mut stream = &mut event_stream;

let mut full_response = String::new();
while let Some(event) = stream.next().await {
    match event {
        SessionEvent::ContentDelta(delta) => {
            full_response.push_str(&delta);
            print!("{}", delta);
        }
        SessionEvent::ToolCall { name, index } => {
            // 工具调用由 LLMSession 内部通过 ToolRegistry::conduct 自动执行，
            // 结果以 SessionEvent::ToolResult 事件推送，无需手动调用。
            println!("[工具调用] {} (index: {})", name, index);
        }
        SessionEvent::TurnEnd { .. } => break,
        _ => {}
    }
}

println!("\nFinal text:\n{}", full_response);
```

### 场景 5：会话内切换插件

同一段对话中途更换 AI 供应商，前几轮走 OpenAI，后续换到千问（成本优化或降级场景）。

```rust
let mut session = client.create_llm_session("openai", "sk-openai-xxx")?;

let (input_tx, input_rx) = mpsc::channel(32);
let (mut event_stream, handle) = session.run(input_rx);

handle.set_model("gpt-4o").await?;
handle.set_stream(true).await?;

// 第一轮：走 OpenAI
input_tx.send("解释一下量子纠缠".to_string()).await?;
while let Some(event) = event_stream.next().await {
    match event {
        SessionEvent::ContentDelta(text) => print!("{}", text),
        SessionEvent::TurnEnd { .. } => break,
        _ => {}
    }
}

// 更新编排上下文（下一轮 assemble 生效）
handle.set_task_context(TaskContext {
    task_type: "proofreading".to_string(),
    ..Default::default()
}).await?;

// 切换到千问（下一轮生效，不影响对话历史）
handle.switch_plugin("qwen", "sk-qwen-xxx").await?;
handle.set_model("qwen-max").await?;

// 第二轮：走千问，对话历史保持连续
input_tx.send("用更简单的语言再解释一次".to_string()).await?;
while let Some(event) = event_stream.next().await {
    match event {
        SessionEvent::ContentDelta(text) => print!("{}", text),
        SessionEvent::TurnEnd { .. } => break,
        _ => {}
    }
}
```

> **注意**：`switch_plugin` 不清空对话历史，切换后的轮次会把完整的历史作为上下文发送给新插件。
> 不同供应商对 `message.role` 的支持可能有差异，切换前确认目标供应商的插件支持多轮对话格式。

### 场景 6：历史回退与重说

```rust
let mut session = client.create_llm_session("openai", "sk-xxx")?;

let (input_tx, input_rx) = mpsc::channel(32);
let (mut event_stream, handle) = session.run(input_rx);

handle.set_model("gpt-4o").await?;
handle.set_stream(true).await?;

// 第一轮：发送问题，记录 node_id
let mut user_node: u64 = 0;
let mut asst_node: u64 = 0;

input_tx.send("用一句话解释递归".to_string()).await?;

while let Some(event) = event_stream.next().await {
    match event {
        SessionEvent::TurnBegin { node_id, .. } => user_node = node_id,
        SessionEvent::ContentDelta(text) => print!("{}", text),
        SessionEvent::TurnEnd { node_id, .. } => { asst_node = node_id; break; }
        _ => {}
    }
}
// user_node = 用户消息节点，asst_node = 助手回答节点

// ── 重说：checkout 到 user 节点，drive loop 立即重新生成（无需再发消息）──
handle.checkout(user_node).await?;

while let Some(event) = event_stream.next().await {
    match event {
        SessionEvent::ContentDelta(text) => print!("{}", text),
        SessionEvent::TurnEnd { .. } => { println!("\n[重说完成]"); break; }
        _ => {}
    }
}

// ── 历史回退：checkout 到助手节点，继续对话 ──
handle.checkout(asst_node).await?;
input_tx.send("用代码示例再解释一次".to_string()).await?;

while let Some(event) = event_stream.next().await {
    match event {
        SessionEvent::ContentDelta(text) => print!("{}", text),
        SessionEvent::TurnEnd { .. } => break,
        _ => {}
    }
}
```

> **节点 ID 的来源**：每个 `TurnBegin` 事件携带 `node_id`（本轮触发节点），每个 `TurnEnd` 事件携带 `node_id`（本轮助手消息节点）。
> 前端只需保存这两个 ID 即可支持完整的重说 / 分支 / 回退操作。

---

## 九、插件开发指南

### 9.1 插件结构

一个 OpenAI 插件示例：

```
openai-plugin/
├── Cargo.toml
├── src/
│   └── lib.rs
├── wit/
│   └── plugin.wit
├── manifest.json
└── icon.png
```

### 9.2 实现 map-request / map-response

```rust
// src/lib.rs
use serde_json::{json, Value};

wit_bindgen::generate!({
    path: "../wit",
});

export!(Component);

struct Component;

impl Exports for Component {
    fn map_request(json: String) -> String {
        if let Ok(mut req) = serde_json::from_str::<Value>(&json) {
            // 转换逻辑：按需修改 req 结构
            // 比如重命名字段、调整格式等
            serde_json::to_string(&req).unwrap_or(json)
        } else {
            json
        }
    }

    fn map_response(json: String) -> String {
        // 反向转换：API 响应 → 标准格式
        json
    }

    fn map_stream_line(line: String) -> String {
        // 处理流式响应的每一行（SSE）
        line
    }
}
```

### 9.3 打包为 .fcplug

```bash
# 编译 WASM
cargo build --target wasm32-wasip1 --release

# 创建 ZIP（.fcplug）
zip openai.fcplug \
    -j target/wasm32-wasip1/release/openai_plugin.wasm:plugin.wasm \
    manifest.json \
    icon.png

# 放到 ./plugins 目录
cp openai.fcplug ../plugins/
```

---

## 十、性能优化建议

### 10.1 插件池大小

```rust
// 根据预期并发会话数调整
let registry = PluginRegistry::build(
    engine,
    linker,
    plugins,
    8,  // max_idle_per_pool：建议 = 预期最大并发数
)?;
```

### 10.2 流式响应处理

```rust
// ✅ 推荐：逐个处理事件
while let Some(event) = stream.next().await {
    match event {
        SessionEvent::ContentDelta(delta) => {
            // 实时显示，不等待整个响应
            ui_update(&delta);
        }
        _ => {}
    }
}

// ❌ 避免：缓存整个响应后再处理
let mut all_events = Vec::new();
while let Some(event) = stream.next().await {
    all_events.push(event);
}
// 处理 all_events
```

### 10.3 工具调用的幂等性

```rust
// 确保工具处理函数可被重复调用（AI 可能重试）
pub fn search(query: &str) -> Result<String> {
    // ✅ 无副作用，多次调用结果相同
    let results = perform_search(query)?;
    Ok(serde_json::to_string(&results)?)
}

// ❌ 避免：有全局副作用的工具
static mut CALL_COUNT: i32 = 0;
pub fn dangerous_tool() -> Result<String> {
    unsafe { CALL_COUNT += 1; }  // 不幂等！
    Ok(format!("Called {} times", CALL_COUNT))
}
```

---

## 十一、常见问题

### Q1: 多个 Session 能共享 ToolRegistry 吗？
**A:** 可以。ToolRegistry 被包装在 Arc 中，多个 Session 可以安全地共享。

```rust
let client = FlowCloudAIClient::new(PathBuf::from("/path/to/plugins"), None)?;
let session1 = client.create_llm_session("openai", "key1")?;
let session2 = client.create_llm_session("claude", "key2")?;
// 两个 session 共享同一个 tool_registry
```

### Q2: 如何切换不同的 LLM 供应商？
**A:** 只需加载不同的插件，创建不同的 Session。

```rust
let mut openai = client.create_llm_session("openai", "sk-xxx")?;
let mut claude = client.create_llm_session("anthropic", "sk-yyy")?;
let mut qwen = client.create_llm_session("qwen", "sk-zzz")?;

// 使用任一会话
let (input_tx, input_rx) = mpsc::channel(32);
let (mut event_stream, _) = openai.run(input_rx);
input_tx.send("Hello".to_string()).await?;
```

### Q3: 工具调用失败了怎么办？
**A:** 工具执行错误会自动作为 `ToolResult` 事件返回，包含 `is_error=true`。

```rust
SessionEvent::ToolCall { index, name } => {
    println!("Tool called: {}", name);
}
SessionEvent::ToolResult { index, output, is_error } => {
    if is_error {
        eprintln!("Tool failed: {}", output);
    } else {
        println!("Tool result: {}", output);
    }
}
```

### Q4: 如何限制 AI 只使用特定工具？
**A:** 通过 Sense 的 `tool_whitelist()` 实现。

```rust
impl Sense for MyWorkflow {
    fn tool_whitelist(&self) -> Option<Vec<String>> {
        Some(vec!["search_only".to_string()])
    }
}
```

### Q5: 能否同时调用多个 Session？
**A:** 可以。每个 `run()` 返回的事件流可以独立处理。

```rust
let (input_tx1, input_rx1) = mpsc::channel(32);
let (mut stream1, _) = session1.run(input_rx1);

let (input_tx2, input_rx2) = mpsc::channel(32);
let (mut stream2, _) = session2.run(input_rx2);

// 并发发送消息
input_tx1.send("写文章".to_string()).await?;
input_tx2.send("生成图片".to_string()).await?;

// 并发处理事件流
let h1 = tokio::spawn(async move {
    while let Some(event) = stream1.next().await {
        // 处理 session1 事件
    }
});

let h2 = tokio::spawn(async move {
    while let Some(event) = stream2.next().await {
        // 处理 session2 事件
    }
});

tokio::join!(h1, h2);
```

---

## 十二、调试和诊断

### 启用日志

```rust
// 初始化日志（使用 tracing 或 log）
tracing_subscriber::fmt::init();

// 关键位置会输出 debug 日志
let client = FlowCloudAIClient::new("/path/to/plugins")?;
// [debug] found plugin: openai (LLM)
// [debug] found plugin: seedream (Image)
```

### 检查插件加载状态

```rust
let stats = client.pool_stats();
for (plugin_id, idle_count) in stats {
    println!("Plugin {}: {} idle mappers", plugin_id, idle_count);
}
```

### 查看当前对话

```rust
let handle = session.handle();
let conv = handle.get_conversation().await;
println!("Messages count: {}", conv.messages.len());
println!("Model: {}", conv.model);
println!("Tools: {:?}", conv.tools.map(|t| t.len()));
```

---

## 十三、对话本地持久化

### 13.1 存储数据结构

```rust
/// 对话元信息（ai_list_conversations 返回此结构，不含消息体）
pub struct ConversationMeta {
    pub id: String,          // 毫秒级时间戳，如 "20260416152345678"
    pub title: String,       // 首次保存时从第一条 user 消息自动截取（50字），可 rename
    pub plugin_id: String,   // 会话使用的插件 ID
    pub model: String,       // 最后一次使用的模型名
    pub created_at: String,  // ISO 8601
    pub updated_at: String,  // ISO 8601，每次 TurnEnd Ok 更新
}

/// 存储格式中的单条消息
pub struct StoredMessage {
    pub role: String,              // "user" / "assistant" / "tool"
    pub content: Option<String>,
    pub reasoning: Option<String>, // 思考链（DeepSeek/o1 等模型）
    pub timestamp: String,         // 本条消息的保存时间（ISO 8601）
}

/// 磁盘文件的完整结构（{id}.json）
pub struct StoredConversation {
    // flatten：顶层字段包含 ConversationMeta 的所有字段
    pub meta: ConversationMeta,
    pub messages: Vec<StoredMessage>,
}
```

**JSON 文件示例**（`conversations/20260416152345678.json`）：

```json
{
  "id": "20260416152345678",
  "title": "解释量子纠缠",
  "plugin_id": "openai",
  "model": "gpt-4o",
  "created_at": "2026-04-16T15:23:45.678+00:00",
  "updated_at": "2026-04-16T15:24:12.001+00:00",
  "messages": [
    {
      "role": "user",
      "content": "解释量子纠缠",
      "timestamp": "2026-04-16T15:24:12.001+00:00"
    },
    {
      "role": "assistant",
      "content": "量子纠缠是指…",
      "timestamp": "2026-04-16T15:24:12.001+00:00"
    }
  ]
}
```

### 13.2 自动保存时机

- 每次 `TurnEnd { status: TurnStatus::Ok }` 事件发出后触发
- 工具调用轮次（`finish_reason = "tool_calls"`）不会触发，只有最终轮次（模型真正停止输出）才写盘
- 写盘失败时只打印 stderr 警告，不影响会话继续

### 13.3 标题生成规则

| 场景 | 行为 |
|------|------|
| 首次保存 | 从消息列表中找第一条 `user` 消息，截取前 50 个字符作为标题，超出时追加 `…` |
| 后续保存 | 重读文件，保留已有标题（保证 `ai_rename_conversation` 的修改不被覆盖） |
| 找不到 user 消息 | 使用 `"新对话"` 作为默认标题 |

### 13.4 ConversationStore（直接使用）

如需在 `FlowCloudAIClient` 之外单独操作存储：

```rust
use flowcloudai_client::ConversationStore;

let store = ConversationStore::new(PathBuf::from("./conversations"))?;

// 列出所有对话
let list = store.list();  // Vec<ConversationMeta>，按 updated_at 降序

// 读取完整对话
let conv = store.get("20260416152345678");  // Option<StoredConversation>

// 删除
store.delete("20260416152345678")?;

// 重命名
store.rename("20260416152345678", "新标题".to_string())?;
```

---

## 总结

| 组件                    | 职责           | 核心方法                                                                   |
|-----------------------|--------------|------------------------------------------------------------------------|
| **FlowCloudAIClient** | 初始化、工厂、历史管理  | `new()`, `create_llm_session()`, `ai_list_conversations()`, `ai_get_conversation()` |
| **PluginRegistry**    | WASM 插件管理    | `load()`, `acquire()`                                                  |
| **LLMSession**        | 对话处理         | `load_sense()`, `set_model()`, `run()`                                 |
| **ToolRegistry**      | 工具库          | `register()`, `conduct()`                                              |
| **Sense**             | 模式预设         | `prompts()`, `install_tools()`, `tool_whitelist()`                     |
| **Orchestrate**       | 动态编排接口（trait） | `assemble(ctx) -> AssembledTurn`                                   |
| **DefaultOrchestrator** | 内置编排实现   | `new(registry)`, `with_whitelist()`                                    |
| **ConversationStore** | JSON 文件持久化   | `list()`, `get()`, `delete()`, `rename()`                              |
| **ImageSession**      | 图像生成         | `generate()`, `text_to_image()`                                        |
| **TTSSession**        | 语音合成         | `synthesize()`, `speak()`                                              |
| **AudioDecoder**      | 音频解码播放       | `decode()`, `play()`                                                   |

**设计原则**：
- ✅ **插件隔离**：WASM 沙箱执行，互不影响
- ✅ **会话无状态**：ImageSession、TTSSession 可重复使用
- ✅ **工具解耦**：ToolRegistry 独立于 Session
- ✅ **流式优先**：原生支持 SSE，秒级反馈
- ✅ **编排灵活**：`Orchestrate` trait 完全可自定义，`DefaultOrchestrator` 开箱即用
- ✅ **多模态统一**：LLM、Image、TTS、Audio 统一管理
- ✅ **持久化可选**：JSON 文件存储，按需开启，不改变核心流程

---

**文档版本**：1.1  
**最后更新**：2026-04-16  
**项目**：flowcloudai_client_core  
**维护者**：FlowCloud 团队