use crate::http_poster::HttpPoster;
use crate::llm::accumulator::ToolCallAccumulator;
use crate::llm::config::SessionConfig;
use crate::llm::handle::SessionHandle;
use crate::llm::stream_decoder::StreamDecoder;
use crate::llm::tree::ConversationTree;
use crate::llm::types::{
    ChatRequest, ChatResponse, CtrlMsg, DecoderEventPayload, Message, SessionEvent, ThinkingType,
    ToolCall, TurnStatus, Usage,
};
use crate::orchestrator::{AssembledTurn, Orchestrate, TaskContext};
use crate::plugin::pipeline::ApiPipeline;
use crate::storage::{StorageCtx, StoredMessage};
use crate::tool::registry::ToolRegistry;
use anyhow::{anyhow, Context, Result};
use futures_util::future::{self, Either};
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch, RwLock};
use tokio_stream::wrappers::ReceiverStream;

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

    /// 可选的持久化上下文（storage_path 不为 None 时由 client 注入）
    storage_ctx: Option<StorageCtx>,
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
            storage_ctx: None,
        })
    }

    /// 注入存储上下文（由 FlowCloudAIClient 在 create_llm_session 中调用）。
    pub fn set_storage_ctx(&mut self, plugin_id: String, store: std::sync::Arc<crate::storage::ConversationStore>) {
        self.storage_ctx = Some(StorageCtx::new(plugin_id, store));
    }

    /// 获取当前会话对应的持久化对话 ID。
    pub fn conversation_id(&self) -> Option<&str> {
        self.storage_ctx
            .as_ref()
            .map(|ctx| ctx.conversation_id.as_str())
    }

    /// 注入存储上下文，复用已有对话 ID（续聊时调用，保证写盘覆盖而非新建文件）。
    pub fn resume_storage_ctx(
        &mut self,
        conversation_id: String,
        plugin_id: String,
        created_at: String,
        store: std::sync::Arc<crate::storage::ConversationStore>,
    ) {
        self.storage_ctx = Some(StorageCtx::from_existing(
            conversation_id,
            plugin_id,
            store,
            created_at,
        ));
    }

    /// 将已有历史消息回放到内部 ConversationTree（必须在 run() 之前调用）。
    ///
    /// 由 `FlowCloudAIClient::resume_llm_session` 在创建 session 后、启动前注入，
    /// 使 tree 与磁盘上的对话历史保持一致。
    ///
    /// `head` 为 v3 持久化格式中的当前活跃节点；旧格式（v2）无此字段传 `None`，
    /// 此时退化为以最后一条消息为 head。
    pub fn preload_history(
        &mut self,
        messages: Vec<crate::storage::StoredMessage>,
        head: Option<u64>,
    ) {
        // 在 run() 调用前，只有 self 持有 Arc<RwLock<ConversationTree>>，
        // 因此 Arc::get_mut 保证成功，避免引入 async。
        if let Some(tree_lock) = Arc::get_mut(&mut self.tree) {
            let tree = tree_lock.get_mut();
            let mut prev_id: Option<u64> = None;
            let mut last_id: Option<u64> = None;
            for stored in messages {
                let msg = crate::llm::types::Message {
                    role: stored.role,
                    content: stored.content,
                    reasoning_content: stored.reasoning,
                    tool_call_id: stored.tool_call_id,
                    tool_calls: stored.tool_calls,
                };
                let parent = stored.parent.or(prev_id);
                let id = stored.node_id.unwrap_or(tree.next_id());
                tree.insert_node(
                    id,
                    parent,
                    msg,
                    stored.turn_id.unwrap_or(0),
                    stored.timestamp,
                );
                prev_id = Some(id);
                last_id = Some(id);
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

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to create session runtime");

            rt.block_on(async move {
                if let Err(e) = self
                    .drive(input_rx, ctrl_rx, Some(ctx_rx), cancel_rx, event_tx.clone())
                    .await
                {
                    let _ = event_tx.send(SessionEvent::Error(format!("{:#}", e))).await;
                }
            });
        });

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
    /// ```rust
    /// let (ctx_tx, ctx_rx) = mpsc::channel::<TaskContext>(16);
    /// let (events, handle) = session.run_with_context_channel(input_rx, ctx_rx);
    /// // 外部推送（等价于 handle.set_task_context，但可跨模块持有 tx）
    /// ctx_tx.send(my_ctx).await?;
    /// ```
    pub fn run_with_context_channel(
        self,
        input_rx: mpsc::Receiver<String>,
        mut ext_ctx_rx: mpsc::Receiver<TaskContext>,
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
            ctx_tx: ctx_tx.clone(),
        };

        // 将外部 ctx_rx 转发到内部 channel，与 handle.set_task_context 合并为同一路输入
        tokio::spawn(async move {
            while let Some(ctx) = ext_ctx_rx.recv().await {
                if ctx_tx.send(ctx).await.is_err() {
                    break;
                }
            }
        });

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to create session runtime");

            rt.block_on(async move {
                if let Err(e) = self
                    .drive(input_rx, ctrl_rx, Some(ctx_rx), cancel_rx, event_tx.clone())
                    .await
                {
                    let _ = event_tx.send(SessionEvent::Error(format!("{:#}", e))).await;
                }
            });
        });

        (ReceiverStream::new(event_rx), handle)
    }
}

