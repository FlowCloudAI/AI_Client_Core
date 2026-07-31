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

    /// 从持久化历史恢复时先等待显式输入或 checkout，避免自动重放末尾未完成的用户消息。
    wait_for_input_on_start: bool,

    orchestrator: Option<Box<dyn Orchestrate>>,
}

// ── 构建 & 配置 ──

impl LLMSession {
    pub fn new(
        config: SessionConfig,
        pipeline: ApiPipeline,
        tool_registry: Arc<ToolRegistry>,
    ) -> Result<Self> {
        config.validate()?;
        let client = HttpPoster::new(config.request_timeout, config.max_line_bytes)?;
        Ok(Self {
            client,
            conversation: Arc::new(RwLock::new(ChatRequest::default())),
            tree: Arc::new(RwLock::new(ConversationTree::new())),
            system_messages: Arc::new(Vec::new()),
            tool_registry,
            config,
            pipeline,
            turn_id: 0,
            wait_for_input_on_start: false,
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
        self.wait_for_input_on_start = true;
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
        self.config.api_key = api_key.into();
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
                sys_msgs.append(&mut request.messages);
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
    /// 全部启动变体的共享实现：组装 channel / SessionHandle，驱动会话状态机。
    ///
    /// 驱动任务的落点：优先 `Handle::try_current()` spawn 到调用方现有 runtime
    /// （Tauri 命令、既有异步上下文），避免每个 session 各起一条 OS 线程 + runtime；
    /// 无 ambient runtime 时才退回自建 current_thread runtime 独占线程
    /// （保留“无外层 runtime 也能启动”的语义）。
    ///
    /// `ext_ctx_rx` 为调用方自持的上下文接收端（*_with_context_channel 变体），
    /// 会被转发到内部 latest-only watch 通道，与 handle.set_task_context 合并。
    fn start_inner(
        self,
        input_rx: mpsc::Receiver<String>,
        ext_ctx_rx: Option<mpsc::Receiver<TaskContext>>,
    ) -> Result<(ReceiverStream<SessionEvent>, SessionHandle)> {
        let (event_tx, event_rx) = mpsc::channel::<SessionEvent>(self.config.event_buffer);
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<CtrlMsg>(8);
        let (cancel_tx, cancel_rx) = watch::channel::<u64>(0);
        let (ctx_tx, ctx_rx) = watch::channel::<TaskContext>(TaskContext::default());
        let ctx_forward_tx = ctx_tx.clone();

        let handle = SessionHandle {
            inner: Arc::clone(&self.conversation),
            tree: Arc::clone(&self.tree),
            system_messages: Arc::clone(&self.system_messages),
            ctrl_tx,
            cancel_tx,
            ctx_tx,
        };

        let main_task = async move {
            // 将外部 ctx_rx 转发到内部 latest-only 通道，
            // 与 handle.set_task_context 合并为同一路输入。
            if let Some(mut ext_ctx_rx) = ext_ctx_rx {
                tokio::spawn(async move {
                    while let Some(ctx) = ext_ctx_rx.recv().await {
                        if ctx_forward_tx.send(ctx).is_err() {
                            break;
                        }
                    }
                });
            }

            if let Err(e) = self
                .drive(input_rx, ctrl_rx, Some(ctx_rx), cancel_rx, event_tx.clone())
                .await
            {
                let _ = event_tx
                    .send(SessionEvent::Error(ClientError::from_anyhow_owned(e)))
                    .await;
            }
        };

        match tokio::runtime::Handle::try_current() {
            Ok(rt_handle) => {
                rt_handle.spawn(main_task);
            }
            Err(_) => {
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
                    rt.block_on(main_task);
                });
            }
        }

        Ok((ReceiverStream::new(event_rx), handle))
    }

    /// `run` / `run_with_context_channel` 的兼容包装：
    /// 启动失败时返回一个只发送 `SessionEvent::Error` 的事件流。
    fn start_or_error_stream(
        self,
        input_rx: mpsc::Receiver<String>,
        ext_ctx_rx: Option<mpsc::Receiver<TaskContext>>,
    ) -> (ReceiverStream<SessionEvent>, SessionHandle) {
        let event_buffer = self.config.event_buffer;
        let fallback_handle = SessionHandle {
            inner: Arc::clone(&self.conversation),
            tree: Arc::clone(&self.tree),
            system_messages: Arc::clone(&self.system_messages),
            ctrl_tx: mpsc::channel::<CtrlMsg>(8).0,
            cancel_tx: watch::channel::<u64>(0).0,
            ctx_tx: watch::channel::<TaskContext>(TaskContext::default()).0,
        };
        match self.start_inner(input_rx, ext_ctx_rx) {
            Ok(pair) => pair,
            Err(e) => {
                let (event_tx, event_rx) = mpsc::channel::<SessionEvent>(event_buffer.max(1));
                let _ = event_tx.try_send(SessionEvent::Error(ClientError::from_anyhow_owned(e)));
                (ReceiverStream::new(event_rx), fallback_handle)
            }
        }
    }

    /// 启动会话，失败时通过 `Result` 返回。
    ///
    /// 推荐新代码使用此方法，避免 runtime 创建失败被隐藏。
    pub fn try_run(
        self,
        input_rx: mpsc::Receiver<String>,
    ) -> Result<(ReceiverStream<SessionEvent>, SessionHandle)> {
        self.start_inner(input_rx, None)
    }

    /// 启动会话。
    ///
    /// 兼容旧 API：如果启动失败，会返回一个只发送 `SessionEvent::Error` 的事件流。
    pub fn run(
        self,
        input_rx: mpsc::Receiver<String>,
    ) -> (ReceiverStream<SessionEvent>, SessionHandle) {
        self.start_or_error_stream(input_rx, None)
    }

    /// 底层版本：接受调用方自持的 ctx 接收端，适合将上下文流接入已有系统。
    ///
    /// 与 `run()` 的区别：
    /// - 调用方自己持有 `mpsc::Sender<TaskContext>`，可从任意异步上下文推送
    /// - `SessionHandle::set_task_context` 依然可用（内部合并两路来源）
    /// - 两路上下文都会覆盖内部最新值，每轮 assemble 前只读取最新上下文
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
        ext_ctx_rx: mpsc::Receiver<TaskContext>,
    ) -> Result<(ReceiverStream<SessionEvent>, SessionHandle)> {
        self.start_inner(input_rx, Some(ext_ctx_rx))
    }

