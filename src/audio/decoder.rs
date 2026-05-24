use anyhow::Result;
use base64::Engine as _;
use cpal::StreamConfig;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::io::Cursor;
use std::sync::Arc;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tokio::sync::Mutex;

use crate::error::{ClientError, ErrorCode};

fn decode_err(message: impl Into<String>, source: impl ToString) -> ClientError {
    ClientError::new(ErrorCode::AudioDecodeFailed, message).with_kv("source", source.to_string())
}

fn playback_err(message: impl Into<String>, source: impl ToString) -> ClientError {
    ClientError::new(ErrorCode::AudioPlaybackFailed, message).with_kv("source", source.to_string())
}

// ─────────────────────── 音频来源 ──────────────────────────

/// 音频数据来源。
#[derive(Debug, Clone)]
pub enum AudioSource {
    /// Hex 编码的音频数据（MiniMax 原生格式）
    Hex(String),

    /// Base64 编码的音频数据（千问流式格式）
    Base64(String),

    /// 音频文件 URL（千问非流式、或 MiniMax output_format=url）
    Url(String),

    /// 原始字节
    Raw(Vec<u8>),
}

impl AudioSource {
    /// 自动检测字符串编码类型。
    ///
    /// - 全是 `[0-9a-fA-F]` → Hex
    /// - 含 `[A-Za-z0-9+/=]` 且有大写或 `+/=` → Base64
    /// - 以 `http` 开头 → Url
    pub fn detect(s: &str) -> Self {
        let trimmed = s.trim();
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            return Self::Url(trimmed.to_string());
        }
        if trimmed.is_empty() {
            return Self::Raw(Vec::new());
        }
        if trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
            Self::Hex(trimmed.to_string())
        } else {
            Self::Base64(trimmed.to_string())
        }
    }
}

// ─────────────────────── 解码音频 ──────────────────────────

/// 解码后的 PCM 音频数据。
#[derive(Debug, Clone)]
pub struct DecodedAudio {
    /// 交错的 f32 PCM 采样
    pub samples: Vec<f32>,

    /// 采样率
    pub sample_rate: u32,

    /// 声道数
    pub channels: u16,
}

// ─────────────────────── 音频解码器 ─────────────────────────

/// 统一的音频解码器。
///
/// 职责：
/// 1. 从 AudioSource（hex/base64/url/raw）获取原始字节
/// 2. 通过 symphonia 解码为 PCM f32 采样
/// 3. 通过 cpal 播放到默认音频设备
pub struct AudioDecoder;

impl AudioDecoder {
    // ── 数据获取 ──

