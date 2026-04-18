# Orchestrator — 装配层设计与用法

> **受众**：维护本库或在下游项目中集成编排功能的开发者（含 Claude Code）。
> 本文档描述当前实现，可直接对照源码阅读。

---

## 文件位置

```
src/orchestrator/
├── mod.rs          # 重导出：Orchestrate, TaskContext, AssembledTurn, DefaultOrchestrator
├── orchestrate.rs  # Orchestrate trait 定义
├── context.rs      # TaskContext + AssembledTurn 数据结构
└── orchestrator.rs # DefaultOrchestrator 内置实现
```

外部入口（`src/lib.rs`）：
```rust
pub use orchestrator::{Orchestrate, TaskContext, AssembledTurn, DefaultOrchestrator};
```

---

## 核心 trait

```rust
// src/orchestrator/orchestrate.rs
pub trait Orchestrate: Send + Sync {
    fn assemble(&self, ctx: &TaskContext) -> Result<AssembledTurn>;
}
```

**调用时机**：`LLMSession::drive()` 每轮循环开始，拿到用户消息后、发送请求前，调用一次 `assemble`。Session 只消费返回的 `AssembledTurn`，不感知具体编排逻辑。

**没有 orchestrator 时**：`AssembledTurn::default()`，`read_only = false`，不读任何 `TaskContext` 字段。

---

## 数据结构

### TaskContext（输入）

```rust
// src/orchestrator/context.rs
pub struct TaskContext {
    // ── 推荐字段（自定义 Orchestrate 实现优先用这几个）────────
    pub attributes: HashMap<String, String>,  // 任意字符串键值对
    pub flags: HashMap<String, bool>,         // 任意布尔标志
    pub payload: Option<serde_json::Value>,   // 非结构化附加数据

    // ── 遗留字段（DefaultOrchestrator 消费，自定义实现可忽略）──
    pub task_type: String,          // "creative_writing" / "proofreading" / "code_generation"
    pub project_id: Option<String>,
    pub selection: Option<String>,
    pub entities: Vec<String>,
    pub read_only: bool,            // 遗留 read_only 标志，被 flags["read_only"] 覆盖
}
```

**辅助方法**：

```rust
ctx.flag("read_only")          // -> bool，不存在时返回 false
ctx.attr("scene")              // -> Option<&str>
ctx.decode_payload::<MyType>() // -> Result<Option<MyType>>，JSON 反序列化
```

### AssembledTurn（输出）

```rust
pub struct AssembledTurn {
    pub context_messages: Vec<String>,    // 注入到消息流的 system 片段
    pub tool_schemas: Option<Vec<Value>>, // 工具配置（见三态语义）
    pub enabled_tools: Vec<String>,       // 工具执行白名单（与 tool_schemas 对应）
    pub read_only: bool,                  // 禁止写入类工具
    pub model_override: Option<String>,
    pub temperature_override: Option<f64>,
    pub max_tokens_override: Option<i64>,
}
```

#### tool_schemas 三态语义（严格区分）

| 值 | Session 行为 |
|---|---|
| `None` | 不覆盖，沿用 `snapshot()` 阶段 ToolRegistry 的全量 schemas |
| `Some(vec![])` | 覆盖为空列表，LLM 本轮不可调用任何工具 |
| `Some(schemas)` | 覆盖为给定工具集 |

实现位置：`src/llm/session.rs` → `apply_assembled()`：

```rust
// 三态：None=不干预 / Some([])=禁用全部 / Some(v)=覆盖
if turn.tool_schemas.is_some() {
    req.tools = turn.tool_schemas.clone();
}
```

---

## DefaultOrchestrator

**设计约定**：不持有 `Sense`。`Sense` 只通过 `session.load_sense()` 写入静态层（system messages + 工具安装），编排器只在运行时裁决。

```rust
// src/orchestrator/orchestrator.rs
pub struct DefaultOrchestrator {
    registry: Arc<ToolRegistry>,
    whitelist: Option<Vec<String>>,  // None = 全量工具
}

impl DefaultOrchestrator {
    pub fn new(registry: Arc<ToolRegistry>) -> Self;

    /// 设置工具白名单（builder）。
    /// 通常从 Sense::tool_whitelist() 提取后传入：
    ///   DefaultOrchestrator::new(reg).with_whitelist(sense.tool_whitelist())
    pub fn with_whitelist(mut self, whitelist: Option<Vec<String>>) -> Self;
}
```

**assemble 三步骤**：

1. **inject_context** — 将 `task_type` / `selection` / `entities` / `attributes` 转为 system message 片段
2. **select_tools** — 按 whitelist 裁剪 ToolRegistry，填充 `tool_schemas` 和 `enabled_tools`
3. **apply_overrides** — 计算 `read_only` 并按 `task_type` 选参数覆盖

**read_only 优先级**（在 `apply_overrides` 中）：

```
flags["read_only"]（存在时优先）
  → TaskContext::read_only（遗留字段回退）
    → false（AssembledTurn::default()）
```

**task_type → temperature 映射**（内置策略）：

| task_type | temperature_override |
|---|---|
| `"creative_writing"` | `Some(0.85)` |
| `"proofreading"` / `"translation"` | `Some(0.1)` |
| `"code_generation"` | `Some(0.0)` |
| 其他 / 空 | 不覆盖 |

---

## 与 Session 的对接

### 注入接口（`src/llm/session.rs`）

