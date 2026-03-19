use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;

use crate::http_poster::HttpPoster;
use crate::llm::config::SessionConfig;
use crate::plugin::pipeline::ApiPipeline;
use crate::tts::types::*;

// ─────────────────────── TTSSession ─────────────────────

/// TTS 语音合成会话。
///
/// 无状态、单次请求-响应模式：
/// - 不需要 drive loop、事件流、SessionHandle
/// - 可反复调用 `synthesize`，每次独立
/// - 通过 `ApiPipeline` 复用插件 mapper 管道
pub struct TTSSession {
    client: HttpPoster,
    config: SessionConfig,
    pipeline: ApiPipeline,
}

impl TTSSession {
    pub fn new(config: SessionConfig, pipeline: ApiPipeline) -> Result<Self> {
        let client = HttpPoster::new()?;
        Ok(Self {
            client,
            config,
            pipeline,
        })
    }

    /// 合成语音。
    ///
    /// 发送 TTSRequest，返回解码后的音频数据。
    /// 插件通过 pipeline 自动做请求/响应映射。
    pub async fn synthesize(&self, req: &TTSRequest) -> Result<TTSResult> {
        // 序列化 → 插件映射 → 反序列化为 Value
        let json = serde_json::to_value(req)?;
        let mapped_json = self.pipeline.prepare_request_json(&json)?;

        // 发送请求，读取完整响应（非流式）
        let raw_body = self.post_and_collect(mapped_json).await?;

        println!("RAW TTS response: {}", raw_body);

        // 插件映射响应
        let normalized = self.pipeline.map_response(&raw_body)?;

        println!("MAP TTS response: {}", normalized);

        // 解析响应
        let resp: TTSResponse = serde_json::from_str(&normalized)
            .context("failed to parse TTS response")?;

        println!("NORM TTS response: {:?}", resp);

        // 检查状态
        if let Some(ref base) = resp.base_resp {
            if base.status_code != 0 {
                let msg = base.status_msg.as_deref().unwrap_or("unknown error");
                return Err(anyhow!("TTS error ({}): {}", base.status_code, msg));
            }
        }

        // 提取结果
        self.extract_result(resp)
    }

    /// 便捷方法：最简调用，只需 model + text + voice_id。
    pub async fn speak(&self, model: &str, text: &str, voice_id: &str) -> Result<TTSResult> {
        let req = TTSRequest::new(model, text, voice_id);
        self.synthesize(&req).await
    }

    // ── 内部方法 ──

    /// 发送 POST 请求并收集完整响应体。
    async fn post_and_collect(&self, json: serde_json::Value) -> Result<String> {
        let stream = self
            .client
            .post_json(&self.config.base_url, &self.config.api_key, json)
            .await
            .context("TTS request failed")?;

        tokio::pin!(stream);

        let mut body = String::new();
        while let Some(chunk) = stream.next().await {
            body.push_str(&chunk?);
        }

        if body.is_empty() {
            return Err(anyhow!("TTS response empty"));
        }

        Ok(body)
    }

    /// 从响应中提取音频数据。
    fn extract_result(&self, resp: TTSResponse) -> Result<TTSResult> {
        let data = resp.data
            .ok_or_else(|| anyhow!("TTS response missing audio data"))?;

        let extra = resp.extra_info.as_ref();

        // 优先用 hex 数据
        let audio = if let Some(ref hex_str) = data.audio {
            if !hex_str.is_empty() {
                hex::decode(hex_str).context("failed to decode hex audio")?
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        // hex 为空但有 URL → 标记为 url 模式，调用方自行下载
        if audio.is_empty() && data.url.as_ref().map_or(true, |u| u.is_empty()) {
            return Err(anyhow!("empty audio data and no URL provided"));
        }

        Ok(TTSResult {
            audio,
            format: extra
                .and_then(|e| e.audio_format.clone())
                .unwrap_or_else(|| "mp3".to_string()),
            duration_ms: extra.and_then(|e| e.audio_length),
            size: extra.and_then(|e| e.audio_size),
            usage_characters: extra.and_then(|e| e.usage_characters),
            url: data.url,
        })
    }
}