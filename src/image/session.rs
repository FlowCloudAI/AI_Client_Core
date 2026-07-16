use anyhow::Result;

use crate::error::{ClientError, ErrorCode};
use crate::http_poster::HttpPoster;
use crate::image::types::*;
use crate::llm::config::SessionConfig;
use crate::plugin::pipeline::ApiPipeline;

// ─────────────────────── 图像会话 ──────────────────────

/// 图像生成会话。
///
/// 无状态、单次请求-响应模式：
/// - 支持文生图、图文生图、多图融合、组图生成
/// - 通过 `ApiPipeline` 复用插件 mapper 管道
/// - 可反复调用 `generate`，每次独立
pub struct ImageSession {
    client: HttpPoster,
    config: SessionConfig,
    pipeline: ApiPipeline,
}

impl ImageSession {
    pub fn new(config: SessionConfig, pipeline: ApiPipeline) -> Result<Self> {
        config.validate()?;
        let client = HttpPoster::new(config.request_timeout, config.max_line_bytes)?;
        Ok(Self {
            client,
            config,
            pipeline,
        })
    }

    /// 完整调用：发送 ImageRequest，返回解析后的结果。
    pub async fn generate(&self, req: &ImageRequest) -> Result<ImageResult> {
        let body = self.pipeline.prepare_request_body(req)?;
        log::debug!("[image] mapped request bytes={}", body.len());

        let raw_body = self
            .client
            .post_collect(&self.config.base_url, self.config.api_key.expose(), body)
            .await?;
        if raw_body.is_empty() {
            return Err(ClientError::new(ErrorCode::ImageTaskEmptyResponse, "图像响应为空").into());
        }

        let normalized = self.pipeline.map_response(&raw_body)?;

        let resp: ImageResponse = serde_json::from_str(&normalized).map_err(|e| {
            ClientError::new(ErrorCode::LlmResponseParseError, "图像响应解析失败")
                .with_kv("source", e.to_string())
        })?;

        // 检查错误
        if let Some(ref err) = resp.error {
            let code = err.code.as_deref().unwrap_or("unknown");
            let msg = err.message.as_deref().unwrap_or("unknown error");
            return Err(ClientError::new(
                ErrorCode::ImageTaskFailed,
                format!("图像生成失败 ({}): {}", code, msg),
            )
            .with_kv("provider_code", code.to_string())
            .with_kv("message", msg.to_string())
            .into());
        }

        self.extract_result(resp)
    }

    /// 便捷方法：文生图
    pub async fn text_to_image(&self, model: &str, prompt: &str) -> Result<ImageResult> {
        let req = ImageRequest::text_to_image(model, prompt);
        self.generate(&req).await
    }

    /// 便捷方法：单图编辑
    pub async fn edit_image(
        &self,
        model: &str,
        prompt: &str,
        image_url: &str,
    ) -> Result<ImageResult> {
        let req = ImageRequest::image_to_image(model, prompt, image_url);
        self.generate(&req).await
    }

    /// 便捷方法：多图融合
    pub async fn merge_images(
        &self,
        model: &str,
        prompt: &str,
        image_urls: Vec<String>,
    ) -> Result<ImageResult> {
        let req = ImageRequest::images_to_image(model, prompt, image_urls);
        self.generate(&req).await
    }

    fn extract_result(&self, resp: ImageResponse) -> Result<ImageResult> {
        let data_list = resp.data.ok_or_else(|| {
            ClientError::new(ErrorCode::ImageTaskEmptyResponse, "图像响应缺少 data")
        })?;

        if data_list.is_empty() {
            return Err(ClientError::new(
                ErrorCode::ImageTaskEmptyResponse,
                "图像响应未返回任何图片",
            )
            .into());
        }

        let mut images = Vec::with_capacity(data_list.len());

        for item in data_list {
            let data = if let Some(ref b64) = item.b64_json {
                if !b64.is_empty() {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .map_err(|e| {
                            ClientError::new(ErrorCode::ImageTaskFailed, "解码 b64_json 图片失败")
                                .with_kv("source", e.to_string())
                        })?
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };

            images.push(GeneratedImage {
                url: item.url,
                data,
                size: item.size,
                revised_prompt: item.revised_prompt,
            });
        }

        Ok(ImageResult {
            images,
            usage: resp.usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::config::SecretString;
    use crate::plugin::registry::PluginRegistry;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::{Duration, sleep};

    async fn spawn_server(delay: Duration, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await;
            sleep(delay).await;

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });

        format!("http://{address}")
    }

    fn session(config: SessionConfig) -> ImageSession {
        let registry = Arc::new(PluginRegistry::empty().unwrap());
        let pipeline = ApiPipeline::try_new(registry, None).unwrap();
        ImageSession::new(config, pipeline).unwrap()
    }

    fn config(base_url: String, request_timeout: u64, max_line_bytes: usize) -> SessionConfig {
        SessionConfig {
            base_url,
            api_key: SecretString::from("test-key"),
            event_buffer: 0,
            request_timeout,
            max_tool_rounds: 0,
            max_line_bytes,
        }
    }

    #[test]
    fn new_rejects_invalid_config() {
        let registry = Arc::new(PluginRegistry::empty().unwrap());
        let pipeline = ApiPipeline::try_new(registry, None).unwrap();

        let error = match ImageSession::new(SessionConfig::default(), pipeline) {
            Ok(_) => panic!("无效配置不应创建图片会话"),
            Err(error) => error,
        };
        assert_eq!(
            ClientError::from_anyhow(&error).map(|error| error.code),
            Some(ErrorCode::ValidationMissingField)
        );
    }

    #[tokio::test]
    async fn request_timeout_is_applied() {
        let url = spawn_server(
            Duration::from_millis(1_500),
            r#"{"data":[{"url":"https://example.test/image.png"}]}"#,
        )
        .await;
        let session = session(config(url, 1, 1024));

        let error = session
            .text_to_image("test-model", "test-prompt")
            .await
            .expect_err("请求应按会话配置超时");
        assert_eq!(
            ClientError::from_anyhow(&error).map(|error| error.code),
            Some(ErrorCode::HttpTimeout)
        );
    }

    #[tokio::test]
    async fn max_line_bytes_is_applied() {
        let url = spawn_server(
            Duration::ZERO,
            r#"{"data":[{"url":"https://example.test/image.png"}]}"#,
        )
        .await;
        let session = session(config(url, 5, 32));

        let error = session
            .text_to_image("test-model", "test-prompt")
            .await
            .expect_err("超长响应行应被拒绝");
        assert_eq!(
            ClientError::from_anyhow(&error).map(|error| error.code),
            Some(ErrorCode::HttpResponseTooLarge)
        );
    }
}
