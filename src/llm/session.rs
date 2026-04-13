use crate::http_poster::HttpPoster;
use crate::llm::accumulator::ToolCallAccumulator;
use crate::llm::config::SessionConfig;
use crate::llm::handle::SessionHandle;
use crate::llm::stream_decoder::StreamDecoder;
use crate::llm::types::{
    ChatRequest, ChatResponse, CtrlMsg, DecoderEventPayload, Message, SessionEvent, ThinkingType,
    ToolCall, TurnStatus,
};
use crate::orchestrator::{AssembledTurn, TaskContext, TaskOrchestrator};
use crate::plugin::pipeline::ApiPipeline;
use crate::tool::registry::ToolRegistry;
use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
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

    /// 对话请求与历史
    conversation: Arc<RwLock<ChatRequest>>,

    /// 工具函数管理器
    tool_registry: Arc<ToolRegistry>,

    /// 连接配置
    config: SessionConfig,

    /// 插件注册中心（共享，只读，通过 acquire 借出 mapper）
    pipeline: ApiPipeline,

    /// 当前轮次 ID
    turn_id: u64,

    orchestrator: Option<TaskOrchestrator>,
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
            tool_registry,
            config,
            pipeline,
            turn_id: 0,
            orchestrator: None,
        })
    }

    pub fn set_api(&mut self, api_key: &str) {
        self.config.api_key = api_key.to_string();
    }

    pub fn set_url(&mut self, url: &str) {
        self.config.base_url = url.to_string();
    }

    pub async fn load_sense(&mut self, sense: impl crate::sense::Sense) -> Result<&mut Self> {
        {
            let mut conv = self.conversation.write().await;
            if let Some(request) = sense.default_request() {
                *conv = request;
            }
            for prompt in sense.prompts() {
                conv.messages.push(Message::system(prompt));
            }
        }
        self.conversation.write().await.tools = self.tool_registry.schemas();
        let schemas = self.tool_registry.schemas();
        println!(
            "[debug] tool schemas count: {:?}",
            schemas.as_ref().map(|v| v.len())
        );
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
}

// ── 启动 ──

impl LLMSession {
    pub fn run(
        self,
        input_rx: mpsc::Receiver<String>,
        ctx_rx: Option<mpsc::Receiver<TaskContext>>,
    ) -> (ReceiverStream<SessionEvent>, SessionHandle) {
        let (event_tx, event_rx) = mpsc::channel::<SessionEvent>(self.config.event_buffer);
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<CtrlMsg>(8);

        let handle = SessionHandle {
            inner: Arc::clone(&self.conversation),
            ctrl_tx,
        };

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to create session runtime");

            rt.block_on(async move {
                if let Err(e) = self.drive(input_rx, ctrl_rx, ctx_rx, event_tx.clone()).await {
                    let _ = event_tx.send(SessionEvent::Error(format!("{:#}", e))).await;
                }
            });
        });

        (ReceiverStream::new(event_rx), handle)
    }
}

// ── 核心状态机 ──

