use crate::error::{ClientError, ErrorCode};
use crate::http_poster::HttpPoster;
use crate::llm::accumulator::ToolCallAccumulator;
use crate::llm::config::SessionConfig;
use crate::llm::handle::SessionHandle;
use crate::llm::stream_decoder::StreamDecoder;
use crate::llm::tree::{ConversationNodeSeed, ConversationTree};
use crate::llm::types::{
    ChatRequest, ChatResponse, CtrlMsg, DecoderEventPayload, Message, SessionEvent, ThinkingType,
    ToolCall, TurnStatus, Usage,
};
use crate::orchestrator::{AssembledTurn, Orchestrate, TaskContext};
use crate::plugin::pipeline::ApiPipeline;
use crate::plugin::types::ThinkingEffort;
use crate::tool::registry::ToolRegistry;
use anyhow::Result;
use futures_util::StreamExt;
use futures_util::future::{self, Either};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;

type TurnOutput = (
    String,
    String,
    Option<Vec<ToolCall>>,
    Option<String>,
    TurnStatus,
    Option<Usage>,
);

/// 单轮取消上下文。
///
/// 每轮开始时记录 watch 当前版本，之后只要版本变化就视为取消当前轮。
#[derive(Clone)]
struct TurnCancel {
    rx: watch::Receiver<u64>,
    baseline: u64,
}

impl TurnCancel {
    fn new(rx: &watch::Receiver<u64>) -> Self {
        Self {
            rx: rx.clone(),
            baseline: *rx.borrow(),
        }
    }

    fn is_cancelled(&self) -> bool {
        *self.rx.borrow() != self.baseline
    }

    async fn cancelled(&mut self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            if self.rx.changed().await.is_err() {
                return;
            }
        }
    }
}

fn cancelled_turn_output(
    content: String,
    reasoning: String,
    tool_calls: Vec<ToolCall>,
    usage: Option<Usage>,
) -> TurnOutput {
    (
        content,
        reasoning,
        if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        Some("cancelled".to_string()),
        TurnStatus::Cancelled,
        usage,
    )
}

// ═════════════════════════════════════════════════════════════
//                    核心会话管理器
// ═════════════════════════════════════════════════════════════

/// LLM 会话管理器
///
/// 负责：
/// - 维护对话历史和配置
/// - 管理请求/响应流程
/// - 处理工具调用
/// - 实现会话状态机
pub struct LLMSession {
    /// HTTP 客户端
    client: HttpPoster,

    /// 对话参数（model、temperature 等；messages 字段不再使用）
    conversation: Arc<RwLock<ChatRequest>>,

    /// 消息历史树（用户/助手/工具消息）
    tree: Arc<RwLock<ConversationTree>>,

    /// 系统级消息（由 Sense 注入，跨分支保持不变）
    system_messages: Arc<Vec<Message>>,

    /// 工具函数管理器
    tool_registry: Arc<ToolRegistry>,

    /// 连接配置
    config: SessionConfig,

    /// 插件注册中心（共享，只读，通过 acquire 借出 mapper）
    pipeline: ApiPipeline,

    /// 当前轮次 ID
    turn_id: u64,

    orchestrator: Option<Box<dyn Orchestrate>>,

}

// ── 构建 & 配置 ──

impl LLMSession {
    pub fn new(
        config: SessionConfig,
        pipeline: ApiPipeline,
        tool_registry: Arc<ToolRegistry>,
    ) -> Result<Self> {
        let client = HttpPoster::new()?;
        Ok(Self {
            client,
            conversation: Arc::new(RwLock::new(ChatRequest::default())),
            tree: Arc::new(RwLock::new(ConversationTree::new())),
            system_messages: Arc::new(Vec::new()),
            tool_registry,
            config,
            pipeline,
            turn_id: 0,
            orchestrator: None,
        })
    }

    /// 将已有历史消息回放到内部 ConversationTree（必须在 run() 之前调用）。
    ///
    /// 调用方可从数据库、文件或内存状态构造 `ConversationNodeSeed`，
    /// 再在创建 session 后、启动前注入，使 tree 与上层历史状态保持一致。
    ///
    /// `head` 为当前活跃节点；无显式 head 时传 `None`，此时退化为以最后一条消息为 head。
    pub fn preload_history(&mut self, messages: Vec<ConversationNodeSeed>, head: Option<u64>) {
        // 在 run() 调用前，只有 self 持有 Arc<RwLock<ConversationTree>>，
        // 因此 Arc::get_mut 保证成功，避免引入 async。
        if let Some(tree_lock) = Arc::get_mut(&mut self.tree) {
            let tree = tree_lock.get_mut();
            let mut prev_id: Option<u64> = None;
            let mut last_id: Option<u64> = None;
            for seed in messages {
                let seed_id = seed.node_id;
                let parent = if seed_id.is_some() {
                    seed.parent
                } else {
                    prev_id
                };
                let id = seed_id.unwrap_or(tree.next_id());
                tree.insert_node(
                    id,
                    parent,
                    seed.message,
                    seed.turn_id.unwrap_or(0),
                    seed.timestamp
                        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
                );
                prev_id = Some(id);
                last_id = Some(id);
            }
            let repaired_cycles = tree.repair_parent_cycles();
            if repaired_cycles > 0 {
                log::warn!(
                    "[client:session][preload_history_repaired] repaired_parent_cycles={}",
                    repaired_cycles
                );
            }
            // 设置 head：优先使用显式 head，其次退化为最后一条消息
            let effective_head = head.or(last_id);
            if let Some(h) = effective_head {
                let _ = tree.set_head(h);
            }
        }
    }

    pub fn set_api(&mut self, api_key: &str) {
        self.config.api_key = api_key.to_string();
    }

    pub fn set_url(&mut self, url: &str) {
        self.config.base_url = url.to_string();
    }

    pub async fn load_sense(&mut self, sense: impl crate::sense::Sense) -> Result<&mut Self> {
        let mut sys_msgs = Vec::new();
        {
            let mut conv = self.conversation.write().await;
            if let Some(mut request) = sense.default_request() {
                // 将 default_request 中预置的 messages 移入 system_messages
                sys_msgs.extend(request.messages.drain(..));
                *conv = request;
                conv.messages.clear();
            }
        }
        for prompt in sense.prompts() {
            sys_msgs.push(Message::system(prompt));
        }
        self.system_messages = Arc::new(sys_msgs);
        self.conversation.write().await.tools = self.tool_registry.schemas();
        Ok(self)
    }

    pub async fn set_model(&mut self, model: &str) -> &mut Self {
        self.conversation.write().await.model = model.to_string();
        self
    }

    pub async fn set_temperature(&mut self, v: f64) -> &mut Self {
        self.conversation.write().await.temperature = Some(v);
        self
    }

    pub async fn set_stream(&mut self, v: bool) -> &mut Self {
        self.conversation.write().await.stream = Some(v);
        self
    }

    pub async fn set_max_tokens(&mut self, v: i64) -> &mut Self {
        self.conversation.write().await.max_tokens = Some(v);
        self
    }

    pub async fn set_thinking(&mut self, enabled: bool) -> &mut Self {
        self.conversation.write().await.thinking = Some(if enabled {
            ThinkingType::enabled()
        } else {
            ThinkingType::disabled()
        });
        self
    }

    pub async fn set_thinking_effort(&mut self, effort: ThinkingEffort) -> &mut Self {
        self.conversation.write().await.thinking_effort = Some(effort);
        self
    }

    pub async fn clear_thinking_effort(&mut self) -> &mut Self {
        self.conversation.write().await.thinking_effort = None;
        self
    }

    pub async fn set_frequency_penalty(&mut self, v: f64) -> &mut Self {
        self.conversation.write().await.frequency_penalty = Some(v);
        self
    }

