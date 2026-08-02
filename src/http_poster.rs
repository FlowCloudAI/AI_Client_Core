use anyhow::Result;
use futures_util::{StreamExt, TryStreamExt};
use reqwest::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, RETRY_AFTER};
use serde_json::Value;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::io::StreamReader;

use crate::error::{ClientError, ErrorCode};

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_ERROR_PREVIEW_CHARS: usize = 2_000;

fn map_status(status: reqwest::StatusCode) -> ErrorCode {
    match status.as_u16() {
        400 | 422 => ErrorCode::HttpBadRequest,
        401 => ErrorCode::HttpUnauthorized,
        402 => ErrorCode::HttpPaymentRequired,
        403 => ErrorCode::HttpForbidden,
        404 => ErrorCode::HttpNotFound,
        408 => ErrorCode::HttpTimeout,
        429 => ErrorCode::HttpTooManyRequests,
        500..=599 => ErrorCode::HttpServerError,
        _ => ErrorCode::LlmResponseBadStatus,
    }
}

fn status_message(status: reqwest::StatusCode) -> &'static str {
    match status.as_u16() {
        400 | 422 => "请求参数不符合 AI 服务要求",
        401 => "认证失败，请检查 API Key 是否正确、有效，并确认接口地址匹配",
        402 => "AI 服务账户余额或付费状态异常",
        403 => "当前凭据无权访问该服务，请检查模型权限、账号状态或服务区域",
        404 => "请求的接口、模型或资源不存在",
        408 => "AI 服务请求超时",
        429 => "请求受限，可能是频率、并发或配额已达上限",
        500..=599 => "AI 服务暂时不可用",
        _ => "AI 服务返回异常状态",
    }
}

fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn nested_text(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    value_text(current)
}

fn first_nested_text(value: &Value, paths: &[&[&str]]) -> Option<String> {
    paths.iter().find_map(|path| nested_text(value, path))
}

fn header_text(headers: &HeaderMap, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn body_preview(body: &str) -> String {
    let mut chars = body.chars();
    let preview: String = chars.by_ref().take(MAX_ERROR_PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        format!("{preview}...(truncated)")
    } else {
        preview
    }
}

fn build_http_error(
    status: reqwest::StatusCode,
    headers: &HeaderMap,
    body: &str,
    body_truncated: bool,
    url: &str,
) -> ClientError {
    let parsed = serde_json::from_str::<Value>(body).ok();
    let provider_code = parsed.as_ref().and_then(|value| {
        first_nested_text(
            value,
            &[
                &["error", "code"],
                &["code"],
                &["base_resp", "status_code"],
                &["status", "code"],
            ],
        )
    });
    let provider_message = parsed.as_ref().and_then(|value| {
        first_nested_text(
            value,
            &[
                &["error", "message"],
                &["message"],
                &["msg"],
                &["base_resp", "status_msg"],
                &["status", "message"],
            ],
        )
    });
    let request_id = parsed
        .as_ref()
        .and_then(|value| {
            first_nested_text(
                value,
                &[&["request_id"], &["requestId"], &["error", "request_id"]],
            )
        })
        .or_else(|| {
            header_text(
                headers,
                &[
                    "x-request-id",
                    "request-id",
                    "x-dashscope-request-id",
                    "x-tt-logid",
                ],
            )
        });

    let mut error = ClientError::new(map_status(status), status_message(status))
        .with_kv("phase", "http_response")
        .with_kv("url", url.to_string())
        .with_kv("status_code", status.as_u16())
        .with_kv(
            "retryable",
            matches!(status.as_u16(), 408 | 429 | 500..=599),
        );
    if let Some(code) = provider_code {
        error = error.with_kv("provider_code", code);
    }
    if let Some(message) = provider_message {
        error = error.with_kv("provider_message", message);
    }
    if let Some(request_id) = request_id {
        error = error.with_kv("request_id", request_id);
    }
    if let Some(retry_after_ms) = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|seconds| seconds.saturating_mul(1_000))
    {
        error = error.with_kv("retry_after_ms", retry_after_ms);
    }
    if body_truncated {
        error = error.with_kv("body_truncated", true);
    }
    if parsed.is_none() && !body.trim().is_empty() {
        error = error.with_kv("body_preview", body_preview(body));
    }
    error
}