    /// 底层版本：接受调用方自持的 ctx 接收端，适合将上下文流接入已有系统。
    ///
    /// 兼容旧 API：如果启动失败，会返回一个只发送 `SessionEvent::Error` 的事件流。
    pub fn run_with_context_channel(
        self,
        input_rx: mpsc::Receiver<String>,
        ext_ctx_rx: mpsc::Receiver<TaskContext>,
    ) -> (ReceiverStream<SessionEvent>, SessionHandle) {
        self.start_or_error_stream(input_rx, Some(ext_ctx_rx))
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

    fn apply_assembled(&self, mut req: ChatRequest, turn: &AssembledTurn) -> ChatRequest {
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
    ) -> Result<Option<u64>> {
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
                self.tree.write().await.checkout(node_id).map_err(|e| {
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
            CtrlMsg::Continue { node_id } => return Ok(Some(node_id)),
        }
        Ok(None)
    }

    async fn drive(
        mut self,
        mut input_rx: mpsc::Receiver<String>,
        mut ctrl_rx: mpsc::Receiver<CtrlMsg>,
        mut ctx_rx: Option<watch::Receiver<TaskContext>>,
        cancel_rx: watch::Receiver<u64>,
        event_tx: mpsc::Sender<SessionEvent>,
    ) -> Result<()> {
        let mut current_ctx = TaskContext::default();
        let mut tool_rounds = 0usize;
        let mut accumulated_usage: Option<Usage> = None;
        let mut force_wait_for_user = self.wait_for_input_on_start;
        let mut continuation_of: Option<u64> = None;
        let mut pending_continuation_context: Option<u64> = None;

        loop {
            if force_wait_for_user || self.should_wait_for_user().await {
                let forced_wait = force_wait_for_user;
                force_wait_for_user = false;
                tool_rounds = 0;
                accumulated_usage = None;
                continuation_of = None;
                pending_continuation_context = None;
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
                            continuation_of = None;
                            pending_continuation_context = None;
                            log::info!(
                                "[client:drive][user_message_added] next_turn_id={}",
                                self.turn_id + 1
                            );
                            break 'wait;
                        }
                        Either::Left((None, _)) => return Ok(()),
                        Either::Right((Some(ctrl), _)) => {
                            if let Some(node_id) = self.apply_ctrl(ctrl, &event_tx).await? {
                                continuation_of = Some(node_id);
                                pending_continuation_context = Some(node_id);
                                log::info!(
                                    "[client:drive][continuation_received] next_turn_id={} node_id={}",
                                    self.turn_id + 1,
                                    node_id
                                );
                                break 'wait;
                            }
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

            // 每轮开始前尝试更新 context（非阻塞）。上下文只保留最新值，避免等待用户输入时积压。
            if let Some(ref mut rx) = ctx_rx
                && rx.has_changed().unwrap_or(false)
            {
                current_ctx = rx.borrow_and_update().clone();
                log::info!(
                    "[client:drive][context_updated] next_turn_id={} project_id={:?} task_type={} attributes={} flags={}",
                    self.turn_id + 1,
                    current_ctx.project_id.as_deref(),
                    current_ctx.task_type,
                    current_ctx.attributes.len(),
                    current_ctx.flags.len()
                );
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
            let mut req = self.snapshot().await;
            if let Some(node_id) = pending_continuation_context.take() {
                if let Err(error) = self.append_continuation_context(&mut req, node_id).await {
                    event_tx
                        .send(SessionEvent::TurnEnd {
                            status: TurnStatus::Error(ClientError::from_anyhow_owned(error)),
                            node_id: None,
                            finish_reason: None,
                            continuation_of,
                            usage: accumulated_usage.take(),
                        })
                        .await?;
                    continue;
                }
            }
            let snapshot_elapsed_ms = stage_started.elapsed().as_millis();
            if log::log_enabled!(log::Level::Info) {
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
            }

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
                let req = self.apply_assembled(req, &assembled);
                (req, read_only)
            } else {
                (req, AssembledTurn::default().read_only)
            };
            let auto_confirm_writes = current_ctx
                .flags
                .get("auto_confirm_writes")
                .copied()
                .unwrap_or(false);
            let enabled_tools = Self::enabled_tool_names_from_request(&req);
            log::info!(
                "[client:drive][assemble_done] turn_id={} elapsed_ms={} read_only={} auto_confirm_writes={} messages={} tool_count={}",
                self.turn_id,
                stage_started.elapsed().as_millis(),
                read_only,
                auto_confirm_writes,
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

            let has_assistant_output = !content.is_empty()
                || !reasoning.is_empty()
                || tool_calls.as_ref().is_some_and(|calls| !calls.is_empty());
            let asst_node_id = if has_assistant_output {
                let safe_tool_calls = if matches!(
                    turn_status,
                    TurnStatus::Cancelled | TurnStatus::Interrupted | TurnStatus::Error(_)
                ) {
                    None
                } else {
                    tool_calls.clone()
                };
                Some(
                    self.add_message(Message::assistant(
                        Some(content).filter(|value: &String| !value.is_empty()),
                        Some(reasoning).filter(|value: &String| !value.is_empty()),
                        safe_tool_calls,
                    ))
                    .await,
                )
            } else {
                // 请求在首个有效输出前结束时保持 user head，并显式等待下一次输入，
                // 避免为了停止 drive 而写入无法发送给供应商的空 assistant 节点。
                force_wait_for_user = true;
                None
            };

            if finish_reason.as_deref() == Some("tool_calls")
                && let Some(calls) = tool_calls
            {
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
                    // 为每个未执行的 call 补占位 tool 消息，避免悬空 tool_calls
                    // 被持久化后打死会话。补完后 head 是 tool 节点，
                    // should_wait_for_user() 不再成立，必须显式强制等待，
                    // 否则 drive 会立即重发请求形成无限工具轮循环。
                    self.append_unexecuted_tool_placeholders(
                        MAX_ROUNDS_PLACEHOLDER_REASON,
                        calls,
                        &event_tx,
                    )
                    .await;
                    force_wait_for_user = true;
                    event_tx
                        .send(SessionEvent::TurnEnd {
                            status,
                            node_id: asst_node_id,
                            finish_reason: finish_reason.clone(),
                            continuation_of,
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
                        auto_confirm_writes,
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
                            finish_reason: Some("cancelled".to_string()),
                            continuation_of,
                            usage: accumulated_usage.take(),
                        })
                        .await?;
                    continue;
                }
                continue;
            }

            event_tx
                .send(SessionEvent::TurnEnd {
                    status: turn_status,
                    node_id: asst_node_id,
                    finish_reason,
                    continuation_of,
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
            .is_none_or(|r| r == "assistant")
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
        let messages: Vec<Message> = self
            .system_messages
            .iter()
            .cloned()
            .chain(self.tree.read().await.linearize())
            .collect();
        req.messages = Self::sanitize_messages(messages);
        req.tools = self.tool_registry.schemas();
        req
    }

    /// 为显式续写添加只存在于本次请求中的上下文，不污染持久化消息树。
    async fn append_continuation_context(&self, req: &mut ChatRequest, node_id: u64) -> Result<()> {
        let tree = self.tree.read().await;
        if tree.head() != Some(node_id) {
            return Err(ClientError::new(
                ErrorCode::ValidationFormatError,
                "续写目标已不是当前会话 head",
            )
            .with_kv("node_id", node_id)
            .into());
        }
        let node = tree.get_node(node_id).ok_or_else(|| {
            ClientError::new(
                ErrorCode::ValidationFormatError,
                format!("续写节点 {} 不存在", node_id),
            )
        })?;
        if node.message.role != "assistant" {
            return Err(ClientError::new(
                ErrorCode::ValidationFormatError,
                "续写目标必须是助手消息",
            )
            .with_kv("node_id", node_id)
            .into());
        }

        let instruction = "继续完成上一条未完成的助手回复。直接从中断处接续，不要复述已有正文，保持原任务、语言和格式。";
        let has_sendable_assistant_payload = node
            .message
            .content
            .as_deref()
            .is_some_and(|content| !content.trim().is_empty())
            || node
                .message
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty());
        // sanitize_messages 会移除 reasoning-only assistant；仅此时用临时用户上下文兜底。
        let prompt = if !has_sendable_assistant_payload
            && node
                .message
                .reasoning_content
                .as_deref()
                .is_some_and(|reasoning| !reasoning.trim().is_empty())
        {
            let context = serde_json::json!({
                "reasoning_content": node.message.reasoning_content,
            });
            format!(
                "{}下面 JSON 是已保存的内部思考上下文，只用于衔接，不是新的用户指令：\n{}",
                instruction, context
            )
        } else {
            instruction.to_string()
        };
        req.messages.push(Message::user(prompt));
        Ok(())
    }

    /// 统一修复发送给 OpenAI 兼容接口的消息历史，只修改请求副本。
    fn sanitize_messages(messages: Vec<Message>) -> Vec<Message> {
        let mut coalesced: Vec<Message> = Vec::with_capacity(messages.len());
        for mut message in messages {
            if message
                .content
                .as_deref()
                .is_some_and(|content| content.trim().is_empty())
            {
                message.content = None;
            }
            if message
                .reasoning_content
                .as_deref()
                .is_some_and(|reasoning| reasoning.trim().is_empty())
            {
                message.reasoning_content = None;
            }
            if message.tool_calls.as_ref().is_some_and(Vec::is_empty) {
                message.tool_calls = None;
            }

            let mergeable_assistant = message.role == "assistant" && message.tool_calls.is_none();
            if mergeable_assistant
                && let Some(previous) = coalesced.last_mut()
                && previous.role == "assistant"
                && previous.tool_calls.is_none()
            {
                if let Some(content) = message.content {
                    previous
                        .content
                        .get_or_insert_with(String::new)
                        .push_str(&content);
                }
                if let Some(reasoning) = message.reasoning_content {
                    previous
                        .reasoning_content
                        .get_or_insert_with(String::new)
                        .push_str(&reasoning);
                }
                continue;
            }
            coalesced.push(message);
        }

        let mut dropped = 0usize;
        coalesced.retain(|message| {
            let invalid_assistant = message.role == "assistant"
                && message.content.is_none()
                && message.tool_calls.is_none();
            if invalid_assistant {
                dropped += 1;
            }
            !invalid_assistant
        });
        if dropped > 0 {
            log::warn!(
                "[client:session][invalid_assistant_messages_dropped] count={}",
                dropped
            );
        }

        Self::sanitize_tool_call_blocks(coalesced)
    }

    /// 修复请求消息序列中的悬空/错配 tool_calls 块（只作用于请求副本，不动树）。
    ///
    /// 历史损坏可能已被持久化（取消/超限旧版本不补 tool 消息，且旧版非流式
    /// 路径 assistant 侧存 provider ID、tool 侧存合成 ID），这类历史一旦原样
    /// 发出会被 OpenAI 兼容 API 以 400 拒绝且每轮复发。处理三类损坏：
    /// 1. assistant(tool_calls) 后缺少部分或全部 tool 结果 → 补占位消息；
    /// 2. tool 消息数量在但 tool_call_id 与 call.id 错配 → 按位置配对重写 ID；
    /// 3. 无法归属到任何 call 的多余 tool 消息 → 丢弃。
    fn sanitize_tool_call_blocks(messages: Vec<Message>) -> Vec<Message> {
        let mut out: Vec<Message> = Vec::with_capacity(messages.len());
        let mut iter = messages.into_iter().peekable();
        while let Some(mut msg) = iter.next() {
            let has_calls =
                msg.role == "assistant" && msg.tool_calls.as_ref().is_some_and(|c| !c.is_empty());
            if !has_calls {
                // 没有 assistant(tool_calls) 锚点的孤儿 tool 消息同样会被 API 拒绝。
                if msg.role == "tool" {
                    log::warn!(
                        "[client:session][orphan_tool_message_dropped] position={}",
                        out.len()
                    );
                    continue;
                }
                out.push(msg);
                continue;
            }

            let mut tools: Vec<Message> = Vec::new();
            while iter.peek().is_some_and(|m| m.role == "tool") {
                tools.push(iter.next().expect("peek 已确认存在"));
            }

            // 空 call.id 无法与任何 tool 消息配对，先在副本上归一化补齐。
            let anchor = out.len();
            let calls = msg.tool_calls.as_mut().expect("has_calls 已确认非空");
            for call in calls.iter_mut() {
                if call.id.as_deref().is_none_or(str::is_empty) {
                    call.id = Some(format!("sanitized:{}:{}", anchor, call.index));
                }
            }
            let expected: Vec<String> = calls
                .iter()
                .map(|c| c.id.clone().expect("上方已归一化"))
                .collect();

            // 第一遍：按 ID 精确配对。
            let mut tool_matched = vec![false; tools.len()];
            let mut call_matched = vec![false; expected.len()];
            for (ci, id) in expected.iter().enumerate() {
                if let Some(ti) = (0..tools.len())
                    .find(|&ti| !tool_matched[ti] && tools[ti].tool_call_id.as_deref() == Some(id))
                {
                    tool_matched[ti] = true;
                    call_matched[ci] = true;
                }
            }

            // 第二遍：剩余的按位置配对并重写 tool_call_id。不能对未匹配 call
            // 一律补占位——"数量齐但 ID 错配"的存量块会被误判为全缺失，插入
            // 重复 tool 消息反而让请求更坏。
            let unmatched_calls: Vec<usize> = (0..expected.len())
                .filter(|&ci| !call_matched[ci])
                .collect();
            let unmatched_tools: Vec<usize> =
                (0..tools.len()).filter(|&ti| !tool_matched[ti]).collect();
            let mut rewritten = 0usize;
            for (&ci, &ti) in unmatched_calls.iter().zip(unmatched_tools.iter()) {
                tools[ti].tool_call_id = Some(expected[ci].clone());
                tool_matched[ti] = true;
                call_matched[ci] = true;
                rewritten += 1;
            }

            // 第三遍：仍未覆盖的 call 补占位；未归属的 tool 丢弃。
            let missing: Vec<String> = (0..expected.len())
                .filter(|&ci| !call_matched[ci])
                .map(|ci| expected[ci].clone())
                .collect();
            let dropped = tool_matched.iter().filter(|matched| !**matched).count();
            if rewritten > 0 || !missing.is_empty() || dropped > 0 {
                log::warn!(
                    "[client:session][tool_block_sanitized] anchor={} calls={} rewritten={} missing={} dropped={}",
                    anchor,
                    expected.len(),
                    rewritten,
                    missing.len(),
                    dropped
                );
            }

            out.push(msg);
            for (ti, tool) in tools.into_iter().enumerate() {
                if tool_matched[ti] {
                    out.push(tool);
                }
            }
            for id in missing {
                out.push(Message::tool(
                    "工具执行失败: 会话历史中缺失该工具调用的结果（已自动补齐占位）",
                    id,
                ));
            }
        }
        out
    }

    async fn add_message(&self, msg: Message) -> u64 {
        self.tree.write().await.append(msg, self.turn_id)
    }
}

// ── 插件映射（核心变化点） ──

impl LLMSession {
    /// 请求转换：acquire mapper → map → release（自动）。
    fn prepare_request(&self, req: &ChatRequest) -> Result<String> {
        self.pipeline
            .validate_llm_request(&req.model, req.thinking_effort)?;
        self.pipeline.prepare_request_body(req)
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

        let first_result = self.send_once(req, cancel, event_tx).await;
        let Err(ref error) = first_result else {
            return first_result;
        };
        let Some(client_error) = ClientError::from_anyhow(error) else {
            return first_result;
        };
        let Some((repaired, rule)) = Self::repair_after_bad_request(req, client_error) else {
            return first_result;
        };
        if cancel.is_cancelled() {
            return Ok(cancelled_turn_output(
                String::new(),
                String::new(),
                Vec::new(),
                None,
            ));
        }
        log::warn!(
            "[client:llm][request_auto_repair] turn_id={} rule={} retry=1",
            self.turn_id,
            rule
        );
        self.send_once(&repaired, cancel, event_tx).await
    }

    async fn send_once(
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

    /// 对供应商明确指出的安全问题做一次确定性修复；返回 None 时不得重试。
    fn repair_after_bad_request(
        req: &ChatRequest,
        error: &ClientError,
    ) -> Option<(ChatRequest, &'static str)> {
        if error.code != ErrorCode::HttpBadRequest {
            return None;
        }
        let provider_message = error
            .detail
            .get("provider_message")
            .and_then(Value::as_str)?
            .to_ascii_lowercase();
        let mut repaired = req.clone();

        if provider_message.contains("reasoning_content")
            && ["not allowed", "unsupported", "unknown", "extra inputs"]
                .iter()
                .any(|marker| provider_message.contains(marker))
        {
            let mut changed = false;
            for message in &mut repaired.messages {
                changed |= message.reasoning_content.take().is_some();
            }
            if changed {
                return Some((repaired, "unsupported_reasoning_content"));
            }
        }

        if provider_message.contains("invalid assistant message")
            || provider_message.contains("content or tool_calls must be set")
        {
            let previous_len = repaired.messages.len();
            repaired.messages = Self::sanitize_messages(repaired.messages);
            if repaired.messages.len() != previous_len {
                return Some((repaired, "invalid_assistant_message"));
            }
        }

        if provider_message.contains("tools")
            && ["empty", "at least one", "must contain"]
                .iter()
                .any(|marker| provider_message.contains(marker))
            && repaired.tools.as_ref().is_some_and(Vec::is_empty)
        {
            repaired.tools = None;
            repaired.tool_choice = None;
            return Some((repaired, "empty_tools"));
        }

        None
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
        let body = self.prepare_request(req)?;
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
            body.len()
        );

        let raw_line = {
            log::info!(
                "[client:llm][non_stream_http_send_start] turn_id={} base_url={}",
                self.turn_id,
                self.config.base_url
            );
            let stage_started = Instant::now();
            let post_fut =
                self.client
                    .post_collect(&self.config.base_url, self.config.api_key.expose(), body);
            let raw_body = tokio::select! {
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
            if raw_body.is_empty() {
                return Err(ClientError::new(ErrorCode::LlmResponseEmpty, "LLM 响应为空").into());
            }
            raw_body
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
        let body = self.prepare_request(req)?;
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
            body.len()
        );

        log::info!(
            "[client:llm][stream_http_send_start] turn_id={} base_url={}",
            self.turn_id,
            self.config.base_url
        );
        let stage_started = Instant::now();
        let post_fut =
            self.client
                .post_json(&self.config.base_url, self.config.api_key.expose(), body);
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
        let mut saw_tool_call_start = false;

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
            let line = match raw_line {
                Ok(line) => line,
                Err(error) if !full_content.is_empty() || !full_reasoning.is_empty() => {
                    let error = ClientError::from_anyhow_owned(error);
                    log::warn!(
                        "[client:llm][stream_read_interrupted] turn_id={} content_chars={} reasoning_chars={} error={}",
                        self.turn_id,
                        full_content.chars().count(),
                        full_reasoning.chars().count(),
                        error
                    );
                    turn_status = TurnStatus::Error(error);
                    finish_reason = Some("interrupted".to_string());
                    usage = decoder.take_pending_usage();
                    break 'outer;
                }
                Err(error) => return Err(error),
            };
            if cancel.is_cancelled() {
                turn_status = TurnStatus::Cancelled;
                finish_reason = Some("cancelled".to_string());
                usage = decoder.take_pending_usage();
                break 'outer;
            }
            line_count += 1;
            if line_count == 1 || line_count.is_multiple_of(50) {
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
            let normalized = match self.normalize_stream_line(&line) {
                Ok(normalized) => normalized,
                Err(error) if !full_content.is_empty() || !full_reasoning.is_empty() => {
                    let error = ClientError::from_anyhow_owned(error);
                    log::warn!(
                        "[client:llm][stream_map_interrupted] turn_id={} content_chars={} reasoning_chars={} error={}",
                        self.turn_id,
                        full_content.chars().count(),
                        full_reasoning.chars().count(),
                        error
                    );
                    turn_status = TurnStatus::Error(error);
                    finish_reason = Some("interrupted".to_string());
                    usage = decoder.take_pending_usage();
                    break 'outer;
                }
                Err(error) => return Err(error),
            };

            let events = decoder.decode(&normalized);

            for ev in events {
                let ev = match ev {
                    Ok(event) => event,
                    Err(error) if !full_content.is_empty() || !full_reasoning.is_empty() => {
                        let error = ClientError::from_anyhow_owned(error);
                        log::warn!(
                            "[client:llm][stream_decode_interrupted] turn_id={} content_chars={} reasoning_chars={} error={}",
                            self.turn_id,
                            full_content.chars().count(),
                            full_reasoning.chars().count(),
                            error
                        );
                        turn_status = TurnStatus::Error(error);
                        finish_reason = Some("interrupted".to_string());
                        usage = decoder.take_pending_usage();
                        break 'outer;
                    }
                    Err(error) => return Err(error),
                };

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
                        saw_tool_call_start = true;
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

                    DecoderEventPayload::TurnEnd {
                        status,
                        finish_reason: stream_finish_reason,
                        usage: u,
                    } => {
                        turn_status = status.clone();
                        if u.is_some() {
                            usage = u;
                        }
                        let normalized_finish_reason = match &turn_status {
                            TurnStatus::Ok => stream_finish_reason
                                .clone()
                                .unwrap_or_else(|| "stop".to_string()),
                            TurnStatus::Cancelled => "cancelled".to_string(),
                            TurnStatus::Interrupted => "interrupted".to_string(),
                            TurnStatus::Error(error)
                                if !full_content.is_empty() || !full_reasoning.is_empty() =>
                            {
                                turn_status = TurnStatus::Error(error.clone());
                                finish_reason = Some("interrupted".to_string());
                                break 'outer;
                            }
                            TurnStatus::Error(error) => return Err(error.clone().into()),
                        };
                        // [DONE] 常被解码为 stop；不能覆盖此前明确的 length 等结束原因。
                        if finish_reason.is_none() || normalized_finish_reason != "stop" {
                            finish_reason = Some(normalized_finish_reason.clone());
                        }
                        log::info!(
                            "[client:llm][stream_turn_end_event] turn_id={} status={} finish_reason={} saw_tool_call_start={} content_chars={} reasoning_chars={}",
                            self.turn_id,
                            match &turn_status {
                                TurnStatus::Ok => "ok",
                                TurnStatus::Cancelled => "cancelled",
                                TurnStatus::Interrupted => "interrupted",
                                TurnStatus::Error(_) => "error",
                            },
                            normalized_finish_reason,
                            saw_tool_call_start,
                            full_content.chars().count(),
                            full_reasoning.chars().count()
                        );

                        if saw_tool_call_start && normalized_finish_reason != "tool_calls" {
                            let status = TurnStatus::Error(
                                ClientError::new(
                                    ErrorCode::LlmStreamProtocolError,
                                    "模型开始输出工具调用，但未以 tool_calls 结束，工具未执行",
                                )
                                .with_kv("finish_reason", normalized_finish_reason.clone())
                                .with_kv("turn_id", self.turn_id)
                                .with_kv("content_chars", full_content.chars().count() as u64)
                                .with_kv("reasoning_chars", full_reasoning.chars().count() as u64),
                            );
                            log::warn!(
                                "[client:llm][incomplete_tool_call_stream] turn_id={} finish_reason={} content_chars={} reasoning_chars={}",
                                self.turn_id,
                                normalized_finish_reason,
                                full_content.chars().count(),
                                full_reasoning.chars().count()
                            );
                            turn_status = status;
                            break 'outer;
                        }

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

/// 工具被取消/超限时补进树的占位 tool 消息文案。
/// 前缀对齐既有 "工具执行失败: ..." 风格，便于模型理解这不是正常结果。
const CANCEL_PLACEHOLDER_REASON: &str = "工具执行失败: 用户取消了本轮对话，该工具调用未执行";
const MAX_ROUNDS_PLACEHOLDER_REASON: &str =
    "工具执行失败: 已达最大连续工具调用轮数上限，该工具调用未执行";

impl LLMSession {
    async fn execute_tool_calls(
        &mut self,
        tool_calls: Vec<ToolCall>,
        enabled_tools: &Option<HashSet<String>>,
        read_only: bool,
        auto_confirm_writes: bool,
        cancel: &mut TurnCancel,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<bool> {
        let total_calls = tool_calls.len();
        let enabled_scope = enabled_tools
            .as_ref()
            .map(|tools| tools.len().to_string())
            .unwrap_or_else(|| "all".to_string());
        log::info!(
            "[client:tools][batch_start] turn_id={} calls={} read_only={} enabled_scope={}",
            self.turn_id,
            total_calls,
            read_only,
            enabled_scope
        );

        let mut calls_iter = tool_calls.into_iter().enumerate();
        while let Some((call_position, call)) = calls_iter.next() {
            if cancel.is_cancelled() {
                log::warn!(
                    "[client:tools][batch_cancelled_before_call] turn_id={} next_call_position={} calls={}",
                    self.turn_id,
                    call_position + 1,
                    total_calls
                );
                let pending: Vec<ToolCall> = std::iter::once(call)
                    .chain(calls_iter.by_ref().map(|(_, c)| c))
                    .collect();
                self.append_unexecuted_tool_placeholders(
                    CANCEL_PLACEHOLDER_REASON,
                    pending,
                    event_tx,
                )
                .await;
                return Ok(true);
            }

            let func_name = &call.function.name;
            let args_str = call.function.arguments.trim();
            log::info!(
                "[client:tools][call_start] turn_id={} call_position={}/{} index={} name={} args_chars={} args_preview={:?}",
                self.turn_id,
                call_position + 1,
                total_calls,
                call.index,
                func_name,
                args_str.chars().count(),
                Self::log_preview(args_str, 512)
            );

            let (output, is_error) = if enabled_tools
                .as_ref()
                .is_some_and(|tools| !tools.contains(func_name))
            {
                log::warn!(
                    "[client:tools][call_blocked_not_enabled] turn_id={} index={} name={}",
                    self.turn_id,
                    call.index,
                    func_name
                );
                (
                    format!("工具执行失败: 本轮不允许调用工具 '{}'", func_name),
                    true,
                )
            } else if read_only && !self.tool_registry.is_read_tool(func_name) {
                log::warn!(
                    "[client:tools][call_blocked_read_only] turn_id={} index={} name={}",
                    self.turn_id,
                    call.index,
                    func_name
                );
                (
                    format!(
                        "工具执行失败: 只读模式下仅允许显式标注为读的工具，'{}' 未标注或为写工具",
                        func_name
                    ),
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
                            log::warn!(
                                "[client:tools][call_args_parse_failed] turn_id={} index={} name={} error={} args_preview={:?}",
                                self.turn_id,
                                call.index,
                                func_name,
                                e,
                                Self::log_preview(args_str, 512)
                            );
                            event_tx
                                .send(SessionEvent::ToolResult {
                                    index: call.index,
                                    output: output.clone(),
                                    is_error: true,
                                })
                                .await?;
                            log::info!(
                                "[client:tools][tool_result_event_sent] turn_id={} index={} name={} is_error=true output_chars={}",
                                self.turn_id,
                                call.index,
                                func_name,
                                output.chars().count()
                            );
                            let tool_call_id = Self::tool_message_id(&call, self.turn_id);
                            let _ = self.add_message(Message::tool(output, tool_call_id)).await;
                            log::info!(
                                "[client:tools][tool_message_added] turn_id={} index={} name={}",
                                self.turn_id,
                                call.index,
                                func_name
                            );
                            if cancel.is_cancelled() {
                                log::warn!(
                                    "[client:tools][batch_cancelled_after_parse_error] turn_id={} index={} name={}",
                                    self.turn_id,
                                    call.index,
                                    func_name
                                );
                                let pending: Vec<ToolCall> =
                                    calls_iter.by_ref().map(|(_, c)| c).collect();
                                self.append_unexecuted_tool_placeholders(
                                    CANCEL_PLACEHOLDER_REASON,
                                    pending,
                                    event_tx,
                                )
                                .await;
                                return Ok(true);
                            }
                            continue;
                        }
                    }
                };

                log::info!(
                    "[client:tools][conduct_start] turn_id={} index={} name={}",
                    self.turn_id,
                    call.index,
                    func_name
                );
                let conduct_started = Instant::now();
                let conduct_fut = crate::tool::with_auto_confirm_writes(
                    auto_confirm_writes,
                    self.tool_registry
                        .conduct(func_name, Some(&args_v), Duration::from_secs(600)),
                );
                match tokio::select! {
                    result = conduct_fut => result,
                    _ = cancel.cancelled() => {
                        log::warn!(
                            "[client:tools][conduct_cancelled] turn_id={} index={} name={} elapsed_ms={}",
                            self.turn_id,
                            call.index,
                            func_name,
                            conduct_started.elapsed().as_millis()
                        );
                        let pending: Vec<ToolCall> = std::iter::once(call.clone())
                            .chain(calls_iter.by_ref().map(|(_, c)| c))
                            .collect();
                        self.append_unexecuted_tool_placeholders(
                            CANCEL_PLACEHOLDER_REASON,
                            pending,
                            event_tx,
                        )
                        .await;
                        return Ok(true);
                    },
                } {
                    Ok(o) => {
                        log::info!(
                            "[client:tools][conduct_done] turn_id={} index={} name={} elapsed_ms={} output_chars={}",
                            self.turn_id,
                            call.index,
                            func_name,
                            conduct_started.elapsed().as_millis(),
                            o.chars().count()
                        );
                        (o, false)
                    }
                    Err(e) => {
                        log::warn!(
                            "[client:tools][conduct_failed] turn_id={} index={} name={} elapsed_ms={} error={}",
                            self.turn_id,
                            call.index,
                            func_name,
                            conduct_started.elapsed().as_millis(),
                            e
                        );
                        (format!("工具执行失败: {}", e), true)
                    }
                }
            };

            let tool_call_id = Self::tool_message_id(&call, self.turn_id);

            event_tx
                .send(SessionEvent::ToolResult {
                    index: call.index,
                    output: output.clone(),
                    is_error,
                })
                .await?;
            log::info!(
                "[client:tools][tool_result_event_sent] turn_id={} index={} name={} is_error={} output_chars={} output_preview={:?}",
                self.turn_id,
                call.index,
                func_name,
                is_error,
                output.chars().count(),
                Self::log_preview(&output, 512)
            );

            let _ = self.add_message(Message::tool(output, tool_call_id)).await;
            log::info!(
                "[client:tools][tool_message_added] turn_id={} index={} name={}",
                self.turn_id,
                call.index,
                func_name
            );
            if cancel.is_cancelled() {
                log::warn!(
                    "[client:tools][batch_cancelled_after_call] turn_id={} index={} name={}",
                    self.turn_id,
                    call.index,
                    func_name
                );
                let pending: Vec<ToolCall> = calls_iter.by_ref().map(|(_, c)| c).collect();
                self.append_unexecuted_tool_placeholders(
                    CANCEL_PLACEHOLDER_REASON,
                    pending,
                    event_tx,
                )
                .await;
                return Ok(true);
            }
        }

        log::info!(
            "[client:tools][batch_done] turn_id={} calls={}",
            self.turn_id,
            total_calls
        );
        Ok(false)
    }

    fn log_preview(value: &str, max_chars: usize) -> String {
        let mut chars = value.chars();
        let preview: String = chars.by_ref().take(max_chars).collect();
        if chars.next().is_some() {
            format!("{}...(truncated)", preview)
        } else {
            preview
        }
    }

    /// tool 消息的 tool_call_id：provider 给了真实 id 就用它（与 assistant 侧
    /// tool_calls 里的 id 配对），否则退回合成 ID。
    fn tool_message_id(call: &ToolCall, turn_id: u64) -> String {
        call.id
            .as_deref()
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| Self::synth_tool_call_id(turn_id, call.index))
    }

    /// 为未执行的 tool_calls 补占位 tool 消息。
    ///
    /// assistant(tool_calls) 节点已入树且会被持久化；任何提前退出若不为每个
    /// call 留下配对的 tool 消息，该历史再发给 OpenAI 兼容 API 会被 400 拒绝
    /// 且每轮复发。树写入优先于事件——事件发送失败只影响本次 UI 展示。
    async fn append_unexecuted_tool_placeholders(
        &self,
        reason: &str,
        calls: Vec<ToolCall>,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) {
        for call in calls {
            let tool_call_id = Self::tool_message_id(&call, self.turn_id);
            let _ = self.add_message(Message::tool(reason, tool_call_id)).await;
            let _ = event_tx
                .send(SessionEvent::ToolResult {
                    index: call.index,
                    output: reason.to_string(),
                    is_error: true,
                })
                .await;
            log::info!(
                "[client:tools][placeholder_tool_message_added] turn_id={} index={} name={}",
                self.turn_id,
                call.index,
                call.function.name
            );
        }
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
    use crate::orchestrator::TaskContext;
    use crate::plugin::pipeline::ApiPipeline;
    use crate::plugin::registry::PluginRegistry;
    use crate::tool::registry::ToolRegistry;
    use futures_util::StreamExt;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;

    fn new_test_session() -> LLMSession {
        let registry = Arc::new(PluginRegistry::empty().unwrap());
        let pipeline = ApiPipeline::try_new(registry, None).unwrap();
        let config = SessionConfig {
            base_url: "https://example.test".to_string(),
            api_key: "test-key".into(),
            ..SessionConfig::default()
        };
        LLMSession::new(config, pipeline, Arc::new(ToolRegistry::new())).unwrap()
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
        let config = SessionConfig {
            base_url,
            api_key: "test-key".into(),
            event_buffer: 64,
            max_tool_rounds: 4,
            ..SessionConfig::default()
        };

        let mut session = LLMSession::new(config, pipeline, tool_registry).unwrap();
        session.set_model("mock-model").await;
        session.set_stream(stream).await;
        session
    }

    enum MockReply {
        DelayHeaders(Duration),
        HttpBadRequest { message: String },
        StreamPartialThenHold { content: String, hold: Duration },
        StreamPartialThenDisconnect { content: String },
        StreamLength { content: String },
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
            MockReply::HttpBadRequest { message } => {
                let body = serde_json::json!({"error": {"message": message}}).to_string();
                let response = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
            MockReply::StreamPartialThenHold { content, hold } => {
                let _ = write_stream_headers(&mut socket).await;
                let _ = write_sse_line(&mut socket, stream_content_chunk(&content, None)).await;
                let _ = socket.flush().await;
                tokio::time::sleep(hold).await;
            }
            MockReply::StreamPartialThenDisconnect { content } => {
                let body = format!("data: {}\n\n", stream_content_chunk(&content, None));
                let declared_length = body.len() + 128;
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\n"
                );
                let _ = socket.write_all(headers.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
                let _ = socket.flush().await;
            }
            MockReply::StreamLength { content } => {
                let _ = write_stream_headers(&mut socket).await;
                let _ = write_sse_line(&mut socket, stream_content_chunk(&content, None)).await;
                let _ = write_sse_line(&mut socket, stream_content_chunk("", Some("length"))).await;
                let _ = write_sse_line(&mut socket, "[DONE]".to_string()).await;
                let _ = socket.flush().await;
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

    async fn read_request_body(socket: &mut TcpStream) -> std::io::Result<String> {
        let mut buf = [0u8; 2048];
        let mut data = Vec::new();
        let header_end = loop {
            let n = socket.read(&mut buf).await?;
            if n == 0 {
                return Ok(String::new());
            }
            data.extend_from_slice(&buf[..n]);
            if let Some(index) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8_lossy(&data[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while data.len() < header_end + content_length {
            let n = socket.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
        }
        Ok(String::from_utf8_lossy(&data[header_end..]).to_string())
    }

    async fn spawn_capturing_stream_server() -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (body_tx, body_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let body = read_request_body(&mut socket).await.unwrap_or_default();
            let _ = body_tx.send(body);
            let _ = write_stream_headers(&mut socket).await;
            let _ =
                write_sse_line(&mut socket, stream_content_chunk("续写正文", Some("stop"))).await;
            let _ = write_sse_line(&mut socket, "[DONE]".to_string()).await;
            let _ = socket.flush().await;
        });
        (format!("http://{}", addr), body_rx)
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

    async fn wait_for_turn_end(
        events: &mut ReceiverStream<SessionEvent>,
    ) -> (TurnStatus, Option<u64>, Option<String>, Option<u64>) {
        loop {
            match events.next().await {
                Some(SessionEvent::TurnEnd {
                    status,
                    node_id,
                    finish_reason,
                    continuation_of,
                    ..
                }) => return (status, node_id, finish_reason, continuation_of),
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

        let (status, node_id, _, _) = wait_for_turn_end(&mut events).await;
        assert!(matches!(status, TurnStatus::Cancelled));
        assert_eq!(node_id, None);
        assert!(
            handle
                .get_conversation()
                .await
                .messages
                .iter()
                .all(|message| message.role != "assistant")
        );
    }

    #[tokio::test]
    async fn stream_disconnect_after_partial_preserves_partial() {
        let (url, request_count) =
            spawn_mock_server(vec![MockReply::StreamPartialThenDisconnect {
                content: "已生成部分".to_string(),
            }])
            .await;
        let session = new_http_test_session(url, true, Arc::new(ToolRegistry::new())).await;
        let (input_tx, input_rx) = mpsc::channel(1);
        let (mut events, handle) = session.try_run(input_rx).unwrap();

        input_tx.send("开始生成".to_string()).await.unwrap();
        wait_for_turn_begin(&mut events).await;
        wait_for_request_count(&request_count, 1).await;
        assert_eq!(wait_for_content_delta(&mut events).await, "已生成部分");

        let (status, node_id, finish_reason, _) = wait_for_turn_end(&mut events).await;
        assert!(matches!(status, TurnStatus::Error(_)));
        assert!(node_id.is_some());
        assert_eq!(finish_reason.as_deref(), Some("interrupted"));
        assert!(
            handle
                .get_conversation()
                .await
                .messages
                .iter()
                .any(|message| {
                    message.role == "assistant" && message.content.as_deref() == Some("已生成部分")
                })
        );
        drop(input_tx);
    }

    #[tokio::test]
    async fn stream_length_preserves_finish_reason() {
        let (url, _) = spawn_mock_server(vec![MockReply::StreamLength {
            content: "达到上限".to_string(),
        }])
        .await;
        let session = new_http_test_session(url, true, Arc::new(ToolRegistry::new())).await;
        let (input_tx, input_rx) = mpsc::channel(1);
        let (mut events, _handle) = session.try_run(input_rx).unwrap();

        input_tx.send("生成长文".to_string()).await.unwrap();
        assert_eq!(wait_for_content_delta(&mut events).await, "达到上限");
        let (status, node_id, finish_reason, _) = wait_for_turn_end(&mut events).await;
        assert!(matches!(status, TurnStatus::Ok));
        assert!(node_id.is_some());
        assert_eq!(finish_reason.as_deref(), Some("length"));
        drop(input_tx);
    }

    #[tokio::test]
    async fn reasoning_only_continuation_uses_ephemeral_context() {
        let (url, body_rx) = spawn_capturing_stream_server().await;
        let mut session = new_http_test_session(url, true, Arc::new(ToolRegistry::new())).await;
        session.preload_history(
            vec![
                ConversationNodeSeed {
                    node_id: Some(1),
                    parent: None,
                    turn_id: Some(1),
                    timestamp: Some("2026-07-31T00:00:00Z".to_string()),
                    message: Message::user("写一篇长文"),
                },
                ConversationNodeSeed {
                    node_id: Some(2),
                    parent: Some(1),
                    turn_id: Some(1),
                    timestamp: Some("2026-07-31T00:00:01Z".to_string()),
                    message: Message::assistant(None::<String>, Some("已有思考上下文"), None),
                },
            ],
            Some(2),
        );
        let (input_tx, input_rx) = mpsc::channel(1);
        let (mut events, handle) = session.try_run(input_rx).unwrap();

        handle.continue_generation(2).await.unwrap();
        assert_eq!(wait_for_content_delta(&mut events).await, "续写正文");
        let request_body = body_rx.await.unwrap();
        let request: serde_json::Value = serde_json::from_str(&request_body).unwrap();
        let messages = request["messages"].as_array().unwrap();
        assert!(messages.iter().all(|message| {
            message["role"] != "assistant"
                || message
                    .get("content")
                    .is_some_and(|content| !content.is_null())
                || message.get("tool_calls").is_some()
        }));
        let continuation_prompt = messages.last().unwrap()["content"].as_str().unwrap();
        assert!(continuation_prompt.contains("已有思考上下文"));

        let (status, node_id, finish_reason, continuation_of) =
            wait_for_turn_end(&mut events).await;
        assert!(matches!(status, TurnStatus::Ok));
        assert_eq!(finish_reason.as_deref(), Some("stop"));
        assert_eq!(continuation_of, Some(2));
        let node = handle.get_node(node_id.unwrap()).await.unwrap();
        assert_eq!(node.parent, Some(2));
        drop(input_tx);
    }

    #[tokio::test]
    async fn content_continuation_does_not_duplicate_reasoning_in_prompt() {
        let (url, body_rx) = spawn_capturing_stream_server().await;
        let mut session = new_http_test_session(url, true, Arc::new(ToolRegistry::new())).await;
        session.preload_history(
            vec![
                stored_message(Some(1), None, "user", "写一篇长文"),
                ConversationNodeSeed {
                    node_id: Some(2),
                    parent: Some(1),
                    turn_id: Some(1),
                    timestamp: Some("2026-07-31T00:00:01Z".to_string()),
                    message: Message::assistant(Some("已有正文"), Some("已有思考上下文"), None),
                },
            ],
            Some(2),
        );
        let (input_tx, input_rx) = mpsc::channel(1);
        let (mut events, handle) = session.try_run(input_rx).unwrap();

        handle.continue_generation(2).await.unwrap();
        assert_eq!(wait_for_content_delta(&mut events).await, "续写正文");
        let request: serde_json::Value = serde_json::from_str(&body_rx.await.unwrap()).unwrap();
        let messages = request["messages"].as_array().unwrap();
        assert!(messages.iter().any(|message| {
            message["role"] == "assistant" && message["reasoning_content"] == "已有思考上下文"
        }));
        assert!(
            !messages.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("已有思考上下文")
        );
        assert!(matches!(
            wait_for_turn_end(&mut events).await.0,
            TurnStatus::Ok
        ));
        drop(input_tx);
    }

    #[tokio::test]
    async fn stale_queued_continuation_does_not_stop_session() {
        let (url, _) = spawn_mock_server(vec![
            MockReply::StreamPartialThenHold {
                content: "第一段续写".to_string(),
                hold: Duration::from_millis(50),
            },
            MockReply::StreamDone {
                content: "会话仍可用".to_string(),
            },
        ])
        .await;
        let mut session = new_http_test_session(url, true, Arc::new(ToolRegistry::new())).await;
        session.preload_history(
            vec![
                stored_message(Some(1), None, "user", "写一篇长文"),
                stored_message(Some(2), Some(1), "assistant", "已有正文"),
            ],
            Some(2),
        );
        let (input_tx, input_rx) = mpsc::channel(1);
        let (mut events, handle) = session.try_run(input_rx).unwrap();

        handle.continue_generation(2).await.unwrap();
        handle.continue_generation(2).await.unwrap();
        assert_eq!(wait_for_content_delta(&mut events).await, "第一段续写");
        let (_, first_node_id, _, _) = wait_for_turn_end(&mut events).await;
        assert!(first_node_id.is_some());

        let (status, node_id, _, continuation_of) = wait_for_turn_end(&mut events).await;
        assert!(matches!(status, TurnStatus::Error(_)));
        assert_eq!(node_id, None);
        assert_eq!(continuation_of, Some(2));

        input_tx.send("继续对话".to_string()).await.unwrap();
        assert_eq!(wait_for_content_delta(&mut events).await, "会话仍可用");
        assert!(matches!(
            wait_for_turn_end(&mut events).await.0,
            TurnStatus::Ok
        ));
        drop(input_tx);
    }

    #[tokio::test]
    async fn recognized_bad_request_is_repaired_once() {
        let (url, request_count) = spawn_mock_server(vec![
            MockReply::HttpBadRequest {
                message: "reasoning_content is not allowed".to_string(),
            },
            MockReply::StreamDone {
                content: "修复成功".to_string(),
            },
        ])
        .await;
        let mut session = new_http_test_session(url, true, Arc::new(ToolRegistry::new())).await;
        session.preload_history(
            vec![
                stored_message(Some(1), None, "user", "旧问题"),
                ConversationNodeSeed {
                    node_id: Some(2),
                    parent: Some(1),
                    turn_id: Some(1),
                    timestamp: Some("2026-07-31T00:00:01Z".to_string()),
                    message: Message::assistant(Some("旧正文"), Some("旧思考"), None),
                },
            ],
            Some(2),
        );
        let (input_tx, input_rx) = mpsc::channel(1);
        let (mut events, _handle) = session.try_run(input_rx).unwrap();

        input_tx.send("新问题".to_string()).await.unwrap();
        assert_eq!(wait_for_content_delta(&mut events).await, "修复成功");
        let (status, _, _, _) = wait_for_turn_end(&mut events).await;
        assert!(matches!(status, TurnStatus::Ok));
        assert_eq!(request_count.load(Ordering::SeqCst), 2);
        drop(input_tx);
    }

    #[tokio::test]
    async fn unknown_bad_request_is_not_retried() {
        let (url, request_count) = spawn_mock_server(vec![MockReply::HttpBadRequest {
            message: "unknown parameter combination".to_string(),
        }])
        .await;
        let session = new_http_test_session(url, true, Arc::new(ToolRegistry::new())).await;
        let (input_tx, input_rx) = mpsc::channel(1);
        let (mut events, _handle) = session.try_run(input_rx).unwrap();

        input_tx.send("触发错误".to_string()).await.unwrap();
        loop {
            match events.next().await {
                Some(SessionEvent::Error(_)) => break,
                Some(_) => {}
                None => panic!("错误事件流提前结束"),
            }
        }
        assert_eq!(request_count.load(Ordering::SeqCst), 1);
        drop(input_tx);
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

        let (status, _, _, _) = wait_for_turn_end(&mut events).await;
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
        let (status, _, _, _) = wait_for_turn_end(&mut events).await;
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

        let (status, _, _, _) = wait_for_turn_end(&mut events).await;
        assert!(matches!(status, TurnStatus::Cancelled));
    }

    #[tokio::test]
    async fn set_task_context_keeps_latest_without_waiting_for_drive() {
        let session = new_test_session();
        let (input_tx, input_rx) = mpsc::channel(1);
        let (_events, handle) = session.try_run(input_rx).unwrap();

        for index in 0..64 {
            let mut ctx = TaskContext::default();
            ctx.attributes
                .insert("index".to_string(), index.to_string());
            tokio::time::timeout(Duration::from_millis(100), handle.set_task_context(ctx))
                .await
                .expect("上下文更新不应等待会话开始下一轮")
                .unwrap();
        }

        drop(input_tx);
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
        let (status, _, _, _) = wait_for_turn_end(&mut events).await;
        assert!(matches!(status, TurnStatus::Cancelled));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(request_count.load(Ordering::SeqCst), 1);
        assert!(!finished.load(Ordering::SeqCst));

        // 取消后不能留下悬空 tool_calls：每个 call 必须有配对的 tool 消息
        let snapshot = handle.get_conversation().await;
        let call_count: usize = snapshot
            .messages
            .iter()
            .filter(|m| m.role == "assistant")
            .filter_map(|m| m.tool_calls.as_ref())
            .map(Vec::len)
            .sum();
        let tool_count = snapshot
            .messages
            .iter()
            .filter(|m| m.role == "tool")
            .count();
        assert!(call_count > 0);
        assert_eq!(call_count, tool_count);
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

    fn tc(id: Option<&str>, index: usize, name: &str) -> ToolCall {
        ToolCall {
            id: id.map(str::to_string),
            call_type: Some("function".to_string()),
            function: ToolFunctionCall {
                name: name.to_string(),
                arguments: "{}".to_string(),
            },
            index,
        }
    }

    #[test]
    fn sanitize_fills_missing_tool_results() {
        let messages = vec![
            Message::user("q"),
            Message::assistant(
                None::<String>,
                None::<String>,
                Some(vec![tc(Some("a"), 0, "t1"), tc(Some("b"), 1, "t2")]),
            ),
            Message::tool("ok", "a"),
        ];
        let out = LLMSession::sanitize_tool_call_blocks(messages);
        assert_eq!(out.len(), 4);
        assert_eq!(out[3].role, "tool");
        assert_eq!(out[3].tool_call_id.as_deref(), Some("b"));
    }

    #[test]
    fn sanitize_rewrites_mismatched_ids_by_position() {
        // 旧版非流式路径：assistant 侧 provider ID、tool 侧合成 ID
        let messages = vec![
            Message::assistant(
                None::<String>,
                None::<String>,
                Some(vec![tc(Some("prov_1"), 0, "t1")]),
            ),
            Message::tool("ok", "t1:idx:0"),
        ];
        let out = LLMSession::sanitize_tool_call_blocks(messages);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].tool_call_id.as_deref(), Some("prov_1"));
    }

    #[test]
    fn sanitize_drops_orphan_tool_messages() {
        let messages = vec![
            Message::user("q"),
            Message::tool("orphan", "x"),
            Message::assistant(Some("a"), None::<String>, None),
        ];
        let out = LLMSession::sanitize_tool_call_blocks(messages);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|m| m.role != "tool"));
    }

    #[test]
    fn sanitize_drops_reasoning_only_assistant() {
        let messages = vec![
            Message::user("问题"),
            Message::assistant(None::<String>, Some("只有思考"), None),
        ];
        let out = LLMSession::sanitize_messages(messages);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "user");
    }

    #[test]
    fn sanitize_coalesces_consecutive_assistant_segments() {
        let messages = vec![
            Message::user("问题"),
            Message::assistant(Some("前半"), Some("思考一"), None),
            Message::assistant(Some("后半"), Some("思考二"), None),
        ];
        let out = LLMSession::sanitize_messages(messages);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].content.as_deref(), Some("前半后半"));
        assert_eq!(out[1].reasoning_content.as_deref(), Some("思考一思考二"));
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

    #[tokio::test]
    async fn preloaded_user_head_waits_for_explicit_input() {
        let (url, request_count) = spawn_mock_server(vec![MockReply::StreamDone {
            content: "回复".into(),
        }])
        .await;
        let mut session = new_http_test_session(url, true, Arc::new(ToolRegistry::new())).await;
        session.preload_history(
            vec![stored_message(Some(1), None, "user", "上次失败的问题")],
            Some(1),
        );
        let (input_tx, input_rx) = mpsc::channel(1);
        let (mut events, _handle) = session.try_run(input_rx).unwrap();

        let event = tokio::time::timeout(Duration::from_secs(1), events.next())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, SessionEvent::NeedInput));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(request_count.load(Ordering::SeqCst), 0);

        input_tx.send("新问题".to_string()).await.unwrap();
        wait_for_request_count(&request_count, 1).await;
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
