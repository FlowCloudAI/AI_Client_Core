use anyhow::Result;
use futures_util::{StreamExt, TryStreamExt};
use reqwest::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
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

#[derive(Debug)]
pub struct HttpPoster {
    client: Client,
    max_line_bytes: usize,
}

impl HttpPoster {
    pub fn new(request_timeout: u64, max_line_bytes: usize) -> Result<Self> {
        let mut builder = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // SSE 流式传输禁用自动解压，防止 Qwen 等会返回
            // gzip 压缩响应的 API 出现 "error decoding response body"
            .no_gzip()
            .no_brotli()
            .no_deflate();
        if request_timeout > 0 {
            builder = builder.timeout(Duration::from_secs(request_timeout));
        }
        let client = builder.build().map_err(|e| {
            ClientError::new(ErrorCode::CoreClientInitFailed, "构建 HTTP 客户端失败")
                .with_kv("source", e.to_string())
        })?;
        Ok(Self {
            client,
            max_line_bytes,
        })
    }

    pub async fn post_json(
        &self,
        url: &str,
        key: &str,
        body: Value,
    ) -> Result<impl futures_util::Stream<Item = Result<String>>> {
        let body_bytes = body.to_string().len();
        log::info!(
            "[client:http][post_json_start] url={} body_bytes={}",
            url,
            body_bytes
        );
        let req = self
            .client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .header(AUTHORIZATION, format!("Bearer {}", key));

        let started = Instant::now();
        let res = req.json(&body).send().await.map_err(|e| {
            let code = classify_reqwest_error(&e);
            ClientError::new(code, "HTTP 请求发送失败")
                .with_kv("url", url.to_string())
                .with_kv("source", e.to_string())
        })?;
        log::info!(
            "[client:http][post_json_response] url={} elapsed_ms={} status={}",
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
}
