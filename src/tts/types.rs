use serde::{Deserialize, Serialize};

// ─────────────────────── 请求类型 ───────────────────────

/// TTS 合成请求。
///
/// 基准接口：MiniMax T2A v2。
/// 其他供应商（千问等）通过 wasm 插件的 `map_request` 映射到此格式。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TTSRequest {
    /// 模型名称
    pub model: String,

    /// 要合成的文本（上限 10,000 字符）
    pub text: String,

    /// 是否流式输出（默认 false）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,

    /// 语音设置
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice_setting: Option<VoiceSetting>,

    /// 音频设置
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_setting: Option<AudioSetting>,

    /// 语种增强
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language_boost: Option<String>,

    /// 输出格式：`url` 或 `hex`（默认 `hex`）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_format: Option<OutputFormat>,

    /// 发音词典
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pronunciation_dict: Option<PronunciationDict>,

    /// 音色混合权重
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timbre_weights: Option<Vec<TimbreWeight>>,

    /// 变声效果
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice_modify: Option<VoiceModify>,

    /// 是否生成字幕
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_enable: Option<bool>,
}

impl TTSRequest {
    /// 最简构造：model + text + voice_id
    pub fn new(model: &str, text: &str, voice_id: &str) -> Self {
        Self {
            model: model.to_string(),
            text: text.to_string(),
            stream: Some(false),
            voice_setting: Some(VoiceSetting {
                voice_id: voice_id.to_string(),
                speed: None,
                vol: None,
                pitch: None,
                emotion: None,
            }),
            audio_setting: None,
            language_boost: None,
            output_format: None,
            pronunciation_dict: None,
            timbre_weights: None,
            voice_modify: None,
            subtitle_enable: None,
        }
    }

    pub fn format(mut self, format: AudioFormat) -> Self {
        let setting = self.audio_setting.get_or_insert(AudioSetting::default());
        setting.format = Some(format);
        self
    }

    pub fn sample_rate(mut self, rate: u32) -> Self {
        let setting = self.audio_setting.get_or_insert(AudioSetting::default());
        setting.sample_rate = Some(rate);
        self
    }

    pub fn speed(mut self, speed: f32) -> Self {
        if let Some(ref mut vs) = self.voice_setting {
            vs.speed = Some(speed);
        }
        self
    }

    pub fn language(mut self, lang: &str) -> Self {
        self.language_boost = Some(lang.to_string());
        self
    }

    pub fn output_url(mut self) -> Self {
        self.output_format = Some(OutputFormat::Url);
        self
    }

    pub fn output_hex(mut self) -> Self {
        self.output_format = Some(OutputFormat::Hex);
        self
    }
}

// ─────────────────────── 语音设置 ───────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceSetting {
    /// 音色 ID
    pub voice_id: String,

    /// 语速，范围 [0.5, 2.0]，默认 1.0
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<f32>,

    /// 音量，范围 (0, 10]，默认 1.0
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vol: Option<f32>,

    /// 音调，范围 [-12, 12]，默认 0
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pitch: Option<i32>,

    /// 情感控制
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emotion: Option<Emotion>,
}

// ─────────────────────── 音频设置 ───────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AudioSetting {
    /// 采样率：8000, 16000, 22050, 24000, 32000, 44100
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,

    /// 比特率：32000, 64000, 128000, 256000（仅 mp3）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<u32>,

    /// 音频格式
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<AudioFormat>,

    /// 声道数：1=单声道，2=立体声
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<u8>,
}

// ─────────────────────── 变声效果 ───────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceModify {
    /// 深沉/明亮 [-100, 100]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pitch: Option<i32>,

    /// 强烈/柔和 [-100, 100]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intensity: Option<i32>,

    /// 浑厚/清脆 [-100, 100]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timbre: Option<i32>,

    /// 音效
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sound_effects: Option<SoundEffect>,
}

// ─────────────────────── 发音词典 ───────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PronunciationDict {
    /// 发音规则，如 ["omg/oh my god"]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tone: Option<Vec<String>>,
}

// ─────────────────────── 混音权重 ───────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimbreWeight {
    pub voice_id: String,
    /// 权重 [1, 100]，最多 4 个音色混合
    pub weight: u32,
}

// ─────────────────────── 枚举类型 ───────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    Mp3,
    Pcm,
    Flac,
    Wav,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Url,
    Hex,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Emotion {
    Happy,
    Sad,
    Angry,
    Fearful,
    Disgusted,
    Surprised,
    Calm,
    Fluent,
    Whisper,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoundEffect {
    SpaciousEcho,
    AuditoriumEcho,
    LofiTelephone,
    Robotic,
}

// ─────────────────────── 响应类型 ───────────────────────

/// TTS 合成响应。
#[derive(Debug, Clone, Deserialize)]
pub struct TTSResponse {
    /// 音频数据
    pub data: Option<TTSAudioData>,

    /// 额外信息（时长、大小、计费等）
    pub extra_info: Option<TTSExtraInfo>,

    /// 请求跟踪 ID
    pub trace_id: Option<String>,

    /// 状态
    pub base_resp: Option<TTSBaseResp>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TTSAudioData {
    /// hex 编码的音频数据，或空（当 output_format=url 时）
    pub audio: Option<String>,

    /// 音频 URL（当 output_format=url 时）
    pub url: Option<String>,

    /// 字幕文件下载链接
    pub subtitle_file: Option<String>,

    /// 状态：1=合成中，2=合成完成
    pub status: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TTSExtraInfo {
    /// 音频时长（毫秒）
    pub audio_length: Option<u64>,

    /// 采样率
    pub audio_sample_rate: Option<u32>,

    /// 文件大小（字节）
    pub audio_size: Option<u64>,

    /// 比特率
    pub bitrate: Option<u32>,

    /// 音频格式
    pub audio_format: Option<String>,

    /// 声道数
    pub audio_channel: Option<u8>,

    /// 计费字符数
    pub usage_characters: Option<u64>,

    /// 文字数（中文字符+数字+字母）
    pub word_count: Option<u64>,

    /// 无效字符比例
    pub invisible_character_ratio: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TTSBaseResp {
    /// 状态码：0=成功
    pub status_code: i32,

    /// 状态描述
    pub status_msg: Option<String>,
}

// ─────────────────────── 便捷结果类型 ───────────────────

/// TTSSession::synthesize 的返回类型，屏蔽原始响应细节。
#[derive(Debug, Clone)]
pub struct TTSResult {
    /// 音频数据（hex 解码后的原始字节）
    pub audio: Vec<u8>,

    /// 音频格式
    pub format: String,

    /// 音频时长（毫秒）
    pub duration_ms: Option<u64>,

    /// 文件大小（字节）
    pub size: Option<u64>,

    /// 计费字符数
    pub usage_characters: Option<u64>,

    /// 音频 URL（如果请求了 url 格式）
    pub url: Option<String>,
}