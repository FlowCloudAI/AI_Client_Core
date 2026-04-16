use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;

use crate::http_poster::HttpPoster;
use crate::llm::config::SessionConfig;
use crate::plugin::pipeline::ApiPipeline;
use crate::image::types::*;

// ─────────────────────── ImageSession ───────────────────

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
        let json = serde_json::to_value(req)?;
        let mapped_json = self.pipeline.prepare_request_json(&json)?;
        
        println!("[generate] mapped_json:{}", mapped_json);

        let raw_body = self.post_and_collect(mapped_json).await?;

        let normalized = self.pipeline.map_response(&raw_body)?;

        let resp: ImageResponse = serde_json::from_str(&normalized)
            .context("failed to parse image response")?;

        // 检查错误
        if let Some(ref err) = resp.error {
            let code = err.code.as_deref().unwrap_or("unknown");
            let msg = err.message.as_deref().unwrap_or("unknown error");
            return Err(anyhow!("Image generation error ({}): {}", code, msg));
        }

        self.extract_result(resp)
    }

    /// 便捷方法：文生图
    pub async fn text_to_image(
        &self,
        model: &str,
        prompt: &str,
    ) -> Result<ImageResult> {
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
        let stream = match self
            .client
            .post_json(&self.config.base_url, &self.config.api_key, json)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[post_and_collect] post_json failed: {}", e);
                return Err(anyhow!("Image request failed [url={}]: {}", self.config.base_url, e));
            }
        };

        tokio::pin!(stream);

        let mut body = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(c) => body.push_str(&c),
                Err(e) => {
                    eprintln!("[post_and_collect] stream chunk error: {}", e);
                    return Err(anyhow!("Image request stream failed: {}", e));
                }
            }
        }

        if body.is_empty() {
            return Err(anyhow!("Image response empty"));
        }

        println!("[post_and_collect] raw response body (first 1024 chars): {}", &body[..body.len().min(1024)]);

        Ok(body)
    }

    fn extract_result(&self, resp: ImageResponse) -> Result<ImageResult> {
        let data_list = resp.data
            .ok_or_else(|| anyhow!("Image response missing data"))?;

        if data_list.is_empty() {
            return Err(anyhow!("Image response returned no images"));
        }

        let mut images = Vec::with_capacity(data_list.len());

        for item in data_list {
            let data = if let Some(ref b64) = item.b64_json {
                if !b64.is_empty() {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .context("failed to decode b64_json image")?
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