    pub async fn set_top_p(&mut self, v: f64) -> &mut Self {
        self.conversation.write().await.top_p = Some(v);
        self
    }

    pub async fn set_presence_penalty(&mut self, v: f64) -> &mut Self {
        self.conversation.write().await.presence_penalty = Some(v);
        self
    }

    pub async fn set_stop(&mut self, stop: Vec<String>) -> &mut Self {
        self.conversation.write().await.stop = Some(stop);
        self
    }

    pub async fn set_response_format(&mut self, format: Value) -> &mut Self {
        self.conversation.write().await.response_format = Some(format);
        self
    }

    pub async fn set_n(&mut self, n: i32) -> &mut Self {
        self.conversation.write().await.n = Some(n);
        self
    }

    // ── 编排器 ──

    /// 设置编排器（装箱类型，直接接受 `Box<dyn Orchestrate>`）。
    ///
    /// 适合调用方手里已经持有 trait object 的场景。
    pub fn set_orchestrator(&mut self, orch: Box<dyn Orchestrate>) -> &mut Self {
        self.orchestrator = Some(orch);
        self
    }

    /// 设置编排器（泛型便捷版，自动装箱）。
    ///
    /// 适合直接传入具体类型（如 `DefaultOrchestrator`）的场景。
    pub fn with_orchestrator<T: Orchestrate + 'static>(&mut self, orch: T) -> &mut Self {
        self.orchestrator = Some(Box::new(orch));
        self
    }
}

// ── 启动 ──

impl LLMSession {
    /// 启动会话，失败时通过 `Result` 返回。
    ///
    /// 推荐新代码使用此方法，避免 runtime 创建失败被隐藏。
    pub fn try_run(
        self,
        input_rx: mpsc::Receiver<String>,
    ) -> Result<(ReceiverStream<SessionEvent>, SessionHandle)> {
        let (event_tx, event_rx) = mpsc::channel::<SessionEvent>(self.config.event_buffer);
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<CtrlMsg>(8);
        let (cancel_tx, cancel_rx) = watch::channel::<u64>(0);
        let (ctx_tx, ctx_rx) = mpsc::channel::<TaskContext>(16);

        let handle = SessionHandle {
            inner: Arc::clone(&self.conversation),
            tree: Arc::clone(&self.tree),
            system_messages: Arc::clone(&self.system_messages),
            ctrl_tx,
            cancel_tx,
            ctx_tx,
        };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                ClientError::new(
                    ErrorCode::LlmSessionCreateFailed,
                    "创建 session runtime 失败",
                )
                .with_kv("source", e.to_string())
            })?;

        std::thread::spawn(move || {
            rt.block_on(async move {
                if let Err(e) = self
                    .drive(input_rx, ctrl_rx, Some(ctx_rx), cancel_rx, event_tx.clone())
                    .await
                {
                    let _ = event_tx
                        .send(SessionEvent::Error(ClientError::from_anyhow_owned(e)))
                        .await;
                }
            });
        });

        Ok((ReceiverStream::new(event_rx), handle))
    }

    /// 启动会话。
    ///
    /// 兼容旧 API：如果启动失败，会返回一个只发送 `SessionEvent::Error` 的事件流。
    pub fn run(
        self,
        input_rx: mpsc::Receiver<String>,
    ) -> (ReceiverStream<SessionEvent>, SessionHandle) {
        let (event_tx, event_rx) = mpsc::channel::<SessionEvent>(self.config.event_buffer);
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<CtrlMsg>(8);
        let (cancel_tx, cancel_rx) = watch::channel::<u64>(0);
        let (ctx_tx, ctx_rx) = mpsc::channel::<TaskContext>(16);

        let handle = SessionHandle {
            inner: Arc::clone(&self.conversation),
            tree: Arc::clone(&self.tree),
            system_messages: Arc::clone(&self.system_messages),
            ctrl_tx,
            cancel_tx,
            ctx_tx,
        };

        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                ClientError::new(
                    ErrorCode::LlmSessionCreateFailed,
                    "创建 session runtime 失败",
                )
                .with_kv("source", e.to_string())
            })
        {
            Ok(rt) => {
                std::thread::spawn(move || {
                    rt.block_on(async move {
                        if let Err(e) = self
                            .drive(input_rx, ctrl_rx, Some(ctx_rx), cancel_rx, event_tx.clone())
                            .await
                        {
                            let _ = event_tx
                        .send(SessionEvent::Error(ClientError::from_anyhow_owned(e)))
                        .await;
                        }
                    });
                });
            }
            Err(e) => {
                let _ = event_tx.try_send(SessionEvent::Error(e));
            }
        }

        (ReceiverStream::new(event_rx), handle)
    }

    /// 底层版本：接受调用方自持的 ctx 接收端，适合将上下文流接入已有系统。
    ///
    /// 与 `run()` 的区别：
    /// - 调用方自己持有 `mpsc::Sender<TaskContext>`，可从任意异步上下文推送
    /// - `SessionHandle::set_task_context` 依然可用（内部合并两路来源）
    /// - 两路上下文均在每轮 assemble 前以 `try_recv` 排空，取最新值
    ///
    /// # 示例
    /// ```ignore
    /// let (ctx_tx, ctx_rx) = mpsc::channel::<TaskContext>(16);
    /// let (events, handle) = session.run_with_context_channel(input_rx, ctx_rx);
    /// // 外部推送（等价于 handle.set_task_context，但可跨模块持有 tx）
    /// ctx_tx.send(my_ctx).await?;
    /// ```
    pub fn try_run_with_context_channel(
        self,
        input_rx: mpsc::Receiver<String>,
        mut ext_ctx_rx: mpsc::Receiver<TaskContext>,
    ) -> Result<(ReceiverStream<SessionEvent>, SessionHandle)> {
        let (event_tx, event_rx) = mpsc::channel::<SessionEvent>(self.config.event_buffer);
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<CtrlMsg>(8);
        let (cancel_tx, cancel_rx) = watch::channel::<u64>(0);
        let (ctx_tx, ctx_rx) = mpsc::channel::<TaskContext>(16);
        let ctx_forward_tx = ctx_tx.clone();

        let handle = SessionHandle {
            inner: Arc::clone(&self.conversation),
            tree: Arc::clone(&self.tree),
            system_messages: Arc::clone(&self.system_messages),
            ctrl_tx,
            cancel_tx,
            ctx_tx: ctx_tx.clone(),
        };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                ClientError::new(
                    ErrorCode::LlmSessionCreateFailed,
                    "创建 session runtime 失败",
                )
                .with_kv("source", e.to_string())
            })?;

        std::thread::spawn(move || {
            rt.block_on(async move {
                // 将外部 ctx_rx 转发到内部 channel，与 handle.set_task_context 合并为同一路输入。
                tokio::spawn(async move {
                    while let Some(ctx) = ext_ctx_rx.recv().await {
                        if ctx_forward_tx.send(ctx).await.is_err() {
                            break;
                        }
                    }
                });

                if let Err(e) = self
                    .drive(input_rx, ctrl_rx, Some(ctx_rx), cancel_rx, event_tx.clone())
                    .await
                {
                    let _ = event_tx
                        .send(SessionEvent::Error(ClientError::from_anyhow_owned(e)))
                        .await;
                }
            });
        });

        Ok((ReceiverStream::new(event_rx), handle))
    }

    /// 底层版本：接受调用方自持的 ctx 接收端，适合将上下文流接入已有系统。
    ///
    /// 兼容旧 API：如果启动失败，会返回一个只发送 `SessionEvent::Error` 的事件流。
    pub fn run_with_context_channel(
        self,
        input_rx: mpsc::Receiver<String>,
        mut ext_ctx_rx: mpsc::Receiver<TaskContext>,
    ) -> (ReceiverStream<SessionEvent>, SessionHandle) {
        let (event_tx, event_rx) = mpsc::channel::<SessionEvent>(self.config.event_buffer);
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<CtrlMsg>(8);
        let (cancel_tx, cancel_rx) = watch::channel::<u64>(0);
        let (ctx_tx, ctx_rx) = mpsc::channel::<TaskContext>(16);
        let ctx_forward_tx = ctx_tx.clone();

        let handle = SessionHandle {
            inner: Arc::clone(&self.conversation),
            tree: Arc::clone(&self.tree),
            system_messages: Arc::clone(&self.system_messages),
            ctrl_tx,
            cancel_tx,
            ctx_tx,
        };

        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                ClientError::new(
                    ErrorCode::LlmSessionCreateFailed,
                    "创建 session runtime 失败",
                )
                .with_kv("source", e.to_string())
            })
        {
            Ok(rt) => {
                std::thread::spawn(move || {
                    rt.block_on(async move {
                        tokio::spawn(async move {
                            while let Some(ctx) = ext_ctx_rx.recv().await {
                                if ctx_forward_tx.send(ctx).await.is_err() {
                                    break;
                                }
                            }
                        });

                        if let Err(e) = self
                            .drive(input_rx, ctrl_rx, Some(ctx_rx), cancel_rx, event_tx.clone())
                            .await
                        {
                            let _ = event_tx
                        .send(SessionEvent::Error(ClientError::from_anyhow_owned(e)))
                        .await;
                        }
                    });
                });
            }
            Err(e) => {
                let _ = event_tx.try_send(SessionEvent::Error(e));
            }
        }

        (ReceiverStream::new(event_rx), handle)
    }
}