```rust
// 装箱类型（调用方已有 Box<dyn Orchestrate>）
pub fn set_orchestrator(&mut self, orch: Box<dyn Orchestrate>) -> &mut Self;

// 泛型便捷版，自动装箱
pub fn with_orchestrator<T: Orchestrate + 'static>(&mut self, orch: T) -> &mut Self;
```

### 启动接口

```rust
// 标准启动：ctx channel 内部创建，通过 SessionHandle::set_task_context 推送
pub fn run(
    self,
    input_rx: mpsc::Receiver<String>,
) -> (ReceiverStream<SessionEvent>, SessionHandle);

// 底层启动：调用方自持 ctx_tx，Session 内部合并两路来源
// handle.set_task_context 在此模式下依然有效
pub fn run_with_context_channel(
    self,
    input_rx: mpsc::Receiver<String>,
    ext_ctx_rx: mpsc::Receiver<TaskContext>,
) -> (ReceiverStream<SessionEvent>, SessionHandle);
```

### SessionHandle 推送接口（`src/llm/handle.rs`）

```rust
// 推送新上下文（非阻塞排空：每轮 try_recv 取最新值，多次调用只取最后一个）
pub async fn set_task_context(&self, ctx: TaskContext) -> Result<(), String>;
```

### 工厂方法（`src/client.rs`）

```rust
// 同步：传入编排器，返回 LLMSession（编排器已嵌入）
pub fn create_orchestrated_session(
    &self,
    plugin_id: &str,
    api_key: &str,
    orchestrator: Box<dyn Orchestrate>,
) -> Result<LLMSession>;

// 异步：同上 + 内部调用 session.load_sense(sense)
pub async fn create_orchestrated_session_with_sense(
    &self,
    plugin_id: &str,
    api_key: &str,
    sense: impl Sense,
    orchestrator: Box<dyn Orchestrate>,
) -> Result<LLMSession>;
```

---

## 行为速查表

| 情况 | 结果 |
|---|---|
| 未 `set_orchestrator` | `AssembledTurn::default()`，不读 TaskContext 任何字段 |
| `tool_schemas = None` | 沿用 Session 工具配置，不覆盖 |
| `tool_schemas = Some(vec![])` | 禁用全部工具 |
| `tool_schemas = Some(v)` | 覆盖为 v |
| `flags["read_only"]` 存在 | 以它为准 |
| 无 `flags["read_only"]` | 回退到 `TaskContext::read_only` |
| `set_task_context` 多次 | 每轮 `try_recv` 排空，取最后一个值 |
| 从未 `set_task_context` | 用 `TaskContext::default()` |
| `run_with_context_channel` 的外部 tx 和 `handle.set_task_context` 同时发送 | 合并进同一 channel，同样取最新值 |

---

## 完整使用路径

### A. DefaultOrchestrator + create_orchestrated_session_with_sense（推荐）

```rust
let my_sense = ACSSense::new();
client.install_sense(&my_sense)?;

let orch = DefaultOrchestrator::new(client.tool_registry().clone())
    .with_whitelist(my_sense.tool_whitelist());

let mut session = client
    .create_orchestrated_session_with_sense(
        "deepseek-llm", "sk-xxx",
        my_sense,
        Box::new(orch),
    )
    .await?;

session.set_model("deepseek-chat").await.set_stream(true).await;

let (input_tx, input_rx) = mpsc::channel(32);
let (events, handle) = session.run(input_rx);

handle.set_task_context(TaskContext {
    task_type: "code_generation".to_string(),
    ..Default::default()
}).await.ok();
```

### B. 完全自定义 Orchestrate

```rust
struct MyOrch { /* 自己的状态 */ }

impl Orchestrate for MyOrch {
    fn assemble(&self, ctx: &TaskContext) -> Result<AssembledTurn> {
        Ok(AssembledTurn {
            read_only: ctx.flag("read_only"),
            temperature_override: match ctx.attr("mode") {
                Some("precise")  => Some(0.0),
                Some("creative") => Some(0.9),
                _                => None,
            },
            context_messages: vec![format!("mode={}", ctx.attr("mode").unwrap_or("default"))],
            // tool_schemas = None → 不干预，沿用 Session 当前工具配置
            ..Default::default()
        })
    }
}

session.with_orchestrator(MyOrch { /* ... */ });
```

### C. 低层 channel（高级场景）

```rust
// 调用方自己持有 tx，可跨模块、跨线程推送
let (ctx_tx, ctx_rx) = mpsc::channel::<TaskContext>(16);
let (events, handle) = session.run_with_context_channel(input_rx, ctx_rx);

// 两种推送方式均有效，合并到同一 channel
ctx_tx.send(my_ctx).await?;           // 外部 tx
handle.set_task_context(my_ctx).await.ok(); // handle
```

---

## 扩展约定

### 新增 task_type 策略
在 `DefaultOrchestrator::apply_overrides` 的 `match ctx.task_type.as_str()` 里添加分支。
不需要改接口或数据结构。

### 新增 TaskContext 中性字段
优先用 `attributes` / `flags` / `payload`，不要在 `TaskContext` 结构体里继续加业务字段。
遗留字段（`task_type` / `selection` 等）只供 `DefaultOrchestrator` 消费，自定义实现可完全忽略。

### 替换编排器（运行时热切换）
`set_orchestrator` 和 `with_orchestrator` 在 `run()` 之前调用。
`run()` 启动后，编排器被 move 进后台线程，不支持运行时替换。
需要热切换逻辑的，在自己的 `Orchestrate` 实现内部用 `Arc<RwLock<...>>` 持有可变状态。