// ── 核心状态机 ──

impl LLMSession {
    fn enabled_tool_names_from_request(req: &ChatRequest) -> HashSet<String> {
        req.tools
            .as_ref()
            .map(|tools| {
                tools.iter()
                    .filter_map(|tool| {
                        tool.get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn is_write_like_tool(name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        [
            "create", "update", "delete", "remove", "edit", "rename", "write",
            "save", "apply", "confirm", "install", "uninstall", "enable", "disable",
            "set_", "set-", "merge",
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
                let url = self.pipeline.get_url(&plugin_id)?.to_string();
                self.config.base_url = url;
                self.config.api_key = api_key;
                self.pipeline.set_plugin(Some(plugin_id));
            }
            CtrlMsg::Checkout { node_id } => {
                self.tree
                    .write()
                    .await
                    .checkout(node_id)
                    .map_err(|e| anyhow!(e))?;
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

        loop {
            if self.should_wait_for_user().await {
                tool_rounds = 0;
                event_tx.send(SessionEvent::NeedInput).await?;

                // 并发等待用户输入或控制指令
                // 收到 Checkout 后重新检查是否仍需等待，收到输入后追加消息并退出等待
                'wait: loop {
                    let input_fut = input_rx.recv();
                    let ctrl_fut = ctrl_rx.recv();
                    futures_util::pin_mut!(input_fut, ctrl_fut);

                    match future::select(input_fut, ctrl_fut).await {
                        Either::Left((Some(input), _)) => {
                            self.add_message(Message::user(input)).await;
                            break 'wait;
                        }
                        Either::Left((None, _)) => return Ok(()),
                        Either::Right((Some(ctrl), _)) => {
                            self.apply_ctrl(ctrl, &event_tx).await?;
                            // Checkout 可能使 head 移动到 user 节点，届时无需继续等待
                            if !self.should_wait_for_user().await {
                                break 'wait;
                            }
                        }
                        Either::Right((None, _)) => return Ok(()),
                    }
                }
            }

            // 每轮开始前尝试更新 context（非阻塞）
            if let Some(ref mut rx) = ctx_rx {
                while let Ok(ctx) = rx.try_recv() {
                    current_ctx = ctx;
                }
            }

            // 记录本轮开始时的 head 节点（用于 TurnBegin 事件）
            let turn_head_id = self.tree.read().await.head().unwrap_or(0);

            self.turn_id += 1;
            event_tx
                .send(SessionEvent::TurnBegin {
                    turn_id: self.turn_id,
                    node_id: turn_head_id,
                })
                .await?;

            let req = self.snapshot().await;

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

            let (content, reasoning, tool_calls, finish_reason, turn_status, usage) =
                self.send_and_process(&req, cancel_rx.clone(), &event_tx).await?;

            let asst_node_id = if matches!(turn_status, TurnStatus::Cancelled | TurnStatus::Interrupted) {
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
                        let status = TurnStatus::Error(format!(
                            "工具调用超过最大连续轮数限制: {}",
                            self.config.max_tool_rounds
                        ));
                        event_tx
                            .send(SessionEvent::TurnEnd {
                                status,
                                node_id: asst_node_id,
                                usage,
                            })
                            .await?;
                        continue;
                    }
                    self.execute_tool_calls(calls, &enabled_tools, read_only, &event_tx).await?;
                    continue;
                }
            }

            let is_ok = matches!(turn_status, TurnStatus::Ok);
            event_tx
                .send(SessionEvent::TurnEnd {
                    status: turn_status,
                    node_id: asst_node_id,
                    usage,
                })
                .await?;

            if is_ok {
                self.auto_save().await;
            }
        }
    }

    /// 将当前对话树写入磁盘（仅当 storage_ctx 已注入时生效）。
    async fn auto_save(&self) {
        let Some(ref ctx) = self.storage_ctx else {
            return;
        };

        let tree = self.tree.read().await;
        let head = tree.head();
        let messages: Vec<StoredMessage> = tree
            .all_nodes()
            .into_iter()
            .map(|node| StoredMessage {
                message_id: Some(format!("msg_{}", node.id)),
                node_id: Some(node.id),
                turn_id: Some(node.turn_id),
                parent: node.parent,
                role: node.message.role.clone(),
                content: node.message.content.clone(),
                reasoning: node.message.reasoning_content.clone(),
                timestamp: node.timestamp.clone(),
                tool_call_id: node.message.tool_call_id.clone(),
                tool_calls: node.message.tool_calls.clone(),
            })
            .collect();

        let model = self.conversation.read().await.model.clone();
        ctx.flush(messages, &model, head);
    }

    async fn should_wait_for_user(&self) -> bool {
        self.tree
            .read()
            .await
            .head_role()
            .map_or(true, |r| r == "assistant")
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
        cancel_rx: watch::Receiver<u64>,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<(
        String,
        String,
        Option<Vec<ToolCall>>,
        Option<String>,
        TurnStatus,
        Option<Usage>,
    )> {
        if req.stream.unwrap_or(false) {
            self.handle_stream(req, cancel_rx, event_tx).await
        } else {
            self.handle_non_stream(req, event_tx).await
        }
    }

    async fn handle_non_stream(
        &mut self,
        req: &ChatRequest,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<(
        String,
        String,
        Option<Vec<ToolCall>>,
        Option<String>,
        TurnStatus,
        Option<Usage>,
    )> {
        let json = self.prepare_request(req)?;

        let raw_line = {
            let stream = self
                .client
                .post_json(&self.config.base_url, &self.config.api_key, json)
                .await
                .context("创建请求失败")?;
            tokio::pin!(stream);
            stream
                .next()
                .await
                .ok_or_else(|| anyhow!("response empty"))??
        };

        let normalized = self.normalize_response(&raw_line)?;

        let res: ChatResponse = serde_json::from_str(&normalized)?;
        let choice = res
            .choices
            .first()
            .ok_or_else(|| anyhow!("empty choices"))?;

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
        mut cancel_rx: watch::Receiver<u64>,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<(
        String,
        String,
        Option<Vec<ToolCall>>,
        Option<String>,
        TurnStatus,
        Option<Usage>,
    )> {
        // StreamDecoder 和 ToolCallAccumulator 降为方法局部变量
        let mut decoder = StreamDecoder::default();
        decoder.begin_turn(self.turn_id);
        let mut acc = ToolCallAccumulator::default();

        let json = self.prepare_request(req)?;

        let stream = self
            .client
            .post_json(&self.config.base_url, &self.config.api_key, json)
            .await
            .context("创建流式请求失败")?;
        tokio::pin!(stream);

        let mut full_content = String::new();
        let mut full_reasoning = String::new();
        let mut finish_reason: Option<String> = None;
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut turn_status = TurnStatus::Ok;
        let mut usage: Option<Usage> = None;
        let cancel_version = *cancel_rx.borrow();

        'outer: loop {
            let raw_line = tokio::select! {
                changed = cancel_rx.changed() => {
                    match changed {
                        Ok(()) if *cancel_rx.borrow() != cancel_version => {
                            turn_status = TurnStatus::Cancelled;
                            finish_reason = Some("cancelled".to_string());
                            break 'outer;
                        }
                        Ok(()) => {
                            continue;
                        }
                        Err(_) => {
                            turn_status = TurnStatus::Cancelled;
                            finish_reason = Some("cancelled".to_string());
                            break 'outer;
                        }
                    }
                }
                raw_line = stream.next() => {
                    match raw_line {
                        Some(raw_line) => raw_line,
                        None => break 'outer,
                    }
                }
            };
            let line = raw_line?;
            if line.is_empty() {
                continue;
            }

            // acquire → map → release，每行独立借出，不跨 await
            let normalized = self
                .normalize_stream_line(&line)
                .unwrap_or_else(|_| line.clone());

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
                        usage = u;
                        finish_reason = Some(match &turn_status {
                            TurnStatus::Ok => "stop".to_string(),
                            TurnStatus::Cancelled => "cancelled".to_string(),
                            TurnStatus::Interrupted => "interrupted".to_string(),
                            TurnStatus::Error(e) => return Err(anyhow!(e.clone())),
                        });
                        break 'outer;
                    }

                    _ => {}
                }
            }
        }

        if finish_reason.is_none() {
            return Err(anyhow!("流式响应异常结束：缺少终止标记"));
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
}

// ── 工具执行 ──

impl LLMSession {
    async fn execute_tool_calls(
        &mut self,
        tool_calls: Vec<ToolCall>,
        enabled_tools: &HashSet<String>,
        read_only: bool,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<()> {
        for call in tool_calls {
            let func_name = &call.function.name;
            let args_str = call.function.arguments.trim();

            let (output, is_error) = if !enabled_tools.is_empty() && !enabled_tools.contains(func_name) {
                (format!("工具执行失败: 本轮不允许调用工具 '{}'", func_name), true)
            } else if read_only && Self::is_write_like_tool(func_name) {
                (format!("工具执行失败: 只读模式下禁止调用写入类工具 '{}'", func_name), true)
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
                            continue;
                        }
                    }
                };

                match self
                    .tool_registry
                    .conduct(func_name, Some(&args_v), Duration::from_secs(600))
                    .await
                {
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
        }

        Ok(())
    }

    #[inline]
    fn synth_tool_call_id(turn_id: u64, index: usize) -> String {
        format!("t{}:idx:{}", turn_id, index)
    }
}

#[cfg(test)]
mod tests {
    use super::LLMSession;
    use crate::llm::types::{Message, ToolCall, ToolFunctionCall};

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
}
