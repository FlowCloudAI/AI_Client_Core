use anyhow::Result;
use futures_util::{StreamExt, TryStreamExt};
use reqwest::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::io::StreamReader;

use crate::error::{ClientError, ErrorCode};

fn map_status(status: reqwest::StatusCode) -> ErrorCode {
    match status.as_u16() {
        400 => ErrorCode::HttpBadRequest,
        401 | 403 => ErrorCode::HttpUnauthorized,
        404 => ErrorCode::HttpNotFound,
        408 => ErrorCode::HttpTimeout,
        429 => ErrorCode::HttpTooManyRequests,
        500..=599 => ErrorCode::HttpServerError,
        _ => ErrorCode::LlmResponseBadStatus,
    }
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
    /// 每请求超时秒数；0 = 不限。
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
        body: Value,
    ) -> reqwest::RequestBuilder {
        client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .header(AUTHORIZATION, format!("Bearer {}", key))
            .json(&body)
    }

    async fn send_ok(req: reqwest::RequestBuilder, url: &str) -> Result<reqwest::Response> {
        let started = Instant::now();
        let res = req.send().await.map_err(|e| {
            let code = classify_reqwest_error(&e);
            ClientError::new(code, "HTTP 请求发送失败")
                .with_kv("url", url.to_string())
                .with_kv("source", e.to_string())
        })?;
        log::info!(
            "[client:http][post_response] url={} elapsed_ms={} status={}",
            url,
            started.elapsed().as_millis(),
            res.status()
        );

        let status = res.status();
        if !status.is_success() {
            let text = res
                .text()
                .await
                .unwrap_or_else(|e| format!("<读取错误响应体失败: {}>", e));
            return Err(ClientError::new(
                map_status(status),
                format!("HTTP 错误 {}", status.as_u16()),
            )
            .with_kv("url", url.to_string())
            .with_kv("status_code", status.as_u16())
            .with_kv("body", text)
            .into());
        }
        Ok(res)
    }

    /// 流式请求：返回按行解码的响应流。走禁压缩的 STREAM_CLIENT。
    pub async fn post_json(
        &self,
        url: &str,
        key: &str,
        body: Value,
    ) -> Result<impl futures_util::Stream<Item = Result<String>>> {
        log::info!(
            "[client:http][post_json_start] url={} body_bytes={}",
            url,
            body.to_string().len()
        );
        let client = shared_client(&STREAM_CLIENT, true)?;
        let req = self.apply_timeout(Self::build_request(&client, url, key, body));
        let res = Self::send_ok(req, url).await?;

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
    pub async fn post_collect(&self, url: &str, key: &str, body: Value) -> Result<String> {
        log::info!(
            "[client:http][post_collect_start] url={} body_bytes={}",
            url,
            body.to_string().len()
        );
        let client = shared_client(&COLLECT_CLIENT, false)?;
        let req = self.apply_timeout(Self::build_request(&client, url, key, body));
        let res = Self::send_ok(req, url).await?;

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

    async fn spawn_server(headers: &'static str, body: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n",
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
        let url = spawn_server("Content-Encoding: gzip\r\n", GZIP_BODY).await;
        let poster = HttpPoster::new(5, 1024).unwrap();

        let body = poster
            .post_collect(&url, "test-key", serde_json::json!({}))
            .await
            .unwrap();

        assert_eq!(body, "{\n  \"ok\": true\n}");
    }
}
