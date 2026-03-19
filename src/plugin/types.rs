use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ─────────────────────── PluginKind ─────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub enum PluginKind {
    #[serde(rename = "kind/llm")]
    LLM,
    #[serde(rename = "kind/image")]
    Image,
    #[serde(rename = "kind/tts")]
    TTS,
}

// ─────────────────────── manifest.json ──────────────────

/// manifest.json 反序列化目标。
///
/// 结构：`meta` 是共有元信息，其余字段按 `meta.kind` 类型不同而不同，
/// 通过 `#[serde(flatten)]` 收集到 `ext` 中延迟解析。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PluginManifest {
    pub meta: PluginInfoMeta,

    #[serde(flatten)]
    pub ext: serde_json::Value,
}

/// 所有插件共有的元信息。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct PluginInfoMeta {
    pub id: String,
    pub version: String,
    pub author: String,
    pub abi_version: u32,
    pub name: String,
    pub description: String,
    pub kind: PluginKind,
    pub url: String,
}

impl PluginManifest {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

// ─────────────────────── PluginMeta（运行时）────────────

/// 运行时完整插件元数据。
#[derive(Debug, Clone)]
pub struct PluginMeta {
    pub id: String,
    pub name: String,
    pub description: String,
    pub author: String,
    pub version: String,
    pub kind: PluginKind,
    pub url: String,
    pub fcplug_path: PathBuf,

    pub spec: PluginSpec,
}

#[derive(Debug, Clone)]
pub enum PluginSpec {
    LLM(LLMInfo),
    Image(ImageInfo),
    TTS(TTSInfo),
}

impl PluginMeta {
    pub fn from_manifest(
        manifest: PluginManifest,
        fcplug_path: PathBuf,
    ) -> Result<Self, serde_json::Error> {
        let spec = match manifest.meta.kind {
            PluginKind::LLM => {
                PluginSpec::LLM(serde_json::from_value(manifest.ext.clone())?)
            }
            PluginKind::TTS => {
                PluginSpec::TTS(serde_json::from_value(manifest.ext.clone())?)
            }
            PluginKind::Image => {
                PluginSpec::Image(serde_json::from_value(manifest.ext.clone())?)
            }
        };

        Ok(Self {
            id: manifest.meta.id,
            name: manifest.meta.name,
            description: manifest.meta.description,
            author: manifest.meta.author,
            version: manifest.meta.version,
            kind: manifest.meta.kind,
            url: manifest.meta.url,
            fcplug_path,
            spec,
        })
    }

    pub fn as_llm(&self) -> Option<&LLMInfo> {
        match &self.spec { PluginSpec::LLM(i) => Some(i), _ => None }
    }

    pub fn as_tts(&self) -> Option<&TTSInfo> {
        match &self.spec { PluginSpec::TTS(i) => Some(i), _ => None }
    }

    pub fn as_image(&self) -> Option<&ImageInfo> {
        match &self.spec { PluginSpec::Image(i) => Some(i), _ => None }
    }

    pub fn models(&self) -> &[String] {
        match &self.spec {
            PluginSpec::LLM(i) => &i.models,
            PluginSpec::TTS(i) => &i.models,
            PluginSpec::Image(i) => &i.models,
        }
    }

    pub fn default_model(&self) -> Option<&str> {
        match &self.spec {
            PluginSpec::LLM(i) => i.default_model.as_deref(),
            PluginSpec::TTS(i) => i.default_model.as_deref(),
            PluginSpec::Image(i) => i.default_model.as_deref(),
        }
    }
}

// ─────────────────────── LLMInfo ────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct LLMInfo {
    #[serde(default)]
    pub models: Vec<String>,

    #[serde(default)]
    pub default_model: Option<String>,

    #[serde(default)]
    pub supports_thinking: bool,

    #[serde(default)]
    pub supports_tools: bool,

    #[serde(default = "default_true")]
    pub supports_stream: bool,

    #[serde(default)]
    pub max_tokens: Option<u64>,
}

// ─────────────────────── TTSInfo ────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct TTSInfo {
    #[serde(default)]
    pub models: Vec<String>,

    #[serde(default)]
    pub voices: Vec<VoiceInfo>,

    #[serde(default)]
    pub default_model: Option<String>,

    #[serde(default)]
    pub default_voice: Option<String>,

    #[serde(default)]
    pub supported_formats: Vec<String>,

    #[serde(default)]
    pub supported_languages: Vec<String>,

    #[serde(default)]
    pub max_characters: Option<u64>,

    #[serde(default)]
    pub supports_emotion: bool,

    #[serde(default)]
    pub supports_voice_modify: bool,

    #[serde(default)]
    pub supports_ssml: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VoiceInfo {
    pub id: String,
    pub name: String,

    #[serde(default)]
    pub language: Vec<String>,

    #[serde(default)]
    pub gender: Option<String>,

    #[serde(default)]
    pub description: Option<String>,

    #[serde(default)]
    pub preview_url: Option<String>,
}

// ─────────────────────── ImageInfo ──────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ImageInfo {
    #[serde(default)]
    pub models: Vec<String>,

    #[serde(default)]
    pub default_model: Option<String>,

    #[serde(default)]
    pub supported_sizes: Vec<String>,

    #[serde(default)]
    pub supported_formats: Vec<String>,

    #[serde(default)]
    pub max_prompt_length: Option<u64>,

    #[serde(default)]
    pub supports_negative_prompt: bool,

    #[serde(default)]
    pub supports_image_to_image: bool,

    #[serde(default)]
    pub max_batch_size: Option<u32>,
}

// ─────────────────────── 辅助 ───────────────────────────

fn default_true() -> bool { true }