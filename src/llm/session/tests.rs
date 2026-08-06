//! `LLMSession` 的回归测试与本地 HTTP/SSE 测试服务器。
//!
//! 该子模块只承载测试代码，生产会话状态机仍由父模块实现。

use super::{LLMSession, TurnCancel, tool_retry_delay_ms};
use crate::error::{ClientError, ErrorCode};
use crate::llm::config::SessionConfig;
use crate::llm::token_estimate::RequestBaseline;
use crate::llm::tree::ConversationNodeSeed;
use crate::llm::types::{
    ChatRequest, Message, SessionEvent, ToolCall, ToolFunctionArg, ToolFunctionCall, TurnStatus,
    Usage,
};
use crate::orchestrator::TaskContext;
use crate::plugin::pipeline::ApiPipeline;
use crate::plugin::registry::PluginRegistry;
use crate::tool::ToolFailure;
use crate::tool::registry::ToolRegistry;
use futures_util::StreamExt;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

fn new_test_session() -> LLMSession {
    new_test_session_with_registry(ToolRegistry::new())
}

fn new_test_session_with_registry(tool_registry: ToolRegistry) -> LLMSession {
    let registry = Arc::new(PluginRegistry::empty().unwrap());
    let pipeline = ApiPipeline::try_new(registry, None).unwrap();
    let config = SessionConfig {
        base_url: "https://example.test".to_string(),
        api_key: "test-key".into(),
        ..SessionConfig::default()
    };
    LLMSession::new(config, pipeline, Arc::new(tool_registry)).unwrap()
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

fn overflow_request() -> ChatRequest {
    let mut messages = Vec::new();
    for round in 1..=4 {
        messages.push(Message::user(format!(
            "第{round}轮问题-{}",
            "问".repeat(100)
        )));
        messages.push(Message::assistant(
            Some(format!("第{round}轮回答-{}", "答".repeat(100))),
            None::<String>,
            None,
        ));
    }
    ChatRequest {
        messages,
        model: "mock-model".to_string(),
        stream: Some(true),
        ..ChatRequest::default()
    }
}

fn test_tool_call(name: &str) -> ToolCall {
    ToolCall {
        id: Some("call_1".to_string()),
        call_type: Some("function".to_string()),
        function: ToolFunctionCall {
            name: name.to_string(),
            arguments: "{}".to_string(),
        },
        index: 0,
    }
}

#[derive(Clone)]
struct CountingFailureState {
    calls: Arc<AtomicUsize>,
    failure: ToolFailure,
}

fn registry_with_failure(
    name: &str,
    calls: Arc<AtomicUsize>,
    failure: ToolFailure,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.put_state::<crate::sense::SenseState<CountingFailureState>>(Arc::new(
        tokio::sync::Mutex::new(CountingFailureState { calls, failure }),
    ));
    registry.register_async::<CountingFailureState, _>(
        name,
        "失败分类测试工具",
        None::<Vec<ToolFunctionArg>>,
        |state, _args| {
            Box::pin(async move {
                state.calls.fetch_add(1, Ordering::SeqCst);
                Err(state.failure.clone().into())
            })
        },
    );
    registry
}

enum MockReply {
    DelayHeaders(Duration),
    HttpBadRequest {
        message: String,
    },
    ContextLimit {
        message_counts: Arc<std::sync::Mutex<Vec<usize>>>,
        fail: bool,
    },
    StreamPartialThenHold {
        content: String,
        hold: Duration,
    },
    StreamPartialThenDisconnect {
        content: String,
    },
    StreamLength {
        content: String,
    },
    StreamDone {
        content: String,
    },
    StreamDoneRecording {
        content: String,
        requests: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    },
    StreamToolCall {
        name: String,
    },
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
    let request = read_http_request(&mut socket).await.unwrap_or_default();
    match reply {
        MockReply::DelayHeaders(delay) => {
            tokio::time::sleep(delay).await;
        }
        MockReply::HttpBadRequest { message } => {
            write_bad_request(&mut socket, &message).await;
        }
        MockReply::ContextLimit {
            message_counts,
            fail,
        } => {
            message_counts
                .lock()
                .unwrap()
                .push(request_message_count(&request));
            if fail {
                write_bad_request(
                    &mut socket,
                    "maximum context length exceeded: prompt is too long",
                )
                .await;
            } else {
                let _ = write_stream_headers(&mut socket).await;
                let _ = write_sse_line(&mut socket, stream_content_chunk("恢复成功", None)).await;
                let _ = write_sse_line(&mut socket, "[DONE]".to_string()).await;
                let _ = socket.flush().await;
            }
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
        MockReply::StreamDoneRecording { content, requests } => {
            if let Some(request) = request_json(&request) {
                requests.lock().unwrap().push(request);
            }
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

async fn read_http_request(socket: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buf = [0u8; 1024];
    let mut data = Vec::new();
    loop {
        let n = socket.read(&mut buf).await?;
        if n == 0 {
            return Ok(data);
        }
        data.extend_from_slice(&buf[..n]);
        let Some(header_end) = data.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
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
        if data.len() >= header_end + 4 + content_length {
            return Ok(data);
        }
    }
}

fn request_message_count(request: &[u8]) -> usize {
    request_json(request)
        .and_then(|body| body.get("messages")?.as_array().map(Vec::len))
        .unwrap_or(0)
}

fn request_json(request: &[u8]) -> Option<serde_json::Value> {
    let header_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")?;
    serde_json::from_slice(&request[header_end + 4..]).ok()
}

async fn write_bad_request(socket: &mut TcpStream, message: &str) {
    let body = serde_json::json!({"error": {"message": message}}).to_string();
    let response = format!(
        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.flush().await;
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
        let _ = write_sse_line(&mut socket, stream_content_chunk("续写正文", Some("stop"))).await;
        let _ = write_sse_line(&mut socket, "[DONE]".to_string()).await;
        let _ = socket.flush().await;
    });
    (format!("http://{}", addr), body_rx)
}

async fn spawn_reasoning_rejecting_server(expected_requests: usize) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let request_count = Arc::new(AtomicUsize::new(0));
    let count_for_task = Arc::clone(&request_count);
    tokio::spawn(async move {
        for _ in 0..expected_requests {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let body = read_request_body(&mut socket).await.unwrap_or_default();
            count_for_task.fetch_add(1, Ordering::SeqCst);
            if body.contains("\"reasoning_content\"") {
                let body = serde_json::json!({
                    "error": {"message": "reasoning_content is not allowed"}
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            } else {
                let _ = write_stream_headers(&mut socket).await;
                let _ = write_sse_line(&mut socket, stream_content_chunk("修复成功", Some("stop")))
                    .await;
                let _ = write_sse_line(&mut socket, "[DONE]".to_string()).await;
            }
            let _ = socket.flush().await;
        }
    });
    (format!("http://{}", addr), request_count)
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
    let (url, request_count) = spawn_mock_server(vec![MockReply::StreamPartialThenDisconnect {
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

    let (status, node_id, finish_reason, continuation_of) = wait_for_turn_end(&mut events).await;
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
    let (url, request_count) = spawn_reasoning_rejecting_server(3).await;
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

    input_tx.send("再问一次".to_string()).await.unwrap();
    assert_eq!(wait_for_content_delta(&mut events).await, "修复成功");
    assert!(matches!(
        wait_for_turn_end(&mut events).await.0,
        TurnStatus::Ok
    ));
    assert_eq!(request_count.load(Ordering::SeqCst), 3);
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

#[test]
fn recognizes_common_context_overflow_formats() {
    let request = overflow_request();
    for provider_message in [
        "context_length_exceeded",
        "This model's maximum context length is 8192 tokens",
        "Prompt is too long",
        "input length and max_tokens exceed context limit",
        "request exceeds the context window",
        "too many tokens in the request",
        "maximum number of tokens exceeded",
    ] {
        let error = ClientError::new(ErrorCode::HttpBadRequest, "请求参数错误")
            .with_kv("provider_message", provider_message);
        let (_, rule) = LLMSession::repair_after_bad_request(&request, &error)
            .expect("常见上下文溢出格式应被识别");
        assert_eq!(rule, "context_length_exceeded");
    }
}

#[tokio::test]
async fn context_overflow_retries_once_with_fewer_messages() {
    let message_counts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (url, request_count) = spawn_mock_server(vec![
        MockReply::ContextLimit {
            message_counts: Arc::clone(&message_counts),
            fail: true,
        },
        MockReply::ContextLimit {
            message_counts: Arc::clone(&message_counts),
            fail: false,
        },
    ])
    .await;
    let mut session = new_http_test_session(url, true, Arc::new(ToolRegistry::new())).await;
    session.config.context_window_tokens = Some(8192);
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(0_u64);
    let mut cancel = TurnCancel::new(&cancel_rx);
    let (event_tx, mut event_rx) = mpsc::channel(32);

    let output = session
        .send_and_process(&overflow_request(), &mut cancel, &event_tx)
        .await
        .unwrap();
    assert_eq!(output.0, "恢复成功");
    assert_eq!(request_count.load(Ordering::SeqCst), 2);
    let counts = message_counts.lock().unwrap().clone();
    assert_eq!(counts.len(), 2);
    assert!(counts[1] < counts[0]);
    assert!(
        std::iter::from_fn(|| event_rx.try_recv().ok())
            .any(|event| matches!(event, SessionEvent::ContextTrimmed { .. }))
    );
}

#[tokio::test]
async fn repeated_context_overflow_returns_chinese_budget_error() {
    let message_counts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (url, request_count) = spawn_mock_server(vec![
        MockReply::ContextLimit {
            message_counts: Arc::clone(&message_counts),
            fail: true,
        },
        MockReply::ContextLimit {
            message_counts,
            fail: true,
        },
    ])
    .await;
    let mut session = new_http_test_session(url, true, Arc::new(ToolRegistry::new())).await;
    session.config.context_window_tokens = Some(8192);
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(0_u64);
    let mut cancel = TurnCancel::new(&cancel_rx);
    let (event_tx, _event_rx) = mpsc::channel(32);

    let error = session
        .send_and_process(&overflow_request(), &mut cancel, &event_tx)
        .await
        .unwrap_err();
    let client_error = ClientError::from_anyhow(&error).unwrap();
    assert_eq!(client_error.code, ErrorCode::ContextBudgetExceeded);
    assert!(client_error.message.contains("上下文"));
    assert!(!client_error.message.contains("maximum context length"));
    assert_eq!(request_count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn transient_tool_failure_retries_exactly_twice() {
    let calls = Arc::new(AtomicUsize::new(0));
    let registry = registry_with_failure(
        "transient_tool",
        Arc::clone(&calls),
        ToolFailure::Transient {
            retry_after_ms: Some(0),
        },
    );
    let mut session = new_test_session_with_registry(registry);
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(0_u64);
    let mut cancel = TurnCancel::new(&cancel_rx);
    let (event_tx, mut event_rx) = mpsc::channel(16);
    let mut fatal_tools = HashSet::new();

    assert!(
        !session
            .execute_tool_calls(
                vec![test_tool_call("transient_tool")],
                &None,
                false,
                false,
                &mut fatal_tools,
                &mut cancel,
                &event_tx,
            )
            .await
            .unwrap()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, SessionEvent::ToolRetrying { .. }))
            .count(),
        2
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SessionEvent::Error(_)))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::ToolResult {
            output,
            is_error: true,
            ..
        } if output.contains("超时")
    )));
}

#[test]
fn 外部_retry_after_不会超过上限() {
    assert_eq!(tool_retry_delay_ms(Some(3_600_000), 0), 5_000);
    assert_eq!(tool_retry_delay_ms(None, 1), 800);
}

#[tokio::test]
async fn denied_tool_failure_never_retries_and_forbids_model_retry() {
    let calls = Arc::new(AtomicUsize::new(0));
    let registry = registry_with_failure(
        "denied_tool",
        Arc::clone(&calls),
        ToolFailure::Denied {
            reason: "用户取消确认".to_string(),
        },
    );
    let mut session = new_test_session_with_registry(registry);
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(0_u64);
    let mut cancel = TurnCancel::new(&cancel_rx);
    let (event_tx, mut event_rx) = mpsc::channel(16);
    let mut fatal_tools = HashSet::new();

    session
        .execute_tool_calls(
            vec![test_tool_call("denied_tool")],
            &None,
            false,
            false,
            &mut fatal_tools,
            &mut cancel,
            &event_tx,
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SessionEvent::ToolRetrying { .. }))
    );
    let output = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .unwrap();
    assert!(output.contains("不要重试"));
}

#[tokio::test]
async fn fatal_tool_is_removed_from_followup_request() {
    let calls = Arc::new(AtomicUsize::new(0));
    let registry = registry_with_failure("fatal_tool", Arc::clone(&calls), ToolFailure::Fatal);
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (url, request_count) = spawn_mock_server(vec![
        MockReply::StreamToolCall {
            name: "fatal_tool".to_string(),
        },
        MockReply::StreamDoneRecording {
            content: "已使用现有信息收尾".to_string(),
            requests: Arc::clone(&requests),
        },
    ])
    .await;
    let session = new_http_test_session(url, true, Arc::new(registry)).await;
    let (input_tx, input_rx) = mpsc::channel(1);
    let (mut events, _handle) = session.try_run(input_rx).unwrap();

    input_tx.send("调用故障工具".to_string()).await.unwrap();
    let (status, _, _, _) = wait_for_turn_end(&mut events).await;
    assert!(matches!(status, TurnStatus::Ok));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(request_count.load(Ordering::SeqCst), 2);
    let requests = requests.lock().unwrap();
    let tools = requests[0]["tools"].as_array();
    assert!(tools.is_none_or(|tools| {
        tools
            .iter()
            .all(|tool| tool["function"]["name"].as_str() != Some("fatal_tool"))
    }));
    drop(input_tx);
}

#[tokio::test]
async fn tool_round_limit_finishes_with_text_instead_of_error() {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (url, request_count) = spawn_mock_server(vec![
        MockReply::StreamToolCall {
            name: "unused_tool".to_string(),
        },
        MockReply::StreamDoneRecording {
            content: "已根据现有结果总结".to_string(),
            requests: Arc::clone(&requests),
        },
    ])
    .await;
    let mut session = new_http_test_session(url, true, Arc::new(ToolRegistry::new())).await;
    session.config.max_tool_rounds = 0;
    let (input_tx, input_rx) = mpsc::channel(1);
    let (mut events, _handle) = session.try_run(input_rx).unwrap();

    input_tx.send("执行复杂任务".to_string()).await.unwrap();
    let (status, _, finish_reason, _) = wait_for_turn_end(&mut events).await;
    assert!(matches!(status, TurnStatus::Ok));
    assert_eq!(finish_reason.as_deref(), Some("tool_rounds_exhausted"));
    assert_eq!(request_count.load(Ordering::SeqCst), 2);
    let requests = requests.lock().unwrap();
    assert_eq!(requests[0]["tool_choice"], "none");
    assert!(
        requests[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["role"] == "system"
                    && message["content"]
                        .as_str()
                        .is_some_and(|content| content.contains("工具调用上限"))
            })
    );
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
fn 无效持久化_head_退回最后一条消息() {
    let mut session = new_test_session();
    session.preload_history(
        vec![
            stored_message(Some(1), None, "user", "问题"),
            stored_message(Some(2), Some(1), "assistant", "回复"),
        ],
        Some(999),
    );

    let tree = session.tree.blocking_read();
    assert_eq!(tree.head(), Some(2));
    assert_eq!(tree.linearize().len(), 2);
}

#[test]
fn 有效持久化_head_被采用() {
    let mut session = new_test_session();
    session.preload_history(
        vec![
            stored_message(Some(1), None, "user", "问题"),
            stored_message(Some(2), Some(1), "assistant", "回复"),
        ],
        Some(1),
    );

    let tree = session.tree.blocking_read();
    assert_eq!(tree.head(), Some(1));
    assert_eq!(tree.linearize().len(), 1);
}

#[test]
fn 无_head_时退回最后一条消息() {
    let mut session = new_test_session();
    session.preload_history(
        vec![
            stored_message(Some(1), None, "user", "问题"),
            stored_message(Some(2), Some(1), "assistant", "回复"),
        ],
        None,
    );

    let tree = session.tree.blocking_read();
    assert_eq!(tree.head(), Some(2));
    assert_eq!(tree.linearize().len(), 2);
}

#[tokio::test]
async fn 树快照的_head_必在节点集合内() {
    let mut session = new_test_session();
    session.preload_history(
        vec![
            stored_message(Some(1), None, "user", "问题"),
            stored_message(Some(2), Some(1), "assistant", "回复"),
        ],
        Some(2),
    );
    let (input_tx, input_rx) = mpsc::channel(1);
    let (_events, handle) = session.try_run(input_rx).unwrap();

    let (nodes, head) = handle.tree_snapshot().await;
    assert!(head.is_none_or(|head| nodes.iter().any(|node| node.id == head)));
    drop(input_tx);
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

#[tokio::test]
async fn 切换模型会清空真实_token_基线() {
    let mut session = new_test_session();
    let request = ChatRequest {
        messages: vec![Message::user("问题")],
        model: "old-model".to_string(),
        ..ChatRequest::default()
    };
    session.last_baseline = RequestBaseline::new(100, &request, 1);

    session.set_model("new-model").await;
    assert!(session.last_baseline.is_none());
}
