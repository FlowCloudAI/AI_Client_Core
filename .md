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
| **会话管理** | 对话历史、状态机、外部句柄 |
| **编排灵活** | Sense 预设 + TaskOrchestrator 动态装配 |

### 五分钟上手

```rust
use flowcloudai_client_core::FlowCloudAIClient;
use flowcloudai_client_core::llm::types::Message;

#[tokio::main]
async fn main() -> Result<()> {
    // 1. 初始化客户端（自动加载 ./plugins 目录下的插件）
    let client = FlowCloudAIClient::new()?;

    // 2. 列出可用的 LLM 插件
    for (id, meta) in client.list_by_kind(PluginKind::LLM) {
        println!("LLM: {} ({})", id, meta.name);
    }

    // 3. 创建 LLM 会话
    let mut session = client.create_llm_session("openai", "sk-...")?;

    // 4. 设置模型和参数
    session
        .set_model("gpt-4")
        .set_temperature(0.7)
        .set_max_tokens(2000);

    // 5. 发送消息并流式接收
    session.push_user("用 Markdown 写一篇关于 AI 的短文").await?;
    
    let mut event_stream = session.drive().await?;
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
  └── Session 工厂
      ├── LLMSession (对话)
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
│  │  TaskOrchestrator（编排引擎）          │  │
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
| **TaskOrchestrator** | 动态装配引擎 | 根据 TaskContext 注入上下文、筛选工具、调整参数 |
| **SessionHandle** | 会话外部句柄 | UI 层无需持有 Session，可异步读写对话 |

---

## 三、核心组件详解

### 3.1 FlowCloudAIClient（主入口）

**职责**：初始化系统、加载插件、创建各类 Session

```rust
pub struct FlowCloudAIClient {
    plugin_registry: Arc<PluginRegistry>,
    tool_registry: Arc<ToolRegistry>,
}

impl FlowCloudAIClient {
    // 初始化，自动扫描 ./plugins 目录
    pub fn new() -> Result<Self>;

    // 列出指定类型的插件（LLM / Image / TTS）
    pub fn list_by_kind(&self, kind: PluginKind) -> Vec<(&String, &PluginMeta)>;

    // 创建 LLM 会话
    pub fn create_llm_session(&self, plugin_id: &str, api_key: &str) -> Result<LLMSession>;

    // 创建图像会话
    pub fn create_image_session(&self, plugin_id: &str, api_key: &str) -> Result<ImageSession>;

    // 创建语音会话
    pub fn create_tts_session(&self, plugin_id: &str, api_key: &str) -> Result<TTSSession>;

    // 安装工具到全局库
    pub fn install_sense(&mut self, sense: &dyn Sense) -> Result<()>;
}
```

**使用示例**：

```rust
let client = FlowCloudAIClient::new()?;

// 列出所有可用的 LLM
for (id, meta) in client.list_by_kind(PluginKind::LLM) {
    println!("Plugin: {}", id);
}

// 创建会话
let session = client.create_llm_session("openai", "sk-xxx")?;
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
    orchestrator: Option<TaskOrchestrator>,
}
```

**生命周期**：

```
1. 创建         → LLMSession::new()
2. 配置         → set_model(), set_temperature(), etc.
3. 加载 Sense   → load_sense(my_sense)
4. 推送消息     → push_user("你好")
5. 驱动会话     → drive() 返回事件流
6. 处理事件     → 处理 ContentDelta、ToolCall 等
7. 响应工具结果 → tool_result() 回源
```

**完整使用示例**：

```rust
let mut session = client.create_llm_session("openai", "sk-xxx")?;

session
    .set_model("gpt-4")
    .set_temperature(0.8)
    .set_max_tokens(2000);

// 加载工作流模式
session.load_sense(CreativeWritingSense).await?;

// 推送用户消息
session.push_user("帮我写一段产品文案").await?;

