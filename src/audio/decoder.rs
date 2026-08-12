use anyhow::Result;
use base64::Engine as _;
use cpal::StreamConfig;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::error::{ClientError, ErrorCode};

fn decode_err(message: impl Into<String>, source: impl ToString) -> ClientError {
    ClientError::new(ErrorCode::AudioDecodeFailed, message).with_kv("source", source.to_string())
}

fn playback_err(message: impl Into<String>, source: impl ToString) -> ClientError {
    ClientError::new(ErrorCode::AudioPlaybackFailed, message).with_kv("source", source.to_string())
}

enum PlaybackEnd {
    Completed,
    Cancelled,
    Failed(ClientError),
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

    /// 无容器头的 16 位小端 PCM 字节
    Pcm {
        data: Vec<u8>,
        sample_rate: u32,
        channels: u16,
    },
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
            AudioSource::Hex(hex) => {
                hex::decode(hex).map_err(|e| decode_err("hex 音频解码失败", e).into())
            }
            AudioSource::Base64(b64) => base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| decode_err("base64 音频解码失败", e).into()),
            AudioSource::Url(url) => Self::fetch_url(url).await,
            AudioSource::Raw(bytes) => Ok(bytes.clone()),
            AudioSource::Pcm { data, .. } => Ok(data.clone()),
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

    /// 将无容器头的 16 位小端 PCM 字节转换为播放采样。
    pub fn decode_pcm_s16le(data: &[u8], sample_rate: u32, channels: u16) -> Result<DecodedAudio> {
        if sample_rate == 0 || channels == 0 {
            return Err(decode_err("PCM 音频参数无效", "zero_sample_rate_or_channels").into());
        }
        if data.is_empty() || !data.len().is_multiple_of(2 * usize::from(channels)) {
            return Err(decode_err("PCM 音频数据不完整", format!("bytes={}", data.len())).into());
        }

        Ok(DecodedAudio {
            samples: data
                .chunks_exact(2)
                .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]) as f32 / 32768.0)
                .collect(),
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
        if let AudioSource::Pcm {
            data,
            sample_rate,
            channels,
        } = source
        {
            return Self::decode_pcm_s16le(data, *sample_rate, *channels);
        }
        let bytes = Self::resolve(source).await?;
        Self::decode(&bytes, format_hint)
    }

    // ── 播放 ──

    /// 使用 cpal 播放 DecodedAudio 到默认输出设备。
    ///
    /// 阻塞直到播放完成。在 async 上下文中应 spawn_blocking。
    pub fn play(audio: &DecodedAudio) -> Result<()> {
        Self::play_cancelable(audio, Arc::new(AtomicBool::new(false)))
    }

    /// 播放到结束或取消；取消属于正常结束，不能恢复。
    pub fn play_cancelable(audio: &DecodedAudio, cancelled: Arc<AtomicBool>) -> Result<()> {
        if cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
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
        let total = samples.len();

        let (done_tx, done_rx) = std::sync::mpsc::channel::<PlaybackEnd>();
        let reported = Arc::new(AtomicBool::new(false));
        let cancelled_callback = Arc::clone(&cancelled);
        let reported_callback = Arc::clone(&reported);
        let reported_error = Arc::clone(&reported);
        let done_tx_error = done_tx.clone();
        let mut position = 0usize;

        let stream = device
            .build_output_stream(
                &config,
                move |output: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if cancelled_callback.load(Ordering::Acquire) {
                        output.fill(0.0);
                        if !reported_callback.swap(true, Ordering::AcqRel) {
                            let _ = done_tx.send(PlaybackEnd::Cancelled);
                        }
                        return;
                    }
                    for sample in output.iter_mut() {
                        if position < total {
                            *sample = samples[position];
                            position += 1;
                        } else {
                            *sample = 0.0;
                        }
                    }
                    if position >= total && !reported_callback.swap(true, Ordering::AcqRel) {
                        let _ = done_tx.send(PlaybackEnd::Completed);
                    }
                },
                move |err| {
                    if !reported_error.swap(true, Ordering::AcqRel) {
                        let _ = done_tx_error
                            .send(PlaybackEnd::Failed(playback_err("音频输出流运行失败", err)));
                    }
                },
                None,
            )
            .map_err(|e| playback_err("构建音频输出流失败", e))?;

        stream
            .play()
            .map_err(|e| playback_err("启动音频播放失败", e))?;

        loop {
            match done_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(PlaybackEnd::Completed) => {
                    // 等最后一个 buffer 交给系统音频设备。
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    break;
                }
                Ok(PlaybackEnd::Cancelled) => break,
                Ok(PlaybackEnd::Failed(error)) => return Err(error.into()),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    if cancelled.load(Ordering::Acquire) =>
                {
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(error) => return Err(playback_err("等待音频播放结束失败", error).into()),
            }
        }

        drop(stream);
        Ok(())
    }

    /// 一站式：从 AudioSource 解码并播放。
    ///
    /// 在 async 上下文中使用，播放部分自动 spawn_blocking。
    pub async fn play_source(source: &AudioSource, format_hint: Option<&str>) -> Result<()> {
        Self::play_source_cancelable(source, format_hint, Arc::new(AtomicBool::new(false))).await
    }

    /// 一站式解码并播放到结束或取消。
    pub async fn play_source_cancelable(
        source: &AudioSource,
        format_hint: Option<&str>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<()> {
        if cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        let audio = Self::decode_source(source, format_hint).await?;
        if cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        tokio::task::spawn_blocking(move || Self::play_cancelable(&audio, cancelled))
            .await
            .map_err(|e| playback_err("播放任务 panic", e))?
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioDecoder, AudioSource};

    #[tokio::test]
    async fn 裸_pcm_无需探测容器格式即可解码() {
        let audio = AudioDecoder::decode_source(
            &AudioSource::Pcm {
                data: vec![0, 0, 0xff, 0x7f, 0, 0x80],
                sample_rate: 24_000,
                channels: 1,
            },
            Some("pcm"),
        )
        .await
        .unwrap();

        assert_eq!(audio.sample_rate, 24_000);
        assert_eq!(audio.channels, 1);
        assert_eq!(audio.samples, vec![0.0, 32767.0 / 32768.0, -1.0]);
    }
}