    /// 将 AudioSource 解析为原始音频字节。
    pub async fn resolve(source: &AudioSource) -> Result<Vec<u8>> {
        match source {
            AudioSource::Hex(hex) => hex::decode(hex)
                .map_err(|e| decode_err("hex 音频解码失败", e).into()),
            AudioSource::Base64(b64) => base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| decode_err("base64 音频解码失败", e).into()),
            AudioSource::Url(url) => Self::fetch_url(url).await,
            AudioSource::Raw(bytes) => Ok(bytes.clone()),
        }
    }

    /// 从 URL 下载音频数据。
    async fn fetch_url(url: &str) -> Result<Vec<u8>> {
        let resp = reqwest::get(url).await.map_err(|e| {
            ClientError::new(ErrorCode::LlmRequestNetworkError, "下载音频 URL 失败")
                .with_kv("url", url.to_string())
                .with_kv("source", e.to_string())
        })?;

        if !resp.status().is_success() {
            return Err(ClientError::new(
                ErrorCode::HttpServerError,
                format!("音频 URL 返回非成功状态 {}", resp.status()),
            )
            .with_kv("url", url.to_string())
            .with_kv("status_code", resp.status().as_u16())
            .into());
        }

        resp.bytes().await.map(|b| b.to_vec()).map_err(|e| {
            ClientError::new(ErrorCode::LlmRequestNetworkError, "读取音频响应体失败")
                .with_kv("url", url.to_string())
                .with_kv("source", e.to_string())
                .into()
        })
    }

    // ── 解码 ──

    /// 使用 symphonia 将音频字节解码为 PCM f32。
    ///
    /// 支持 mp3, wav, flac, ogg 等 symphonia 支持的格式。
    /// `format_hint` 可选，如 "mp3", "wav", "flac"。
    pub fn decode(data: &[u8], format_hint: Option<&str>) -> Result<DecodedAudio> {
        if data.is_empty() {
            return Err(decode_err("音频数据为空", "empty").into());
        }

        let cursor = Cursor::new(data.to_vec());
        let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

        let mut hint = Hint::new();
        if let Some(fmt) = format_hint {
            hint.with_extension(fmt);
        }

        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                mss,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .map_err(|e| decode_err("探测音频格式失败", e))?;

        let mut format_reader = probed.format;

        let track = format_reader
            .default_track()
            .ok_or_else(|| decode_err("未找到音频轨道", "no_track"))?;

        let sample_rate = track
            .codec_params
            .sample_rate
            .ok_or_else(|| decode_err("无法识别采样率", "unknown_sample_rate"))?;
        let channels = track
            .codec_params
            .channels
            .map(|c| c.count() as u16)
            .unwrap_or(1);
        let track_id = track.id;

        let mut decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .map_err(|e| decode_err("创建音频解码器失败", e))?;

        let mut all_samples: Vec<f32> = Vec::new();

        loop {
            let packet = match format_reader.next_packet() {
                Ok(p) => p,
                Err(symphonia::core::errors::Error::IoError(ref e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    break;
                }
                Err(e) => return Err(decode_err("读取音频 packet 失败", e).into()),
            };

            if packet.track_id() != track_id {
                continue;
            }

            let decoded = match decoder.decode(&packet) {
                Ok(d) => d,
                Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
                Err(e) => return Err(decode_err("音频帧解码失败", e).into()),
            };

            let spec = *decoded.spec();
            let duration = decoded.capacity();

            let mut sample_buf = SampleBuffer::<f32>::new(duration as u64, spec);
            sample_buf.copy_interleaved_ref(decoded);
            all_samples.extend_from_slice(sample_buf.samples());
        }

        if all_samples.is_empty() {
            return Err(decode_err("解码得到 0 个采样", "zero_samples").into());
        }

        Ok(DecodedAudio {
            samples: all_samples,
            sample_rate,
            channels,
        })
    }

    // ── 一站式方法 ──

    /// 从 AudioSource 解析 + 解码为 PCM。
    pub async fn decode_source(
        source: &AudioSource,
        format_hint: Option<&str>,
    ) -> Result<DecodedAudio> {
        let bytes = Self::resolve(source).await?;
        Self::decode(&bytes, format_hint)
    }

    // ── 播放 ──

    /// 使用 cpal 播放 DecodedAudio 到默认输出设备。
    ///
    /// 阻塞直到播放完成。在 async 上下文中应 spawn_blocking。
    pub fn play(audio: &DecodedAudio) -> Result<()> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| {
            ClientError::new(ErrorCode::AudioDeviceUnavailable, "未找到默认音频输出设备")
        })?;

        let config = StreamConfig {
            channels: audio.channels,
            sample_rate: audio.sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let samples = Arc::new(audio.samples.clone());
        let position = Arc::new(Mutex::new(0usize));
        let total = samples.len();

        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();

        let samples_clone = Arc::clone(&samples);
        let position_clone = Arc::clone(&position);
        let done_tx_clone = done_tx.clone();

        let stream = device
            .build_output_stream(
                &config,
                move |output: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    // 同步锁，在音频回调中不能用 async
                    let mut pos = position_clone.blocking_lock();
                    for sample in output.iter_mut() {
                        if *pos < total {
                            *sample = samples_clone[*pos];
                            *pos += 1;
                        } else {
                            *sample = 0.0;
                        }
                    }
                    if *pos >= total {
                        let _ = done_tx_clone.send(());
                    }
                },
                move |err| {
                    log::warn!("[AudioDecoder] playback error: {}", err);
                },
                None,
            )
            .map_err(|e| playback_err("构建音频输出流失败", e))?;

        stream.play().map_err(|e| playback_err("启动音频播放失败", e))?;

        // 等待播放完成
        let _ = done_rx.recv();

        // 给一点时间让最后的 buffer flush
        std::thread::sleep(std::time::Duration::from_millis(100));

        drop(stream);
        Ok(())
    }

    /// 一站式：从 AudioSource 解码并播放。
    ///
    /// 在 async 上下文中使用，播放部分自动 spawn_blocking。
    pub async fn play_source(source: &AudioSource, format_hint: Option<&str>) -> Result<()> {
        let audio = Self::decode_source(source, format_hint).await?;
        tokio::task::spawn_blocking(move || Self::play(&audio))
            .await
            .map_err(|e| playback_err("播放任务 panic", e))?
    }
}
