use anyhow::Result;
use futures_util::StreamExt;

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
        let client = HttpPoster::new()?;
        Ok(Self {
            client,
            config,
            pipeline,
        })
    }

    /// 完整调用：发送 ImageRequest，返回解析后的结果。
    pub async fn generate(&self, req: &ImageRequest) -> Result<ImageResult> {
        let json = serde_json::to_value(req).map_err(|e| {
            ClientError::new(ErrorCode::ImageTaskInvalidParams, "图像请求序列化失败")
                .with_kv("source", e.to_string())
        })?;
        let mapped_json = self.pipeline.prepare_request_json(&json)?;
        log::debug!(
            "[image] mapped request bytes={}",
            mapped_json.to_string().len()
        );

        let raw_body = self.post_and_collect(mapped_json).await?;

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

    // ── 内部方法 ──

    async fn post_and_collect(&self, json: serde_json::Value) -> Result<String> {
        let stream = self
            .client
            .post_json(&self.config.base_url, &self.config.api_key, json)
            .await?;

        tokio::pin!(stream);

        let mut body = String::new();
        while let Some(chunk) = stream.next().await {
            body.push_str(&chunk?);
        }

        if body.is_empty() {
            return Err(ClientError::new(ErrorCode::ImageTaskEmptyResponse, "图像响应为空").into());
        }

        log::debug!("[image] raw response bytes={}", body.len());

        Ok(body)
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