fn classify_reqwest_error(e: &reqwest::Error) -> ErrorCode {
    if e.is_timeout() {
        ErrorCode::HttpTimeout
    } else if e.is_connect() {
        ErrorCode::LlmRequestNetworkError
    } else if let Some(status) = e.status() {
        map_status(status)
    } else {
        ErrorCode::LlmRequestNetworkError
    }
}

/// 流式 Client：禁用自动解压。
///
/// SSE 流式传输必须禁用，防止 Qwen 等会返回 gzip 压缩响应的 API 出现
/// "error decoding response body"（reqwest 的 gzip 是 Client 级配置，
/// 无法 per-request 切换，故与收集路径拆成两个共享 Client）。
static STREAM_CLIENT: OnceLock<Client> = OnceLock::new();
/// 收集 Client：启用自动解压，非流式响应（hex 音频 / base64 图像 / 非流式 LLM）
/// 走这里以复用连接并享受 gzip 压缩。
static COLLECT_CLIENT: OnceLock<Client> = OnceLock::new();

fn build_client(no_compression: bool) -> Result<Client> {
    let mut builder = Client::builder().connect_timeout(Duration::from_secs(10));
    if no_compression {
        builder = builder.no_gzip().no_brotli().no_deflate();
    }
    builder.build().map_err(|e| {
        ClientError::new(ErrorCode::CoreClientInitFailed, "构建 HTTP 客户端失败")
            .with_kv("source", e.to_string())
            .into()
    })
}

/// 获取共享 Client；`get_or_init` 不接受 Result，故 miss 时自建后 set
/// （竞态输家的实例被 `set` 丢弃，无害）。
fn shared_client(cell: &'static OnceLock<Client>, no_compression: bool) -> Result<Client> {
    if let Some(c) = cell.get() {
        return Ok(c.clone());
    }
    let client = build_client(no_compression)?;
    let _ = cell.set(client);
    Ok(cell.get().expect("client just set").clone())
}

#[derive(Debug, Clone)]
pub struct HttpPoster {
    /// 非流式请求总超时；流式请求仅限制等待响应头的时长；0 = 不限。
    request_timeout: u64,
    /// 单行 / 响应体总字节上限；0 = 不限。
    max_line_bytes: usize,
}

impl HttpPoster {
    pub fn new(request_timeout: u64, max_line_bytes: usize) -> Result<Self> {
        Ok(Self {
            request_timeout,
            max_line_bytes,
        })
    }

    fn apply_timeout(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.request_timeout > 0 {
            req.timeout(Duration::from_secs(self.request_timeout))
        } else {
            req
        }
    }

    fn build_request(
        client: &Client,
        url: &str,
        key: &str,
        body: String,
    ) -> reqwest::RequestBuilder {
        client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .header(AUTHORIZATION, format!("Bearer {}", key))
            .body(body)
    }

    fn error_body_limit(&self) -> usize {
        if self.max_line_bytes == 0 {
            MAX_ERROR_BODY_BYTES
        } else {
            self.max_line_bytes.min(MAX_ERROR_BODY_BYTES)
        }
    }