// 驱动会话（流式响应）
let mut stream = session.drive().await?;
while let Some(event) = stream.next().await {
    match event {
        SessionEvent::ContentDelta(text) => {
            println!("{}", text);
        }
        SessionEvent::ToolCall { name, index } => {
            println!("调用工具: {}", name);
            let result = execute_tool(&name).await?;
            session.tool_result(index, &result, false).await?;
        }
        SessionEvent::TurnEnd { status } => {
            println!("对话结束");
        }
        _ => {}
    }
}
```

**流程图**：

```
用户消息
   ↓
[push_user] 追加到 conversation.messages
   ↓
[drive] 启动驱动循环
   ↓
┌─────────────────────────────┐
│  流程 #1：第一次调用        │
├─────────────────────────────┤
│ 1. conversation → JSON      │
│ 2. 插件映射 (map_request)   │
│ 3. HTTP POST to LLM API     │
│ 4. SSE 流解码               │
│ 5. 发出事件                 │
│    ├─ ContentDelta          │
│    ├─ ReasoningDelta        │
│    ├─ ToolCallStart/Delta   │
│    └─ ToolCallsRequired     │
│ 6. 追加 assistant message   │
│    到 conversation          │
└─────────────────────────────┘
   ↓
[工具调用？]
  ├─ Yes → [tool_result] 追加 tool message
  │         重回 flow #1
  └─ No  → [TurnEnd] 会话结束
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

    // 获取所有工具的 JSON Schema
    pub fn schemas(&self) -> Option<Vec<Value>>;

    // 执行工具调用
    pub async fn conduct(
        &self,
        func_name: &str,
        args: Option<&Value>,
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
[LLMSession] 收到 ToolCall，追加到 conversation
   ↓
[ToolRegistry::conduct] 执行工具处理函数
   ↓
[session.tool_result] 追加 tool 消息
   ↓
[drive 重新循环] 继续与 LLM 交互
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

### 3.6 TaskOrchestrator（任务编排器）

**职责**：根据 TaskContext 动态装配每轮的 Prompt、工具、参数

```rust
pub struct TaskOrchestrator {
    sense: Box<dyn Sense>,
    registry: Arc<ToolRegistry>,
}

impl TaskOrchestrator {
    pub fn new(sense: Box<dyn Sense>, registry: Arc<ToolRegistry>) -> Self;

    /// 根据任务上下文装配本轮配置
    pub fn assemble(&self, ctx: &TaskContext) -> Result<AssembledTurn>;
}

pub struct TaskContext {
    pub task_type: String,        // "creative_writing", "proofreading"
    pub project_id: Option<String>,
    pub selection: Option<String>, // 当前编辑的文本
    pub entities: Vec<String>,    // 相关实体
    pub read_only: bool,
    pub extra: HashMap<String, String>,
}

pub struct AssembledTurn {
    pub context_messages: Vec<String>, // 要注入的额外 system msg
    pub tool_schemas: Option<Vec<Value>>, // 筛选后的工具 schemas
    pub enabled_tools: Vec<String>,    // 启用的工具名
    pub model_override: Option<String>,
    pub temperature_override: Option<f64>,
}
```

**装配流程**：

```
TaskContext
  ├─ task_type = "creative_writing"
  ├─ selection = "用户选中的文本"
  └─ entities = ["角色A", "地点B"]
       ↓
  [Orchestrator::assemble]
       ↓
AssembledTurn
  ├─ context_messages = [
  │    "[Task type: creative_writing]",
  │    "[Current selection]\n用户选中的文本",
  │    "[Related entities: 角色A, 地点B]"
  │  ]
  ├─ tool_schemas = [search, generate_ideas, ...]
  ├─ enabled_tools = ["search", "generate_ideas"]
  └─ temperature_override = Some(0.85)
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

允许 UI 层无需持有 Session 的所有权，就能异步访问和修改对话。

```rust
pub struct SessionHandle {
    inner: Arc<RwLock<ChatRequest>>,
}

impl SessionHandle {
    pub async fn get_conversation(&self) -> ChatRequest;
    pub async fn set_model(&self, model: &str);
    pub async fn set_temperature(&self, v: f64);
    pub async fn set_stream(&self, v: bool);
    pub async fn set_max_tokens(&self, v: i64);
}
```

**使用示例**：

```rust
// Session 启动后，返回 handle 给 UI
let handle = session.handle();

// UI 线程可以随时修改参数
handle.set_temperature(0.5).await;
handle.set_max_tokens(1000).await;

// 也可以读取当前对话
let conv = handle.get_conversation().await;
println!("Current model: {}", conv.model);
```

### 7.2 事件流

```rust
pub enum SessionEvent {
    NeedInput,
    TurnBegin { turn_id: u64 },
    ReasoningDelta(String),        // 思考过程
    ContentDelta(String),          // AI 生成内容
    ToolCall { index: usize, name: String },
    ToolResult { index: usize, output: String, is_error: bool },
    TurnEnd { status: TurnStatus },
    Error(String),
}

pub enum TurnStatus {
    Ok,
    Cancelled,
    Interrupted,
    Error(String),
}
```

---

## 八、使用场景与最佳实践

### 场景 1：简单对话

```rust
let mut session = client.create_llm_session("openai", "sk-xxx")?;
session.set_model("gpt-4");

session.push_user("你好，请介绍一下 Rust").await?;
let mut stream = session.drive().await?;

while let Some(event) = stream.next().await {
    if let SessionEvent::ContentDelta(text) = event {
        print!("{}", text);
    }
}
```

### 场景 2：带工具调用的对话

```rust
// 1. 先注册工具
let mut registry = ToolRegistry::new();
registry.register::<WebSearchState, _>(
    "search_web",
    "搜索网络信息",
    Some(vec![ToolFunctionArg::new("query", "string").required(true)]),
    |state, args| {
        let query = arg_str(args, "query")?;
        let results = perform_search(query)?;
        Ok(serde_json::to_string(&results)?)
    },
);

// 2. 创建 session，注册工具
let session = client.create_llm_session("openai", "sk-xxx")?;
// session 会自动使用 client.tool_registry

// 3. 对话中 AI 可能调用工具
session.push_user("2024年中国经济增长率是多少？").await?;
let mut stream = session.drive().await?;

while let Some(event) = stream.next().await {
    match event {
        SessionEvent::ToolCall { name, index } => {
            // 工具已被调用，等待手动反馈结果
            let output = format!("Search result for: {}", name);
            session.tool_result(index, &output, false).await?;
        }
        SessionEvent::ContentDelta(text) => print!("{}", text),
        SessionEvent::TurnEnd { .. } => break,
        _ => {}
    }
}
```

### 场景 3：动态编排工作流

```rust
// 1. 定义工作流模式
struct CodeReviewSense;
impl Sense for CodeReviewSense {
    fn prompts(&self) -> Vec<String> {
        vec!["你是一位资深的代码审查员".to_string()]
    }
    fn tool_whitelist(&self) -> Option<Vec<String>> {
        Some(vec!["lint_code".to_string(), "check_security".to_string()])
    }
    fn install_tools(&self, registry: &mut ToolRegistry) -> Result<()> {
        // 注册相关工具
        Ok(())
    }
}

// 2. 创建编排器
let orchestrator = TaskOrchestrator::new(
    Box::new(CodeReviewSense),
    client.tool_registry().clone(),
);

// 3. 根据任务上下文装配
let ctx = TaskContext {
    task_type: "code_review".to_string(),
    selection: Some("fn my_func() { ... }".to_string()),
    ..Default::default()
};

let turn = orchestrator.assemble(&ctx)?;
println!("Tools for this turn: {:?}", turn.enabled_tools);

// 4. 应用装配结果到 session
session.push_system_messages(turn.context_messages).await?;
```

### 场景 4：内容创作工作流

```rust
// 获取用户输入
let user_prompt = "写一篇关于 AI 伦理的文章（500字）";

// 创建 session + Sense
let mut session = client.create_llm_session("openai", "sk-xxx")?;
session.load_sense(CreativeWritingSense).await?;

// 推送消息并驱动
session.push_user(user_prompt).await?;
let mut stream = session.drive().await?;

let mut full_response = String::new();
while let Some(event) = stream.next().await {
    match event {
        SessionEvent::ContentDelta(delta) => {
            full_response.push_str(&delta);
            print!("{}", delta);
        }
        SessionEvent::ToolCall { name, index } => {
            // 比如调用 search_references
            let results = search_references(&name).await?;
            session.tool_result(index, &serde_json::to_string(&results)?, false).await?;
        }
        SessionEvent::TurnEnd { .. } => break,
        _ => {}
    }
}

println!("\nFinal text:\n{}", full_response);
```

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
let client = FlowCloudAIClient::new()?;
let session1 = client.create_llm_session("openai", "key1")?;
let session2 = client.create_llm_session("claude", "key2")?;
// 两个 session 共享同一个 tool_registry
```

### Q2: 如何切换不同的 LLM 供应商？
**A:** 只需加载不同的插件，创建不同的 Session。

```rust
let openai = client.create_llm_session("openai", "sk-xxx")?;
let claude = client.create_llm_session("anthropic", "sk-yyy")?;
let qwen = client.create_llm_session("qwen", "sk-zzz")?;

// 使用任一会话
openai.push_user("Hello").await?;
```

### Q3: 工具调用失败了怎么办？
**A:** 通过 `tool_result` 传递 `is_error=true`，AI 会看到错误并可能重试。

```rust
SessionEvent::ToolCall { index, name } => {
    match execute_tool(&name).await {
        Ok(output) => {
            session.tool_result(index, &output, false).await?;
        }
        Err(e) => {
            session.tool_result(index, &format!("Error: {}", e), true).await?;
        }
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
**A:** 可以。使用 `tokio::spawn` 或 `tokio::join!` 并发。

```rust
let h1 = tokio::spawn(async {
    session1.push_user("写文章").await?;
    session1.drive().await
});

let h2 = tokio::spawn(async {
    session2.push_user("生成图片").await?;
    session2.drive().await
});

let (r1, r2) = tokio::join!(h1, h2);
```

---

## 十二、调试和诊断

### 启用日志

```rust
// 初始化日志（使用 tracing 或 log）
tracing_subscriber::fmt::init();

// 关键位置会输出 debug 日志
let client = FlowCloudAIClient::new()?;
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

## 总结

| 组件                    | 职责        | 核心方法                                               |
|-----------------------|-----------|----------------------------------------------------|
| **FlowCloudAIClient** | 初始化、工厂    | `new()`, `create_llm_session()`                    |
| **PluginRegistry**    | WASM 插件管理 | `load()`, `acquire()`                              |
| **LLMSession**        | 对话处理      | `push_user()`, `drive()`, `tool_result()`          |
| **ToolRegistry**      | 工具库       | `register()`, `conduct()`                          |
| **Sense**             | 模式预设      | `prompts()`, `install_tools()`, `tool_whitelist()` |
| **TaskOrchestrator**  | 动态编排      | `assemble()`                                       |
| **ImageSession**      | 图像生成      | `generate()`, `text_to_image()`                    |
| **TTSSession**        | 语音合成      | `synthesize()`, `speak()`                          |
| **AudioDecoder**      | 音频解码播放    | `decode()`, `play()`                               |

**设计原则**：
- ✅ **插件隔离**：WASM 沙箱执行，互不影响
- ✅ **会话无状态**：ImageSession、TTSSession 可重复使用
- ✅ **工具解耦**：ToolRegistry 独立于 Session
- ✅ **流式优先**：原生支持 SSE，秒级反馈
- ✅ **编排灵活**：Sense + TaskOrchestrator 动态装配
- ✅ **多模态统一**：LLM、Image、TTS、Audio 统一管理

---

**文档版本**：1.0  
**最后更新**：2026-03-23  
**项目**：flowcloudai_client_core  
**维护者**：FlowCloud 团队