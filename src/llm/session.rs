use crate::error::{ClientError, ErrorCode};
use crate::http_poster::HttpPoster;
use crate::llm::accumulator::ToolCallAccumulator;
use crate::llm::config::SessionConfig;
use crate::llm::context_budget::{
    ContextTrimReport, EstimateSource, TrimOptions, calibrated_request_tokens, context_budget,
    context_budget_error_with_baseline, message_blocks, trim_request_for_window,
    trim_request_to_budget_with_baseline,
};
use crate::llm::handle::SessionHandle;
use crate::llm::stream_decoder::StreamDecoder;
use crate::llm::token_estimate::{RequestBaseline, TokenCalibrator, estimate_request_tokens};
use crate::llm::tree::{ConversationNodeSeed, ConversationTree};
use crate::llm::types::{
    ChatRequest, ChatResponse, CtrlMsg, DecoderEventPayload, Message, SessionEvent, ThinkingType,
    ToolCall, TurnStatus, Usage,
};
use crate::orchestrator::{AssembledTurn, Orchestrate, TaskContext};
use crate::plugin::pipeline::ApiPipeline;
use crate::plugin::types::ThinkingEffort;
use crate::tool::{ToolFailure, registry::ToolRegistry};
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

    /// 当前会话的 token 估算校准器。
    token_calibrator: TokenCalibrator,

    /// 上一次成功请求的真实 prompt token 基线。
    last_baseline: Option<RequestBaseline>,

    /// 插件注册中心（共享，只读，通过 acquire 借出 mapper）
    pipeline: ApiPipeline,

    /// 当前轮次 ID
    turn_id: u64,

    /// 从持久化历史恢复时先等待显式输入或 checkout，避免自动重放末尾未完成的用户消息。
    wait_for_input_on_start: bool,

    /// 当前供应商已明确拒绝 reasoning_content，后续请求直接移除该历史字段。
    strip_reasoning_content: bool,

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
        let token_calibrator = TokenCalibrator::new(config.calibration_factor);
        Ok(Self {
            client,
            conversation: Arc::new(RwLock::new(ChatRequest::default())),
            tree: Arc::new(RwLock::new(ConversationTree::new())),
            system_messages: Arc::new(Vec::new()),
            tool_registry,
            config,
            token_calibrator,
            last_baseline: None,
            pipeline,
            turn_id: 0,
            wait_for_input_on_start: false,
            strip_reasoning_content: false,
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
            let persisted_head_valid = head.is_some_and(|h| tree.get_node(h).is_some());
            if head.is_some() && !persisted_head_valid {
                log::warn!(
                    "[client:session][preload_history_invalid_head] head={:?} 已退回最后一条消息",
                    head
                );
            }
            let effective_head = head.filter(|_| persisted_head_valid).or(last_id);
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
        self.last_baseline = None;
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

    fn remove_tools_from_request(req: &mut ChatRequest, removed: &HashSet<String>) {
        let Some(tools) = req.tools.as_mut() else {
            return;
        };
        tools.retain(|tool| {
            tool.get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .is_none_or(|name| !removed.contains(name))
        });
        if tools.is_empty() {
            req.tools = None;
            req.tool_choice = None;
        }
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
        let Some(last_block) = message_blocks(messages).last().cloned() else {
            return 0;
        };
        if last_block.len() > 1
            && messages[last_block.start].role == "assistant"
            && messages[last_block.start]
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty())
        {
            return last_block.start;
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
                self.last_baseline = None;
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
        let mut fatal_tools = HashSet::new();
        let mut tool_round_soft_landing = false;
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
                fatal_tools.clear();
                tool_round_soft_landing = false;
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
                            calibration_factor: Some(self.token_calibrator.factor()),
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
            let (mut req, read_only) = if let Some(ref orch) = self.orchestrator {
                let assembled = orch.assemble(&current_ctx)?;
                let read_only = assembled.read_only;
                let req = self.apply_assembled(req, &assembled);
                (req, read_only)
            } else {
                (req, AssembledTurn::default().read_only)
            };
            let soft_landing_this_round = std::mem::take(&mut tool_round_soft_landing);
            if soft_landing_this_round {
                req.tool_choice = Some("none".to_string());
                req.messages.push(Message::system(
                    "已达到本轮工具调用上限。禁止继续调用工具；请仅使用已有结果给出简洁总结，明确说明尚未完成的步骤。",
                ));
            }
            Self::remove_tools_from_request(&mut req, &fatal_tools);
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

            if soft_landing_this_round {
                if let Some(calls) = tool_calls {
                    self.append_unexecuted_tool_placeholders(
                        MAX_ROUNDS_PLACEHOLDER_REASON,
                        calls,
                        &event_tx,
                    )
                    .await;
                }
                force_wait_for_user = true;
                event_tx
                    .send(SessionEvent::TurnEnd {
                        status: turn_status,
                        node_id: asst_node_id,
                        finish_reason: Some("tool_rounds_exhausted".to_string()),
                        continuation_of,
                        usage: accumulated_usage.take(),
                        calibration_factor: Some(self.token_calibrator.factor()),
                    })
                    .await?;
                continue;
            }

            if finish_reason.as_deref() == Some("tool_calls")
                && let Some(calls) = tool_calls
            {
                tool_rounds += 1;
                if tool_rounds > self.config.max_tool_rounds {
                    self.append_unexecuted_tool_placeholders(
                        MAX_ROUNDS_PLACEHOLDER_REASON,
                        calls,
                        &event_tx,
                    )
                    .await;
                    tool_round_soft_landing = true;
                    continue;
                }
                if self
                    .execute_tool_calls(
                        calls,
                        &enabled_tools,
                        read_only,
                        auto_confirm_writes,
                        &mut fatal_tools,
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
                            calibration_factor: Some(self.token_calibrator.factor()),
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
                    calibration_factor: Some(self.token_calibrator.factor()),
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
        if self.strip_reasoning_content {
            Self::strip_reasoning_content(&mut req.messages);
        }
        req.tools = self.tool_registry.schemas();
        req
    }

    /// 判断助手消息是否会被兼容接口拒绝并由请求清洗阶段移除。
    fn is_droppable_assistant(message: &Message) -> bool {
        message.role == "assistant"
            && message
                .content
                .as_deref()
                .is_none_or(|content| content.trim().is_empty())
            && message.tool_calls.as_ref().is_none_or(Vec::is_empty)
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
        // sanitize_messages 会移除 reasoning-only assistant；仅此时用临时用户上下文兜底。
        let prompt = if Self::is_droppable_assistant(&node.message)
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
            let invalid_assistant = Self::is_droppable_assistant(message);
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

    fn strip_reasoning_content(messages: &mut [Message]) -> bool {
        let mut changed = false;
        for message in messages {
            changed |= message.reasoning_content.take().is_some();
        }
        changed
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

        let calibration_factor = self.token_calibrator.factor();
        let (request_head, head_path) = {
            let tree = self.tree.read().await;
            (tree.head(), tree.path_to_head())
        };
        let baseline = self
            .last_baseline
            .as_ref()
            .filter(|baseline| baseline.extends_request(req, request_head, &head_path))
            .cloned();
        let estimate_source = if baseline.is_some() {
            EstimateSource::Baseline
        } else {
            EstimateSource::Full
        };
        log::info!(
            "[client:llm][token_estimate] turn_id={} estimate_source={} messages={}",
            self.turn_id,
            estimate_source.as_str(),
            req.messages.len()
        );
        let mut outgoing = req.clone();
        if let Some(context_window_tokens) = self.config.context_window_tokens
            && let Some(report) = trim_request_for_window(
                &mut outgoing,
                context_window_tokens,
                calibration_factor,
                baseline.as_ref(),
                TrimOptions::NORMAL,
            )?
        {
            Self::emit_context_trimmed(event_tx, &report).await?;
        }

        let base_estimate = estimate_request_tokens(&outgoing);
        let first_result = self.send_once(&outgoing, cancel, event_tx).await;
        let Err(ref error) = first_result else {
            self.observe_token_usage(&outgoing, request_head, base_estimate, &first_result);
            return first_result;
        };
        let Some(client_error) = ClientError::from_anyhow(error) else {
            return first_result;
        };
        let Some((mut repaired, rule)) = Self::repair_after_bad_request(&outgoing, client_error)
        else {
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
        if rule == "unsupported_reasoning_content" {
            self.strip_reasoning_content = true;
        }
        let retry_budget = if rule == "context_length_exceeded" {
            let budget = self.config.context_window_tokens.map_or_else(
                || {
                    calibrated_request_tokens(&outgoing, calibration_factor, baseline.as_ref())
                        .saturating_mul(70)
                        / 100
                },
                |window| {
                    context_budget(
                        &outgoing,
                        window,
                        TrimOptions::OVERFLOW_RETRY.budget_scale,
                        estimate_source,
                    )
                },
            );
            let report = trim_request_to_budget_with_baseline(
                &mut repaired,
                budget,
                calibration_factor,
                TrimOptions::OVERFLOW_RETRY.force_drop_oldest_round,
                baseline.as_ref(),
            )?
            .ok_or_else(|| {
                context_budget_error_with_baseline(
                    &repaired,
                    budget,
                    calibration_factor,
                    baseline.as_ref(),
                )
            })?;
            Self::emit_context_trimmed(event_tx, &report).await?;
            Some(budget)
        } else {
            None
        };
        let repaired_estimate = estimate_request_tokens(&repaired);
        let repaired_result = self.send_once(&repaired, cancel, event_tx).await;
        self.observe_token_usage(&repaired, request_head, repaired_estimate, &repaired_result);
        if let (Some(budget), Err(error)) = (retry_budget, &repaired_result)
            && ClientError::from_anyhow(error).is_some_and(Self::is_context_overflow_error)
        {
            return Err(context_budget_error_with_baseline(
                &repaired,
                budget,
                calibration_factor,
                baseline.as_ref(),
            )
            .into());
        }
        repaired_result
    }

    async fn emit_context_trimmed(
        event_tx: &mpsc::Sender<SessionEvent>,
        report: &ContextTrimReport,
    ) -> Result<()> {
        log::warn!(
            "[client:llm][context_trimmed] dropped_rounds={} truncated_messages={} before={} after={} budget={} estimate_source={}",
            report.dropped_rounds,
            report.truncated_messages,
            report.before,
            report.after,
            report.budget,
            report.estimate_source.as_str()
        );
        event_tx
            .send(SessionEvent::ContextTrimmed {
                dropped_rounds: report.dropped_rounds,
                truncated_messages: report.truncated_messages,
                before: report.before,
                after: report.after,
                suggest_compaction: report.suggest_compaction,
                estimate_source: report.estimate_source.as_str().to_string(),
            })
            .await?;
        Ok(())
    }

    fn observe_token_usage(
        &mut self,
        request: &ChatRequest,
        request_head: Option<u64>,
        base_estimate: u64,
        result: &Result<TurnOutput>,
    ) {
        let Ok((_, _, _, _, _, Some(usage))) = result else {
            return;
        };
        if let Some(factor) = self
            .token_calibrator
            .observe(usage.prompt_tokens, base_estimate)
        {
            log::info!(
                "[client:llm][token_calibrated] turn_id={} prompt_tokens={} base_estimate={} factor={:.4}",
                self.turn_id,
                usage.prompt_tokens,
                base_estimate,
                factor
            );
        }
        if let Some(head_node_id) = request_head
            && let Some(baseline) = RequestBaseline::new(usage.prompt_tokens, request, head_node_id)
        {
            self.last_baseline = Some(baseline);
        }
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

        if Self::is_context_overflow_message(&provider_message) {
            return Some((repaired, "context_length_exceeded"));
        }

        if provider_message.contains("reasoning_content")
            && ["not allowed", "unsupported", "unknown", "extra inputs"]
                .iter()
                .any(|marker| provider_message.contains(marker))
        {
            if Self::strip_reasoning_content(&mut repaired.messages) {
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

    fn is_context_overflow_error(error: &ClientError) -> bool {
        error.code == ErrorCode::HttpBadRequest
            && error
                .detail
                .get("provider_message")
                .and_then(Value::as_str)
                .is_some_and(Self::is_context_overflow_message)
    }

    fn is_context_overflow_message(message: &str) -> bool {
        let message = message.to_ascii_lowercase();
        [
            "context_length_exceeded",
            "maximum context length",
            "prompt is too long",
            "input length and max_tokens exceed context limit",
            "exceeds the context window",
            "too many tokens",
            "maximum number of tokens",
        ]
        .iter()
        .any(|marker| message.contains(marker))
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
const TOOL_RETRY_DELAYS_MS: [u64; 2] = [200, 800];
const MAX_TOOL_RETRY_DELAY_MS: u64 = 5_000;

fn tool_retry_delay_ms(retry_after_ms: Option<u64>, retry_count: usize) -> u64 {
    retry_after_ms
        .unwrap_or(TOOL_RETRY_DELAYS_MS[retry_count])
        .min(MAX_TOOL_RETRY_DELAY_MS)
}

impl LLMSession {
    async fn execute_tool_calls(
        &mut self,
        tool_calls: Vec<ToolCall>,
        enabled_tools: &Option<HashSet<String>>,
        read_only: bool,
        auto_confirm_writes: bool,
        fatal_tools: &mut HashSet<String>,
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
                let failure = ToolFailure::Denied {
                    reason: format!("本轮不允许调用工具 '{func_name}'"),
                };
                (failure.model_message(), true)
            } else if read_only && !self.tool_registry.is_read_tool(func_name) {
                log::warn!(
                    "[client:tools][call_blocked_read_only] turn_id={} index={} name={}",
                    self.turn_id,
                    call.index,
                    func_name
                );
                let failure = ToolFailure::Denied {
                    reason: format!(
                        "只读模式下仅允许显式标注为读的工具，'{func_name}' 未标注或为写工具"
                    ),
                };
                (failure.model_message(), true)
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

                let mut retry_count = 0;
                let result = loop {
                    log::info!(
                        "[client:tools][conduct_start] turn_id={} index={} name={} attempt={}",
                        self.turn_id,
                        call.index,
                        func_name,
                        retry_count + 1
                    );
                    let conduct_started = Instant::now();
                    let conduct_fut = crate::tool::with_auto_confirm_writes(
                        auto_confirm_writes,
                        self.tool_registry.conduct(
                            func_name,
                            Some(&args_v),
                            Duration::from_secs(600),
                        ),
                    );
                    let attempt = tokio::select! {
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
                    };
                    match attempt {
                        Ok(output) => {
                            log::info!(
                                "[client:tools][conduct_done] turn_id={} index={} name={} attempt={} elapsed_ms={} output_chars={}",
                                self.turn_id,
                                call.index,
                                func_name,
                                retry_count + 1,
                                conduct_started.elapsed().as_millis(),
                                output.chars().count()
                            );
                            break Ok(output);
                        }
                        Err(error) => {
                            let failure = ToolFailure::classify(&error);
                            log::warn!(
                                "[client:tools][conduct_failed] turn_id={} index={} name={} attempt={} elapsed_ms={} class={:?} error={}",
                                self.turn_id,
                                call.index,
                                func_name,
                                retry_count + 1,
                                conduct_started.elapsed().as_millis(),
                                failure,
                                error
                            );
                            if let ToolFailure::Transient { retry_after_ms } = &failure
                                && retry_count < TOOL_RETRY_DELAYS_MS.len()
                            {
                                let delay_ms = tool_retry_delay_ms(*retry_after_ms, retry_count);
                                retry_count += 1;
                                event_tx
                                    .send(SessionEvent::ToolRetrying {
                                        index: call.index,
                                        name: func_name.clone(),
                                        attempt: retry_count,
                                        max_retries: TOOL_RETRY_DELAYS_MS.len(),
                                        delay_ms,
                                    })
                                    .await?;
                                tokio::select! {
                                    _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {},
                                    _ = cancel.cancelled() => {
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
                                }
                                continue;
                            }
                            break Err(failure);
                        }
                    }
                };
                match result {
                    Ok(output) => (output, false),
                    Err(failure) => {
                        if failure == ToolFailure::Fatal {
                            fatal_tools.insert(func_name.clone());
                        }
                        (failure.model_message(), true)
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
mod tests;