    async fn send_ok(&self, req: reqwest::RequestBuilder, url: &str) -> Result<reqwest::Response> {
        let started = Instant::now();
        let res = req.send().await.map_err(|e| {
            let code = classify_reqwest_error(&e);
            ClientError::new(code, "HTTP 请求发送失败")
                .with_kv("phase", "http_send")
                .with_kv("url", url.to_string())
                .with_kv("source", e.to_string())
                .with_kv("retryable", e.is_timeout() || e.is_connect())
        })?;
        log::info!(
            "[client:http][post_response] url={} elapsed_ms={} status={}",
            url,
            started.elapsed().as_millis(),
            res.status()
        );

        let status = res.status();
        if !status.is_success() {
            let headers = res.headers().clone();
            let limit = self.error_body_limit();
            let mut stream = res.bytes_stream();
            let mut buffer = Vec::with_capacity(limit.min(8 * 1024));
            let mut truncated = false;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| {
                    ClientError::new(classify_reqwest_error(&e), "读取 HTTP 错误响应失败")
                        .with_kv("phase", "http_error_body")
                        .with_kv("url", url.to_string())
                        .with_kv("status_code", status.as_u16())
                        .with_kv("source", e.to_string())
                })?;
                let remaining = limit.saturating_sub(buffer.len());
                if chunk.len() > remaining {
                    buffer.extend_from_slice(&chunk[..remaining]);
                    truncated = true;
                    break;
                }
                buffer.extend_from_slice(&chunk);
            }
            let text = String::from_utf8_lossy(&buffer);
            return Err(build_http_error(status, &headers, &text, truncated, url).into());
        }
        Ok(res)
    }

    /// 流式请求：返回按行解码的响应流。走禁压缩的 STREAM_CLIENT。
    pub async fn post_json(
        &self,
        url: &str,
        key: &str,
        body: String,
    ) -> Result<impl futures_util::Stream<Item = Result<String>>> {
        log::info!(
            "[client:http][post_json_start] url={} body_bytes={}",
            url,
            body.len()
        );
        let client = shared_client(&STREAM_CLIENT, true)?;
        let req = Self::build_request(&client, url, key, body);
        let res = if self.request_timeout > 0 {
            match tokio::time::timeout(
                Duration::from_secs(self.request_timeout),
                self.send_ok(req, url),
            )
            .await
            {
                Ok(result) => result?,
                Err(_) => {
                    return Err(
                        ClientError::new(ErrorCode::HttpTimeout, "等待流式响应开始超时")
                            .with_kv("phase", "http_stream_start")
                            .with_kv("url", url.to_string())
                            .with_kv("timeout_seconds", self.request_timeout)
                            .with_kv("retryable", true)
                            .into(),
                    );
                }
            }
        } else {
            self.send_ok(req, url).await?
        };

        // bytes_stream → AsyncRead
        let byte_stream = res.bytes_stream().map_err(std::io::Error::other);
        let reader = StreamReader::new(byte_stream);

        // 按行解码（不再 join 再 split）
        let codec = if self.max_line_bytes == 0 {
            LinesCodec::new()
        } else {
            LinesCodec::new_with_max_length(self.max_line_bytes)
        };
        let lines = FramedRead::new(reader, codec).map(|line| {
            line.map_err(|e| {
                ClientError::new(ErrorCode::LlmStreamProtocolError, "流式响应分行解析失败")
                    .with_kv("source", e.to_string())
                    .into()
            })
        });

        Ok(lines)
    }

    /// 非流式请求：收集完整响应体为字符串。走启用压缩的 COLLECT_CLIENT。
    ///
    /// 边读边累积字节，`max_line_bytes > 0` 时作为**解压后**响应体总字节上限，
    /// 超限立即断开（防解压炸弹；不用 `res.bytes()` 一把读到内存）。
    pub async fn post_collect(&self, url: &str, key: &str, body: String) -> Result<String> {
        log::info!(
            "[client:http][post_collect_start] url={} body_bytes={}",
            url,
            body.len()
        );
        let client = shared_client(&COLLECT_CLIENT, false)?;
        let req = self.apply_timeout(Self::build_request(&client, url, key, body));
        let res = self.send_ok(req, url).await?;

        let limit = self.max_line_bytes;
        let mut stream = res.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                ClientError::new(classify_reqwest_error(&e), "读取响应体失败")
                    .with_kv("url", url.to_string())
                    .with_kv("source", e.to_string())
            })?;
            if limit > 0 && buf.len().saturating_add(chunk.len()) > limit {
                return Err(
                    ClientError::new(ErrorCode::HttpResponseTooLarge, "响应体超过上限")
                        .with_kv("url", url.to_string())
                        .with_kv("limit_bytes", limit)
                        .into(),
                );
            }
            buf.extend_from_slice(&chunk);
        }

        String::from_utf8(buf).map_err(|e| {
            ClientError::new(ErrorCode::LlmResponseParseError, "响应体非 UTF-8")
                .with_kv("url", url.to_string())
                .with_kv("source", e.to_string())
                .into()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_server(
        status: &'static str,
        headers: &'static str,
        body: &'static [u8],
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n",
                body.len(),
                headers
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
        });

        format!("http://{address}")
    }

    #[tokio::test]
    async fn post_collect_decompresses_multiline_json() {
        const GZIP_BODY: &[u8] = &[
            31, 139, 8, 0, 0, 0, 0, 0, 4, 0, 171, 230, 82, 80, 80, 202, 207, 86, 178, 82, 40, 41,
            42, 77, 229, 170, 5, 0, 206, 217, 130, 204, 16, 0, 0, 0,
        ];
        let url = spawn_server("200 OK", "Content-Encoding: gzip\r\n", GZIP_BODY).await;
        let poster = HttpPoster::new(5, 1024).unwrap();

        let body = poster
            .post_collect(&url, "test-key", "{}".to_string())
            .await
            .unwrap();

        assert_eq!(body, "{\n  \"ok\": true\n}");
    }

    #[tokio::test]
    async fn stream_timeout_only_limits_response_start() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            socket.write_all(b"data: first\n").await.unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(1_200)).await;
            socket.write_all(b"data: second\n").await.unwrap();
        });

        let poster = HttpPoster::new(1, 1024).unwrap();
        let url = format!("http://{address}");
        let stream = poster
            .post_json(&url, "test-key", "{}".to_string())
            .await
            .unwrap();
        tokio::pin!(stream);

        assert_eq!(stream.next().await.unwrap().unwrap(), "data: first");
        assert_eq!(stream.next().await.unwrap().unwrap(), "data: second");
    }

    #[test]
    fn http_error_extracts_provider_fields() {
        let status = reqwest::StatusCode::UNAUTHORIZED;
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", "request-header".parse().unwrap());
        let body = r#"{
            "error": {
                "code": "invalid_api_key",
                "message": "Incorrect API key provided"
            },
            "request_id": "request-body"
        }"#;

        let error = build_http_error(status, &headers, body, false, "https://example.test");

        assert_eq!(error.code, ErrorCode::HttpUnauthorized);
        assert_eq!(error.detail["status_code"], 401);
        assert_eq!(error.detail["provider_code"], "invalid_api_key");
        assert_eq!(
            error.detail["provider_message"],
            "Incorrect API key provided"
        );
        assert_eq!(error.detail["request_id"], "request-body");
        assert_eq!(error.detail["retryable"], false);
        assert!(error.detail.get("body_preview").is_none());
    }

    #[test]
    fn http_statuses_keep_actionable_categories_separate() {
        assert_eq!(
            map_status(reqwest::StatusCode::PAYMENT_REQUIRED),
            ErrorCode::HttpPaymentRequired
        );
        assert_eq!(
            map_status(reqwest::StatusCode::FORBIDDEN),
            ErrorCode::HttpForbidden
        );
        assert_eq!(
            map_status(reqwest::StatusCode::UNPROCESSABLE_ENTITY),
            ErrorCode::HttpBadRequest
        );
    }

    #[tokio::test]
    async fn error_body_is_capped_before_building_error() {
        const BODY: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
        let url = spawn_server("500 Internal Server Error", "", BODY).await;
        let poster = HttpPoster::new(5, 8).unwrap();

        let error = poster
            .post_collect(&url, "test-key", "{}".to_string())
            .await
            .unwrap_err();
        let error = ClientError::from_anyhow(&error).unwrap();

        assert_eq!(error.code, ErrorCode::HttpServerError);
        assert_eq!(error.detail["body_preview"], "abcdefgh");
        assert_eq!(error.detail["body_truncated"], true);
        assert_eq!(error.detail["retryable"], true);
    }
}
