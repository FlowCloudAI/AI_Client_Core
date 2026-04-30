use std::time::Duration;
use anyhow::{Result, anyhow};
use futures_util::{StreamExt, TryStreamExt};
use reqwest::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::io::StreamReader;

#[derive(Debug)]
pub struct HttpPoster {
    client: Client,
}

impl HttpPoster {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(120))
                // SSE 流式传输禁用自动解压，防止 Qwen 等会返回
                // gzip 压缩响应的 API 出现 "error decoding response body"
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .build()?,
        })
    }

    pub async fn post_json(
        &self,
        url: &str,
        key: &str,
        body: Value,
    ) -> Result<impl futures_util::Stream<Item = Result<String>>> {
        let req = self
            .client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .header(AUTHORIZATION, format!("Bearer {}", key));

        let res = req.json(&body).send().await?;

        let status = res.status();
        if !status.is_success() {
            let text = res.text().await.unwrap_or_default();
            return Err(anyhow!("HTTP 错误 {}: {}", status, text));
        }

        // bytes_stream → AsyncRead
        let byte_stream = res
            .bytes_stream()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
        let reader = StreamReader::new(byte_stream);

        // 按行解码（不再 join 再 split）
        let lines = FramedRead::new(reader, LinesCodec::new())
            .map(|line| line.map_err(|e| anyhow!(e)).map(|s| s));

        Ok(lines)
    }
}