// ── 核心状态机 ──

impl LLMSession {
    fn enabled_tool_names_from_request(req: &ChatRequest) -> Option<HashSet<String>> {
        req.tools.as_ref().map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
    }

    fn is_write_like_tool(name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        [
            "create",
            "update",
            "delete",
            "remove",
            "edit",
            "rename",
            "write",
            "save",
            "apply",
            "confirm",
            "install",
            "uninstall",
            "enable",
            "disable",
            "set_",
            "set-",
            "merge",
        ]
        .iter()
        .any(|token| lower.contains(token))
    }

    async fn apply_assembled(&self, base: &ChatRequest, turn: &AssembledTurn) -> ChatRequest {
        let mut req = base.clone();
        let insert_at = Self::context_insert_index_before_pending_block(&req.messages);

        // 注入上下文 messages。
        // 规则固定为“插在最后一个待续会话块之前”；
        // 若当前不存在待续会话块，则退化为插在最新用户消息之前。
        for msg in &turn.context_messages {
            req.messages.insert(insert_at, Message::system(msg.clone()));
        }

        // 工具 schemas 三态：
        //   None          → 不干预，保持 snapshot 的工具配置
        //   Some(vec![])  → 显式禁用全部工具
        //   Some(schemas) → 显式覆盖为给定工具集
        if turn.tool_schemas.is_some() {
            req.tools = turn.tool_schemas.clone();
        }

        // 覆盖参数
        if let Some(ref model) = turn.model_override {
            req.model = model.clone();
        }
        if let Some(temp) = turn.temperature_override {
            req.temperature = Some(temp);
        }
        if let Some(max) = turn.max_tokens_override {
            req.max_tokens = Some(max);
        }

        req
    }

    /// 计算 context_messages 的稳定插入点。
    ///
    /// “待续会话块”当前定义为请求尾部的
    /// `assistant(tool_calls) + tool...` 连续片段。
    /// 若检测到该片段，则返回其起始位置；
    /// 否则退化为“最新一条消息之前”，保持普通用户轮行为不变。
    fn context_insert_index_before_pending_block(messages: &[Message]) -> usize {
        if messages.is_empty() {
            return 0;
        }

        let mut tail_start = messages.len();
        while tail_start > 0 && messages[tail_start - 1].role == "tool" {
            tail_start -= 1;
        }

        if tail_start < messages.len()
            && tail_start > 0
            && messages[tail_start - 1].role == "assistant"
            && messages[tail_start - 1]
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty())
        {
            return tail_start - 1;
        }