impl LLMSession {
    async fn apply_assembled(&self, base: &ChatRequest, turn: &AssembledTurn) -> ChatRequest {
        let mut req = base.clone();

        // 注入上下文 messages（在已有消息之前、用户最新消息之前）
        for msg in &turn.context_messages {
            req.messages.insert(req.messages.len().saturating_sub(1), Message::system(msg.clone()));
        }

        // 覆盖工具 schemas
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

    async fn apply_ctrl(&mut self, msg: CtrlMsg) -> Result<()> {
        match msg {
            CtrlMsg::SwitchPlugin { plugin_id, api_key } => {
                let url = self.pipeline.get_url(&plugin_id)?.to_string();
                self.config.base_url = url;
                self.config.api_key = api_key;
                self.pipeline.set_plugin(Some(plugin_id));
            }
        }
        Ok(())
    }

    async fn drive(
        mut self,
        mut input_rx: mpsc::Receiver<String>,
        mut ctrl_rx: mpsc::Receiver<CtrlMsg>,
        mut ctx_rx: Option<mpsc::Receiver<TaskContext>>,
        event_tx: mpsc::Sender<SessionEvent>,
    ) -> Result<()> {
        let mut current_ctx = TaskContext::default();

        loop {
            if self.should_wait_for_user().await {
                event_tx.send(SessionEvent::NeedInput).await?;
                // 在等待用户输入前，先应用所有待处理的控制指令
                while let Ok(ctrl) = ctrl_rx.try_recv() {
                    self.apply_ctrl(ctrl).await?;
                }
                match input_rx.recv().await {
                    Some(input) => self.add_message(Message::user(input)).await,
                    None => return Ok(()),
                }
            }

            // 每轮开始前尝试更新 context（非阻塞）
            if let Some(ref mut rx) = ctx_rx {
                while let Ok(ctx) = rx.try_recv() {
                    current_ctx = ctx;
                }
            }

            self.turn_id += 1;
            event_tx
                .send(SessionEvent::TurnBegin {
                    turn_id: self.turn_id,
                })
                .await?;

            let req = self.snapshot().await;

            // Orchestrator 装配（如果有）
            let req = if let Some(ref orch) = self.orchestrator {
                let assembled = orch.assemble(&current_ctx)?;
                self.apply_assembled(&req, &assembled).await
            } else {
                req
            };

            let (content, reasoning, tool_calls, finish_reason, turn_status) =
                self.send_and_process(&req, &event_tx).await?;

            self.add_message(Message::assistant(
                Some(content).filter(|s| !s.is_empty()),
                Some(reasoning).filter(|s| !s.is_empty()),
                tool_calls.clone(),
            ))
            .await;

            if finish_reason.as_deref() == Some("tool_calls") {
                if let Some(calls) = tool_calls {
                    self.execute_tool_calls(calls, &event_tx).await?;
                    continue;
                }
            }

            event_tx
                .send(SessionEvent::TurnEnd {
                    status: turn_status,
                })
                .await?;
        }
    }

    async fn should_wait_for_user(&self) -> bool {
        self.conversation
            .read()
            .await
            .messages
            .last()
            .map_or(true, |msg| msg.role == "assistant")
    }

    async fn snapshot(&self) -> ChatRequest {
        self.conversation.read().await.clone()
    }

    async fn add_message(&self, msg: Message) {
        self.conversation.write().await.messages.push(msg);
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
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<(
        String,
        String,
        Option<Vec<ToolCall>>,
        Option<String>,
        TurnStatus,
    )> {
        if req.stream.unwrap_or(false) {
            self.handle_stream(req, event_tx).await
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
        ))
    }

    async fn handle_stream(
        &mut self,
        req: &ChatRequest,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<(
        String,
        String,
        Option<Vec<ToolCall>>,
        Option<String>,
        TurnStatus,
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

        'outer: while let Some(raw_line) = stream.next().await {
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
                        finish_reason = Some("tool_calls".to_string());
                        break 'outer;
                    }

                    DecoderEventPayload::TurnEnd { status, .. } => {
                        turn_status = status.clone();
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
        ))
    }
}

// ── 工具执行 ──

impl LLMSession {
    async fn execute_tool_calls(
        &mut self,
        tool_calls: Vec<ToolCall>,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<()> {
        for call in tool_calls {
            let func_name = &call.function.name;
            let args_str = call.function.arguments.trim();

            let args_v: Value = if args_str.is_empty() {
                Value::Object(Default::default())
            } else {
                serde_json::from_str(args_str)?
            };

            let (output, is_error) =
                match self.tool_registry.conduct(func_name, Some(&args_v), Duration::from_secs(60)).await {
                    Ok(o) => (o, false),
                    Err(e) => (format!("工具执行失败: {}", e), true),
                };

            let tool_call_id = Self::synth_tool_call_id(self.turn_id, call.index);

            event_tx
                .send(SessionEvent::ToolResult {
                    index: call.index,
                    output: output.clone(),
                    is_error,
                })
                .await?;

            self.add_message(Message::tool(output, tool_call_id)).await;
        }

        Ok(())
    }

    #[inline]
    fn synth_tool_call_id(turn_id: u64, index: usize) -> String {
        format!("t{}:idx:{}", turn_id, index)
    }
}