        messages.len().saturating_sub(1)
    }

    async fn apply_ctrl(
        &mut self,
        msg: CtrlMsg,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<()> {
        match msg {
            CtrlMsg::SwitchPlugin { plugin_id, api_key } => {
                self.pipeline
                    .ensure_plugin_kind(&plugin_id, crate::plugin::types::PluginKind::LLM)?;
                let url = self.pipeline.get_url(&plugin_id)?.to_string();
                self.config.base_url = url;
                self.config.api_key = api_key;
                self.pipeline.try_set_plugin(Some(plugin_id))?;
            }
            CtrlMsg::Checkout { node_id } => {
                self.tree
                    .write()
                    .await
                    .checkout(node_id)
                    .map_err(|e| {
                        ClientError::new(
                            ErrorCode::ValidationFormatError,
                            format!("会话树 checkout 失败: {}", e),
                        )
                        .with_kv("node_id", node_id)
                        .with_kv("source", e.to_string())
                    })?;
                event_tx
                    .send(SessionEvent::BranchChanged { node_id })
                    .await?;
            }
        }
        Ok(())
    }

    async fn drive(
        mut self,
        mut input_rx: mpsc::Receiver<String>,
        mut ctrl_rx: mpsc::Receiver<CtrlMsg>,
        mut ctx_rx: Option<mpsc::Receiver<TaskContext>>,
        cancel_rx: watch::Receiver<u64>,
        event_tx: mpsc::Sender<SessionEvent>,
    ) -> Result<()> {
        let mut current_ctx = TaskContext::default();
        let mut tool_rounds = 0usize;
        let mut accumulated_usage: Option<Usage> = None;
        let mut force_wait_for_user = false;

        loop {
            if force_wait_for_user || self.should_wait_for_user().await {
                let forced_wait = force_wait_for_user;
                force_wait_for_user = false;
                tool_rounds = 0;
                accumulated_usage = None;
                event_tx.send(SessionEvent::NeedInput).await?;

                // 并发等待用户输入或控制指令
                // 收到 Checkout 后重新检查是否仍需等待，收到输入后追加消息并退出等待
                'wait: loop {
                    let input_fut = input_rx.recv();
                    let ctrl_fut = ctrl_rx.recv();
                    futures_util::pin_mut!(input_fut, ctrl_fut);

                    match future::select(input_fut, ctrl_fut).await {
                        Either::Left((Some(input), _)) => {
                            log::info!(
                                "[client:drive][input_received] next_turn_id={} input_chars={}",
                                self.turn_id + 1,
                                input.chars().count()
                            );
                            self.add_message(Message::user(input)).await;
                            log::info!(
                                "[client:drive][user_message_added] next_turn_id={}",
                                self.turn_id + 1
                            );
                            break 'wait;
                        }
                        Either::Left((None, _)) => return Ok(()),
                        Either::Right((Some(ctrl), _)) => {
                            self.apply_ctrl(ctrl, &event_tx).await?;
                            // Checkout 可能使 head 移动到 user 节点，届时无需继续等待
                            let should_continue_turn = if forced_wait {
                                self.head_is_user().await
                            } else {
                                !self.should_wait_for_user().await
                            };
                            if should_continue_turn {
                                break 'wait;
                            }
                        }
                        Either::Right((None, _)) => return Ok(()),
                    }
                }
            }

            // 每轮开始前尝试更新 context（非阻塞）
            if let Some(ref mut rx) = ctx_rx {
                let mut drained_context_count = 0usize;
                while let Ok(ctx) = rx.try_recv() {
                    current_ctx = ctx;
                    drained_context_count += 1;
                }
                if drained_context_count > 0 {
                    log::info!(
                        "[client:drive][context_drained] next_turn_id={} count={} project_id={:?} task_type={} attributes={} flags={}",
                        self.turn_id + 1,
                        drained_context_count,
                        current_ctx.project_id.as_deref(),
                        current_ctx.task_type,
                        current_ctx.attributes.len(),
                        current_ctx.flags.len()
                    );
                }
            }

            // 记录本轮开始时的 head 节点（用于 TurnBegin 事件）
            let turn_head_id = self.tree.read().await.head().unwrap_or(0);

            self.turn_id += 1;
            let mut turn_cancel = TurnCancel::new(&cancel_rx);
            event_tx
                .send(SessionEvent::TurnBegin {
                    turn_id: self.turn_id,
                    node_id: turn_head_id,
                })
                .await?;

            log::info!(
                "[client:drive][snapshot_start] turn_id={} node_id={}",
                self.turn_id,
                turn_head_id
            );
            let stage_started = Instant::now();
            let req = self.snapshot().await;
            let snapshot_elapsed_ms = stage_started.elapsed().as_millis();
            let snapshot_tool_count = req.tools.as_ref().map_or(0, Vec::len);
            let snapshot_content_chars: usize = req
                .messages
                .iter()
                .filter_map(|message| message.content.as_ref())
                .map(|content| content.chars().count())
                .sum();
            let snapshot_last_role = req
                .messages
                .last()
                .map(|message| message.role.as_str())
                .unwrap_or("<none>");
            log::info!(
                "[client:drive][snapshot_done] turn_id={} elapsed_ms={} messages={} last_role={} content_chars={} tool_count={} stream={:?} thinking_set={}",
                self.turn_id,
                snapshot_elapsed_ms,
                req.messages.len(),
                snapshot_last_role,
                snapshot_content_chars,
                snapshot_tool_count,
                req.stream,
                req.thinking.is_some()
            );

            log::info!(
                "[client:drive][assemble_start] turn_id={} has_orchestrator={}",
                self.turn_id,
                self.orchestrator.is_some()
            );
            let stage_started = Instant::now();

            // Orchestrator 装配（如果有）
            // Session 永远只读 AssembledTurn::read_only，不感知 TaskContext 业务字段。
            // 无编排器时使用 AssembledTurn::default()，read_only = false。
            let (req, read_only) = if let Some(ref orch) = self.orchestrator {
                let assembled = orch.assemble(&current_ctx)?;
                let read_only = assembled.read_only;
                let req = self.apply_assembled(&req, &assembled).await;
                (req, read_only)
            } else {
                (req, AssembledTurn::default().read_only)
            };
            let enabled_tools = Self::enabled_tool_names_from_request(&req);
            log::info!(
                "[client:drive][assemble_done] turn_id={} elapsed_ms={} read_only={} messages={} tool_count={}",
                self.turn_id,
                stage_started.elapsed().as_millis(),
                read_only,
                req.messages.len(),
                req.tools.as_ref().map_or(0, Vec::len)
            );

            log::info!(
                "[client:drive][send_and_process_start] turn_id={} stream={:?} base_url={}",
                self.turn_id,
                req.stream,
                self.config.base_url
            );
            let stage_started = Instant::now();
            let (content, reasoning, tool_calls, finish_reason, turn_status, usage) = self
                .send_and_process(&req, &mut turn_cancel, &event_tx)
                .await?;
            log::info!(
                "[client:drive][send_and_process_done] turn_id={} elapsed_ms={} content_chars={} reasoning_chars={} tool_calls={} finish_reason={:?} status={}",
                self.turn_id,
                stage_started.elapsed().as_millis(),
                content.chars().count(),
                reasoning.chars().count(),
                tool_calls.as_ref().map_or(0, Vec::len),
                finish_reason,
                match &turn_status {
                    TurnStatus::Ok => "ok",
                    TurnStatus::Cancelled => "cancelled",
                    TurnStatus::Interrupted => "interrupted",
                    TurnStatus::Error(_) => "error",
                }
            );

            // 累加本轮的 usage（同一用户 turn 内可能有多次 API 调用，如工具执行后的重试）
            if let Some(ref u) = usage {
                match accumulated_usage {
                    Some(ref mut acc) => {
                        acc.prompt_tokens += u.prompt_tokens;
                        acc.completion_tokens += u.completion_tokens;
                        acc.total_tokens += u.total_tokens;
                    }
                    None => accumulated_usage = Some(u.clone()),
                }
            }

            let asst_node_id =
                if matches!(turn_status, TurnStatus::Cancelled | TurnStatus::Interrupted) {
                    // 将已生成的部分内容保存为 assistant 节点，使 head 推进到 "assistant"，
                    // 确保 should_wait_for_user() 返回 true，避免 drive 循环立即重试。
                    self.add_message(Message::assistant(
                        Some(content).filter(|s: &String| !s.is_empty()),
                        Some(reasoning).filter(|s: &String| !s.is_empty()),
                        None,
                    ))
                    .await
                } else {
                    self.add_message(Message::assistant(
                        Some(content).filter(|s: &String| !s.is_empty()),
                        Some(reasoning).filter(|s: &String| !s.is_empty()),
                        tool_calls.clone(),
                    ))
                    .await
                };

            if finish_reason.as_deref() == Some("tool_calls") {
                if let Some(calls) = tool_calls {
                    tool_rounds += 1;
                    if tool_rounds > self.config.max_tool_rounds {
                        let status = TurnStatus::Error(
                            ClientError::new(
                                ErrorCode::LlmToolCallFailed,
                                format!(
                                    "工具调用超过最大连续轮数限制: {}",
                                    self.config.max_tool_rounds
                                ),
                            )
                            .with_kv("max_tool_rounds", self.config.max_tool_rounds as u64)
                            .with_kv("tool_rounds", tool_rounds as u64),
                        );
                        event_tx
                            .send(SessionEvent::TurnEnd {
                                status,
                                node_id: asst_node_id,
                                usage: accumulated_usage.take(),
                            })
                            .await?;
                        continue;
                    }
                    if self
                        .execute_tool_calls(
                            calls,
                            &enabled_tools,
                            read_only,
                            &mut turn_cancel,
                            &event_tx,
                        )
                        .await?
                    {
                        force_wait_for_user = true;
                        event_tx
                            .send(SessionEvent::TurnEnd {
                                status: TurnStatus::Cancelled,
                                node_id: asst_node_id,
                                usage: accumulated_usage.take(),
                            })
                            .await?;
                        continue;
                    }
                    continue;
                }
            }

            event_tx
                .send(SessionEvent::TurnEnd {
                    status: turn_status,
                    node_id: asst_node_id,
                    usage: accumulated_usage.take(),
                })
                .await?;
        }
    }

    async fn should_wait_for_user(&self) -> bool {
        self.tree
            .read()
            .await
            .head_role()
            .map_or(true, |r| r == "assistant")
    }

    async fn head_is_user(&self) -> bool {
        self.tree
            .read()
            .await
            .head_role()
            .is_some_and(|r| r == "user")
    }

    async fn snapshot(&self) -> ChatRequest {
        let mut req = self.conversation.read().await.clone();
        req.messages = self
            .system_messages
            .iter()
            .cloned()
            .chain(self.tree.read().await.linearize())
            .collect();
        req.tools = self.tool_registry.schemas();
        req
    }

    async fn add_message(&self, msg: Message) -> u64 {
        self.tree.write().await.append(msg, self.turn_id)
    }
}

// ── 插件映射（核心变化点） ──

impl LLMSession {
    /// 请求转换：acquire mapper → map → release（自动）。
    fn prepare_request(&self, req: &ChatRequest) -> Result<Value> {
        self.pipeline
            .validate_llm_request(&req.model, req.thinking_effort)?;
        let json = serde_json::to_value(req)?;
        self.pipeline.prepare_request_json(&json)
    }

    /// 响应转换。
    fn normalize_response(&self, raw: &str) -> Result<String> {
        self.pipeline.map_response(raw)
    }

    /// 流式行转换。
    fn normalize_stream_line(&self, line: &str) -> Result<String> {
        self.pipeline.map_stream_line(line)
    }
}

// ── 请求 & 响应处理 ──

impl LLMSession {
    async fn send_and_process(
        &mut self,
        req: &ChatRequest,
        cancel: &mut TurnCancel,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<TurnOutput> {
        if cancel.is_cancelled() {
            return Ok(cancelled_turn_output(
                String::new(),
                String::new(),
                Vec::new(),
                None,
            ));
        }

        if req.stream.unwrap_or(false) {
            self.handle_stream(req, cancel, event_tx).await
        } else {
            self.handle_non_stream(req, cancel, event_tx).await
        }
    }

    async fn handle_non_stream(
        &mut self,
        req: &ChatRequest,
        cancel: &mut TurnCancel,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<TurnOutput> {
        log::info!(
            "[client:llm][non_stream_prepare_start] turn_id={} messages={} tool_count={}",
            self.turn_id,
            req.messages.len(),
            req.tools.as_ref().map_or(0, Vec::len)
        );
        let stage_started = Instant::now();
        if cancel.is_cancelled() {
            return Ok(cancelled_turn_output(
                String::new(),
                String::new(),
                Vec::new(),
                None,
            ));
        }
        let json = self.prepare_request(req)?;
        if cancel.is_cancelled() {
            return Ok(cancelled_turn_output(
                String::new(),
                String::new(),
                Vec::new(),
                None,
            ));
        }
        log::info!(
            "[client:llm][non_stream_prepare_done] turn_id={} elapsed_ms={} body_bytes={}",
            self.turn_id,
            stage_started.elapsed().as_millis(),
            json.to_string().len()
        );

        let raw_line = {
            log::info!(
                "[client:llm][non_stream_http_send_start] turn_id={} base_url={}",
                self.turn_id,
                self.config.base_url
            );
            let stage_started = Instant::now();
            let post_fut = self
                .client
                .post_json(&self.config.base_url, &self.config.api_key, json);
            let stream = tokio::select! {
                result = post_fut => result?,
                _ = cancel.cancelled() => {
                    return Ok(cancelled_turn_output(
                        String::new(),
                        String::new(),
                        Vec::new(),
                        None,
                    ));
                }
            };
            log::info!(
                "[client:llm][non_stream_http_send_done] turn_id={} elapsed_ms={}",
                self.turn_id,
                stage_started.elapsed().as_millis()
            );
            tokio::pin!(stream);
            log::info!(
                "[client:llm][non_stream_first_line_wait] turn_id={}",
                self.turn_id
            );
            tokio::select! {
                raw_line = stream.next() => {
                    raw_line.ok_or_else(|| {
                        ClientError::new(ErrorCode::LlmResponseEmpty, "LLM 响应为空")
                    })??
                }
                _ = cancel.cancelled() => {
                    return Ok(cancelled_turn_output(
                        String::new(),
                        String::new(),
                        Vec::new(),
                        None,
                    ));
                }
            }
        };
        log::info!(
            "[client:llm][non_stream_first_line_done] turn_id={} bytes={}",
            self.turn_id,
            raw_line.len()
        );

        if cancel.is_cancelled() {
            return Ok(cancelled_turn_output(
                String::new(),
                String::new(),
                Vec::new(),
                None,
            ));
        }
        let normalized = self.normalize_response(&raw_line)?;

        let res: ChatResponse = serde_json::from_str(&normalized).map_err(|e| {
            ClientError::new(ErrorCode::LlmResponseParseError, "LLM 响应 JSON 解析失败")
                .with_kv("source", e.to_string())
        })?;
        let choice = res.choices.first().ok_or_else(|| {
            ClientError::new(ErrorCode::LlmResponseParseError, "LLM 响应 choices 为空")
        })?;

        let reasoning = choice.message.reasoning_content.clone().unwrap_or_default();
        let content = choice.message.content.clone().unwrap_or_default();
        let finish_reason = choice.finish_reason.clone();

        if !reasoning.is_empty() {
            event_tx
                .send(SessionEvent::ReasoningDelta(reasoning.clone()))
                .await?;
        }
        if !content.is_empty() {
            event_tx
                .send(SessionEvent::ContentDelta(content.clone()))
                .await?;
        }

        let tool_calls_vec = choice.message.tool_calls.clone().unwrap_or_default();
        let tool_calls = if tool_calls_vec.is_empty() {
            None
        } else {
            for call in &tool_calls_vec {
                event_tx
                    .send(SessionEvent::ToolCall {
                        index: call.index,
                        name: call.function.name.clone(),
                        arguments: call.function.arguments.clone(),
                    })
                    .await?;
            }
            Some(tool_calls_vec)
        };

        Ok((
            content,
            reasoning,
            tool_calls,
            Some(finish_reason),
            TurnStatus::Ok,
            Some(res.usage),
        ))
    }

    async fn handle_stream(
        &mut self,
        req: &ChatRequest,
        cancel: &mut TurnCancel,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<TurnOutput> {
        // StreamDecoder 和 ToolCallAccumulator 降为方法局部变量
        let mut decoder = StreamDecoder::default();
        decoder.begin_turn(self.turn_id);
        let mut acc = ToolCallAccumulator::default();

        log::info!(
            "[client:llm][stream_prepare_start] turn_id={} messages={} tool_count={}",
            self.turn_id,
            req.messages.len(),
            req.tools.as_ref().map_or(0, Vec::len)
        );
        let stage_started = Instant::now();
        if cancel.is_cancelled() {
            return Ok(cancelled_turn_output(
                String::new(),
                String::new(),
                Vec::new(),
                None,
            ));
        }
        let json = self.prepare_request(req)?;
        if cancel.is_cancelled() {
            return Ok(cancelled_turn_output(
                String::new(),
                String::new(),
                Vec::new(),
                None,
            ));
        }
        log::info!(
            "[client:llm][stream_prepare_done] turn_id={} elapsed_ms={} body_bytes={}",
            self.turn_id,
            stage_started.elapsed().as_millis(),
            json.to_string().len()
        );

        log::info!(
            "[client:llm][stream_http_send_start] turn_id={} base_url={}",
            self.turn_id,
            self.config.base_url
        );
        let stage_started = Instant::now();
        let post_fut = self
            .client
            .post_json(&self.config.base_url, &self.config.api_key, json);
        let stream = tokio::select! {
            result = post_fut => result?,
            _ = cancel.cancelled() => {
                return Ok(cancelled_turn_output(
                    String::new(),
                    String::new(),
                    Vec::new(),
                    None,
                ));
            }
        };
        log::info!(
            "[client:llm][stream_http_send_done] turn_id={} elapsed_ms={}",
            self.turn_id,
            stage_started.elapsed().as_millis()
        );
        tokio::pin!(stream);

        let mut full_content = String::new();
        let mut full_reasoning = String::new();
        let mut finish_reason: Option<String> = None;
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut turn_status = TurnStatus::Ok;
        let mut usage: Option<Usage> = None;
        let mut line_count = 0usize;

        'outer: loop {
            let raw_line = tokio::select! {
                _ = cancel.cancelled() => {
                    turn_status = TurnStatus::Cancelled;
                    finish_reason = Some("cancelled".to_string());
                    usage = decoder.take_pending_usage();
                    break 'outer;
                }
                raw_line = stream.next() => {
                    match raw_line {
                        Some(raw_line) => raw_line,
                        None => break 'outer,
                    }
                }
            };
            let line = raw_line?;
            if cancel.is_cancelled() {
                turn_status = TurnStatus::Cancelled;
                finish_reason = Some("cancelled".to_string());
                usage = decoder.take_pending_usage();
                break 'outer;
            }
            line_count += 1;
            if line_count == 1 || line_count % 50 == 0 {
                log::info!(
                    "[client:llm][stream_line_received] turn_id={} line_count={} bytes={}",
                    self.turn_id,
                    line_count,
                    line.len()
                );
            }
            if line.is_empty() {
                continue;
            }

            // acquire → map → release，每行独立借出，不跨 await
            let normalized = self.normalize_stream_line(&line)?;

            let events = decoder.decode(&normalized);

            for ev in events {
                let ev = ev?;

                match ev.payload {
                    DecoderEventPayload::AssistantReasoningDelta { delta } => {
                        full_reasoning.push_str(&delta);
                        event_tx.send(SessionEvent::ReasoningDelta(delta)).await?;
                    }

                    DecoderEventPayload::AssistantContentDelta { delta } => {
                        full_content.push_str(&delta);
                        event_tx.send(SessionEvent::ContentDelta(delta)).await?;
                    }

                    DecoderEventPayload::ToolCallStart { index, tool_name } => {
                        acc.on_start(index, Some(&tool_name));
                        event_tx
                            .send(SessionEvent::ToolCall {
                                index,
                                name: tool_name,
                                arguments: String::new(),
                            })
                            .await?;
                    }

                    DecoderEventPayload::ToolCallDelta {
                        index,
                        tool_name,
                        args,
                    } => {
                        acc.on_delta(index, tool_name.as_deref(), &args);
                    }

                    DecoderEventPayload::ToolCallsRequired => {
                        // 取出可能已被暂存的 usage（部分 API 在 tool_calls 之前发送 usage chunk）
                        usage = decoder.take_pending_usage();
                        tool_calls = acc.build_calls(self.turn_id);
                        for call in &tool_calls {
                            event_tx
                                .send(SessionEvent::ToolCall {
                                    index: call.index,
                                    name: call.function.name.clone(),
                                    arguments: call.function.arguments.clone(),
                                })
                                .await?;
                        }
                        finish_reason = Some("tool_calls".to_string());
                        break 'outer;
                    }

                    DecoderEventPayload::TurnEnd { status, usage: u } => {
                        turn_status = status.clone();
                        if u.is_some() {
                            usage = u;
                        }
                        finish_reason = Some(match &turn_status {
                            TurnStatus::Ok => "stop".to_string(),
                            TurnStatus::Cancelled => "cancelled".to_string(),
                            TurnStatus::Interrupted => "interrupted".to_string(),
                            TurnStatus::Error(e) => return Err(e.clone().into()),
                        });

                        // Qwen 的 OpenAI 兼容流式响应会先发送 finish_reason=stop，
                        // 再发送 choices=[] 的 usage-only chunk，最后发送 [DONE]。
                        // 普通完成且尚未拿到 usage 时继续读取尾部块，避免用量统计丢失。
                        if Self::should_stop_after_stream_turn_end(&turn_status, &usage) {
                            break 'outer;
                        }
                    }

                    _ => {}
                }
            }
        }

        if finish_reason.is_none() {
            // 部分 API（如 DeepSeek v4 代理）不在流式 chunk 中携带
            // finish_reason，而是仅以 [DONE] 或 TCP 关闭表示结束。
            // 此时视为正常结束。
            finish_reason = Some("stop".to_string());
            if !matches!(turn_status, TurnStatus::Error(_)) {
                turn_status = TurnStatus::Ok;
            }
            // 尝试取出可能已被暂存在 decoder 中的 usage
            if usage.is_none() {
                usage = decoder.take_pending_usage();
            }
        }

        Ok((
            full_content,
            full_reasoning,
            if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            finish_reason,
            turn_status,
            usage,
        ))
    }

    fn should_stop_after_stream_turn_end(turn_status: &TurnStatus, usage: &Option<Usage>) -> bool {
        usage.is_some() || !matches!(turn_status, TurnStatus::Ok)
    }
}

// ── 工具执行 ──

impl LLMSession {
    async fn execute_tool_calls(
        &mut self,
        tool_calls: Vec<ToolCall>,
        enabled_tools: &Option<HashSet<String>>,
        read_only: bool,
        cancel: &mut TurnCancel,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<bool> {
        for call in tool_calls {
            if cancel.is_cancelled() {
                return Ok(true);
            }

            let func_name = &call.function.name;
            let args_str = call.function.arguments.trim();

            let (output, is_error) = if enabled_tools
                .as_ref()
                .is_some_and(|tools| !tools.contains(func_name))
            {
                (
                    format!("工具执行失败: 本轮不允许调用工具 '{}'", func_name),
                    true,
                )
            } else if read_only && Self::is_write_like_tool(func_name) {
                (
                    format!("工具执行失败: 只读模式下禁止调用写入类工具 '{}'", func_name),
                    true,
                )
            } else {
                let args_v: Value = if args_str.is_empty() {
                    Value::Object(Default::default())
                } else {
                    match serde_json::from_str(args_str) {
                        Ok(v) => v,
                        Err(e) => {
                            let output = format!("工具执行失败: 工具参数不是合法 JSON: {}", e);
                            event_tx
                                .send(SessionEvent::ToolResult {
                                    index: call.index,
                                    output: output.clone(),
                                    is_error: true,
                                })
                                .await?;
                            let tool_call_id = Self::synth_tool_call_id(self.turn_id, call.index);
                            let _ = self.add_message(Message::tool(output, tool_call_id)).await;
                            if cancel.is_cancelled() {
                                return Ok(true);
                            }
                            continue;
                        }
                    }
                };

                let conduct_fut =
                    self.tool_registry
                        .conduct(func_name, Some(&args_v), Duration::from_secs(600));
                match tokio::select! {
                    result = conduct_fut => result,
                    _ = cancel.cancelled() => return Ok(true),
                } {
                    Ok(o) => (o, false),
                    Err(e) => (format!("工具执行失败: {}", e), true),
                }
            };

            let tool_call_id = Self::synth_tool_call_id(self.turn_id, call.index);

            event_tx
                .send(SessionEvent::ToolResult {
                    index: call.index,
                    output: output.clone(),
                    is_error,
                })
                .await?;

            let _ = self.add_message(Message::tool(output, tool_call_id)).await;
            if cancel.is_cancelled() {
                return Ok(true);
            }
        }

        Ok(false)
    }

    #[inline]
    fn synth_tool_call_id(turn_id: u64, index: usize) -> String {
        format!("t{}:idx:{}", turn_id, index)
    }
}

#[cfg(test)]
mod tests {
    use super::LLMSession;
    use crate::llm::config::SessionConfig;
    use crate::llm::tree::ConversationNodeSeed;
    use crate::llm::types::{
        Message, SessionEvent, ToolCall, ToolFunctionArg, ToolFunctionCall, TurnStatus, Usage,
    };
    use crate::plugin::pipeline::ApiPipeline;
    use crate::plugin::registry::PluginRegistry;
    use crate::tool::registry::ToolRegistry;
    use futures_util::StreamExt;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;

    fn new_test_session() -> LLMSession {
        let registry = Arc::new(PluginRegistry::empty().unwrap());
        let pipeline = ApiPipeline::try_new(registry, None).unwrap();
        LLMSession::new(
            SessionConfig::default(),
            pipeline,
            Arc::new(ToolRegistry::new()),
        )
        .unwrap()
    }

    fn stored_message(
        node_id: Option<u64>,
        parent: Option<u64>,
        role: &str,
        content: &str,
    ) -> ConversationNodeSeed {
        ConversationNodeSeed {
            node_id,
            parent,
            turn_id: Some(0),
            timestamp: Some("2026-05-14T00:00:00Z".to_string()),
            message: Message {
                role: role.to_string(),
                content: Some(content.to_string()),
                reasoning_content: None,
                tool_call_id: None,
                tool_calls: None,
            },
        }
    }

    async fn new_http_test_session(
        base_url: String,
        stream: bool,
        tool_registry: Arc<ToolRegistry>,
    ) -> LLMSession {
        let registry = Arc::new(PluginRegistry::empty().unwrap());
        let pipeline = ApiPipeline::try_new(registry, None).unwrap();
        let mut config = SessionConfig::default();
        config.base_url = base_url;
        config.api_key = "test-key".to_string();
        config.event_buffer = 64;
        config.max_tool_rounds = 4;

        let mut session = LLMSession::new(config, pipeline, tool_registry).unwrap();
        session.set_model("mock-model").await;
        session.set_stream(stream).await;
        session
    }

    enum MockReply {
        DelayHeaders(Duration),
        StreamPartialThenHold { content: String, hold: Duration },
        StreamDone { content: String },
        StreamToolCall { name: String },
    }

    async fn spawn_mock_server(replies: Vec<MockReply>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let replies = Arc::new(std::sync::Mutex::new(VecDeque::from(replies)));
        let request_count = Arc::new(AtomicUsize::new(0));
        let replies_for_task = Arc::clone(&replies);
        let count_for_task = Arc::clone(&request_count);

        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let reply = replies_for_task.lock().unwrap().pop_front();
                let Some(reply) = reply else {
                    break;
                };
                count_for_task.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    handle_mock_connection(socket, reply).await;
                });
            }
        });

        (format!("http://{}", addr), request_count)
    }

    async fn handle_mock_connection(mut socket: TcpStream, reply: MockReply) {
        let _ = read_request_headers(&mut socket).await;
        match reply {
            MockReply::DelayHeaders(delay) => {
                tokio::time::sleep(delay).await;
            }
            MockReply::StreamPartialThenHold { content, hold } => {
                let _ = write_stream_headers(&mut socket).await;
                let _ = write_sse_line(&mut socket, stream_content_chunk(&content, None)).await;
                let _ = socket.flush().await;
                tokio::time::sleep(hold).await;
            }
            MockReply::StreamDone { content } => {
                let _ = write_stream_headers(&mut socket).await;
                let _ = write_sse_line(&mut socket, stream_content_chunk(&content, None)).await;
                let _ = write_sse_line(&mut socket, "[DONE]".to_string()).await;
                let _ = socket.flush().await;
            }
            MockReply::StreamToolCall { name } => {
                let _ = write_stream_headers(&mut socket).await;
                let _ = write_sse_line(&mut socket, stream_tool_call_chunk(&name)).await;
                let _ = write_sse_line(&mut socket, stream_tool_finish_chunk()).await;
                let _ = socket.flush().await;
            }
        }
    }

    async fn read_request_headers(socket: &mut TcpStream) -> std::io::Result<()> {
        let mut buf = [0u8; 1024];
        let mut data = Vec::new();
        loop {
            let n = socket.read(&mut buf).await?;
            if n == 0 {
                return Ok(());
            }
            data.extend_from_slice(&buf[..n]);
            if data.windows(4).any(|w| w == b"\r\n\r\n") {
                return Ok(());
            }
        }
    }

    async fn write_stream_headers(socket: &mut TcpStream) -> std::io::Result<()> {
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
    }

    async fn write_sse_line(socket: &mut TcpStream, data: String) -> std::io::Result<()> {
        socket
            .write_all(format!("data: {}\n\n", data).as_bytes())
            .await
    }

    fn stream_content_chunk(content: &str, finish_reason: Option<&str>) -> String {
        serde_json::json!({
            "id": "chunk-1",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "mock-model",
            "choices": [{
                "index": 0,
                "delta": { "content": content },
                "finish_reason": finish_reason
            }],
            "usage": null
        })
        .to_string()
    }

    fn stream_tool_call_chunk(name: &str) -> String {
        serde_json::json!({
            "id": "chunk-tool",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "mock-model",
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": "{}"
                        }
                    }]
                },
                "finish_reason": null
            }],
            "usage": null
        })
        .to_string()
    }

    fn stream_tool_finish_chunk() -> String {
        serde_json::json!({
            "id": "chunk-tool-finish",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "mock-model",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "tool_calls"
            }],
            "usage": null
        })
        .to_string()
    }

    async fn wait_for_request_count(count: &Arc<AtomicUsize>, expected: usize) {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            if count.load(Ordering::SeqCst) >= expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "等待请求数量超时: expected={}, actual={}",
            expected,
            count.load(Ordering::SeqCst)
        );
    }

    async fn wait_for_turn_begin(events: &mut ReceiverStream<SessionEvent>) {
        loop {
            match events.next().await {
                Some(SessionEvent::TurnBegin { .. }) => return,
                Some(SessionEvent::Error(e)) => panic!("收到错误事件: {}", e),
                Some(_) => {}
                None => panic!("事件流提前结束"),
            }
        }
    }

    async fn wait_for_content_delta(events: &mut ReceiverStream<SessionEvent>) -> String {
        loop {
            match events.next().await {
                Some(SessionEvent::ContentDelta(delta)) => return delta,
                Some(SessionEvent::Error(e)) => panic!("收到错误事件: {}", e),
                Some(_) => {}
                None => panic!("事件流提前结束"),
            }
        }
    }

    async fn wait_for_turn_end(events: &mut ReceiverStream<SessionEvent>) -> (TurnStatus, u64) {
        loop {
            match events.next().await {
                Some(SessionEvent::TurnEnd {
                    status, node_id, ..
                }) => return (status, node_id),
                Some(SessionEvent::Error(e)) => panic!("收到错误事件: {}", e),
                Some(_) => {}
                None => panic!("事件流提前结束"),
            }
        }
    }

    #[tokio::test]
    async fn stream_cancel_before_response_returns_cancelled() {
        let (url, request_count) =
            spawn_mock_server(vec![MockReply::DelayHeaders(Duration::from_secs(5))]).await;
        let session = new_http_test_session(url, true, Arc::new(ToolRegistry::new())).await;
        let (input_tx, input_rx) = mpsc::channel(1);
        let (mut events, handle) = session.try_run(input_rx).unwrap();

        input_tx.send("你好".to_string()).await.unwrap();
        wait_for_turn_begin(&mut events).await;
        wait_for_request_count(&request_count, 1).await;
        handle.cancel();
        drop(input_tx);

        let (status, _) = wait_for_turn_end(&mut events).await;
        assert!(matches!(status, TurnStatus::Cancelled));
    }

    #[tokio::test]
    async fn stream_cancel_after_partial_preserves_partial_and_recovers() {
        let (url, request_count) = spawn_mock_server(vec![
            MockReply::StreamPartialThenHold {
                content: "半句".to_string(),
                hold: Duration::from_secs(5),
            },
            MockReply::StreamDone {
                content: "完成".to_string(),
            },
        ])
        .await;
        let session = new_http_test_session(url, true, Arc::new(ToolRegistry::new())).await;
        let (input_tx, input_rx) = mpsc::channel(2);
        let (mut events, handle) = session.try_run(input_rx).unwrap();

        input_tx.send("第一轮".to_string()).await.unwrap();
        wait_for_turn_begin(&mut events).await;
        wait_for_request_count(&request_count, 1).await;
        assert_eq!(wait_for_content_delta(&mut events).await, "半句");
        handle.cancel();

        let (status, _) = wait_for_turn_end(&mut events).await;
        assert!(matches!(status, TurnStatus::Cancelled));
        let snapshot = handle.get_conversation().await;
        assert!(
            snapshot
                .messages
                .iter()
                .any(|m| m.role == "assistant" && m.content.as_deref() == Some("半句"))
        );

        input_tx.send("第二轮".to_string()).await.unwrap();
        wait_for_turn_begin(&mut events).await;
        wait_for_request_count(&request_count, 2).await;
        assert_eq!(wait_for_content_delta(&mut events).await, "完成");
        let (status, _) = wait_for_turn_end(&mut events).await;
        assert!(matches!(status, TurnStatus::Ok));
        drop(input_tx);
    }

    #[tokio::test]
    async fn non_stream_cancel_while_waiting_response_returns_cancelled() {
        let (url, request_count) =
            spawn_mock_server(vec![MockReply::DelayHeaders(Duration::from_secs(5))]).await;
        let session = new_http_test_session(url, false, Arc::new(ToolRegistry::new())).await;
        let (input_tx, input_rx) = mpsc::channel(1);
        let (mut events, handle) = session.try_run(input_rx).unwrap();

        input_tx.send("非流式".to_string()).await.unwrap();
        wait_for_turn_begin(&mut events).await;
        wait_for_request_count(&request_count, 1).await;
        handle.cancel();
        drop(input_tx);

        let (status, _) = wait_for_turn_end(&mut events).await;
        assert!(matches!(status, TurnStatus::Cancelled));
    }

    #[derive(Clone)]
    struct SlowToolState {
        started: Arc<AtomicBool>,
        finished: Arc<AtomicBool>,
    }

    #[tokio::test]
    async fn tool_execution_cancel_stops_followup_turn() {
        let (url, request_count) = spawn_mock_server(vec![
            MockReply::StreamToolCall {
                name: "slow_tool".to_string(),
            },
            MockReply::StreamDone {
                content: "不应请求第二轮".to_string(),
            },
        ])
        .await;
        let started = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let mut registry = ToolRegistry::new();
        registry.put_state::<crate::sense::SenseState<SlowToolState>>(Arc::new(
            tokio::sync::Mutex::new(SlowToolState {
                started: Arc::clone(&started),
                finished: Arc::clone(&finished),
            }),
        ));
        registry.register_async::<SlowToolState, _>(
            "slow_tool",
            "慢速测试工具",
            None::<Vec<ToolFunctionArg>>,
            |state, _args| {
                Box::pin(async move {
                    state.started.store(true, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    state.finished.store(true, Ordering::SeqCst);
                    Ok("完成".to_string())
                })
            },
        );

        let session = new_http_test_session(url, true, Arc::new(registry)).await;
        let (input_tx, input_rx) = mpsc::channel(1);
        let (mut events, handle) = session.try_run(input_rx).unwrap();

        input_tx.send("调用工具".to_string()).await.unwrap();
        wait_for_turn_begin(&mut events).await;
        wait_for_request_count(&request_count, 1).await;
        let started_at = Instant::now();
        while !started.load(Ordering::SeqCst) && started_at.elapsed() < Duration::from_secs(2) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(started.load(Ordering::SeqCst));

        handle.cancel();
        drop(input_tx);
        let (status, _) = wait_for_turn_end(&mut events).await;
        assert!(matches!(status, TurnStatus::Cancelled));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(request_count.load(Ordering::SeqCst), 1);
        assert!(!finished.load(Ordering::SeqCst));
    }

    #[test]
    fn context_insert_index_before_pending_block_keeps_latest_user_anchor() {
        let messages = vec![
            Message::system("基础系统提示"),
            Message::user("旧问题"),
            Message::assistant(Some("旧回答"), None::<String>, None),
            Message::user("新问题"),
        ];

        assert_eq!(
            LLMSession::context_insert_index_before_pending_block(&messages),
            3
        );
    }

    #[test]
    fn context_insert_index_before_pending_block_keeps_tool_call_block_adjacent() {
        let messages = vec![
            Message::system("基础系统提示"),
            Message::user("帮我查天气"),
            Message::assistant(
                None::<String>,
                None::<String>,
                Some(vec![ToolCall {
                    id: Some("call_1".to_string()),
                    call_type: Some("function".to_string()),
                    function: ToolFunctionCall {
                        name: "get_weather".to_string(),
                        arguments: "{}".to_string(),
                    },
                    index: 0,
                }]),
            ),
            Message::tool("晴天", "call_1"),
        ];

        assert_eq!(
            LLMSession::context_insert_index_before_pending_block(&messages),
            2
        );
    }

    #[test]
    fn context_insert_index_before_pending_block_keeps_multi_tool_results_adjacent() {
        let messages = vec![
            Message::system("基础系统提示"),
            Message::user("帮我同时查天气和汇率"),
            Message::assistant(
                None::<String>,
                None::<String>,
                Some(vec![
                    ToolCall {
                        id: Some("call_1".to_string()),
                        call_type: Some("function".to_string()),
                        function: ToolFunctionCall {
                            name: "get_weather".to_string(),
                            arguments: "{}".to_string(),
                        },
                        index: 0,
                    },
                    ToolCall {
                        id: Some("call_2".to_string()),
                        call_type: Some("function".to_string()),
                        function: ToolFunctionCall {
                            name: "get_fx_rate".to_string(),
                            arguments: "{}".to_string(),
                        },
                        index: 1,
                    },
                ]),
            ),
            Message::tool("晴天", "call_1"),
            Message::tool("7.25", "call_2"),
        ];

        assert_eq!(
            LLMSession::context_insert_index_before_pending_block(&messages),
            2
        );
    }

    #[test]
    fn run_with_context_channel_can_start_without_outer_tokio_runtime() {
        let session = new_test_session();
        let (input_tx, input_rx) = mpsc::channel(1);
        let (ctx_tx, ctx_rx) = mpsc::channel(1);
        drop(input_tx);
        drop(ctx_tx);

        let (_events, _handle) = session.run_with_context_channel(input_rx, ctx_rx);
    }

    #[test]
    fn preload_history_preserves_v3_root_when_file_order_is_unordered() {
        let mut session = new_test_session();
        session.preload_history(
            vec![
                stored_message(Some(2), Some(1), "assistant", "回复"),
                stored_message(Some(1), None, "user", "问题"),
            ],
            Some(2),
        );

        let tree = session.tree.blocking_read();
        assert_eq!(tree.get_node(1).unwrap().parent, None);
        assert_eq!(tree.get_node(2).unwrap().parent, Some(1));
        assert_eq!(tree.path_to_head(), vec![1, 2]);
    }

    #[test]
    fn preload_history_repairs_persisted_parent_cycle() {
        let mut session = new_test_session();
        session.preload_history(
            vec![
                stored_message(Some(1), Some(3), "user", "节点1"),
                stored_message(Some(2), Some(1), "assistant", "节点2"),
                stored_message(Some(3), Some(2), "user", "节点3"),
            ],
            Some(3),
        );

        let tree = session.tree.blocking_read();
        assert_eq!(tree.get_node(1).unwrap().parent, None);
        assert_eq!(tree.path_to_head(), vec![1, 2, 3]);
    }

    #[test]
    fn stream_turn_end_waits_for_qwen_usage_tail() {
        let usage = Usage {
            prompt_tokens: 10,
            completion_tokens: 20,
            total_tokens: 30,
        };

        assert!(!LLMSession::should_stop_after_stream_turn_end(
            &TurnStatus::Ok,
            &None
        ));

        log::debug!(
            "Qwen usage: prompt_tokens={}, completion_tokens={}, total_tokens={}",
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.total_tokens
        );
        assert!(LLMSession::should_stop_after_stream_turn_end(
            &TurnStatus::Ok,
            &Some(usage)
        ));

        assert!(LLMSession::should_stop_after_stream_turn_end(
            &TurnStatus::Cancelled,
            &None
        ));
    }
}
