use anyhow::Result;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::PathBuf;

use crate::SUPPORTED_AGREEMENT_VERSION;
use crate::error::{ClientError, ErrorCode};

// ─────────────────────── 插件类型 ─────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginKind {
    #[serde(rename = "llm")]
    LLM,
    Image,
    #[serde(rename = "tts")]
    TTS,
}

// ─────────────────────── manifest.json 解析 ─────────────

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
    pub agreement_version: u32,
    pub name: String,
    pub description: String,
    pub kind: PluginKind,
    pub url: String,
}

impl PluginManifest {
    pub fn parse(json: &str) -> Result<Self> {
        let raw: serde_json::Value = serde_json::from_str(json).map_err(|e| {
            ClientError::new(ErrorCode::PluginManifestInvalid, "manifest JSON 解析失败")
                .with_kv("source", e.to_string())
        })?;
        if raw.get("meta").and_then(|m| m.get("abi-version")).is_some()
            && raw
                .get("meta")
                .and_then(|m| m.get("agreement-version"))
                .is_none()
        {
            return Err(ClientError::new(
                ErrorCode::PluginVersionMismatch,
                "manifest 使用旧的 abi-version，请运行 `cargo fcplug update` 迁移",
            )
            .into());
        }

        let manifest: Self = serde_json::from_value(raw).map_err(|e| {
            ClientError::new(ErrorCode::PluginManifestInvalid, "manifest 结构不合法")
                .with_kv("source", e.to_string())
        })?;
        manifest.validate_meta()?;
        Ok(manifest)
    }

    fn validate_meta(&self) -> Result<()> {
        validate_plugin_id(&self.meta.id)?;
        validate_required("meta.version", &self.meta.version)?;
        validate_required("meta.author", &self.meta.author)?;
        validate_required("meta.name", &self.meta.name)?;
        validate_required("meta.description", &self.meta.description)?;
        if self.meta.agreement_version != SUPPORTED_AGREEMENT_VERSION {
            return Err(ClientError::new(
                ErrorCode::PluginVersionMismatch,
                format!(
                    "插件 '{}' 协议版本不匹配：期望 {}，实际 {}",
                    self.meta.id, SUPPORTED_AGREEMENT_VERSION, self.meta.agreement_version
                ),
            )
            .with_kv("plugin_id", self.meta.id.clone())
            .with_kv("expected", SUPPORTED_AGREEMENT_VERSION)
            .with_kv("actual", self.meta.agreement_version)
            .into());
        }
        validate_url_policy(&self.meta.url)?;
        Ok(())
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
    model_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum PluginSpec {
    LLM(LLMInfo),
    Image(ImageInfo),
    TTS(TTSInfo),
}

impl PluginMeta {
    pub fn from_manifest(manifest: PluginManifest, fcplug_path: PathBuf) -> Result<Self> {
        let kind = manifest.meta.kind.clone();
        let map_ext_err = |e: serde_json::Error| {
            ClientError::new(ErrorCode::PluginManifestInvalid, "manifest 扩展段解析失败")
                .with_kv("plugin_id", manifest.meta.id.clone())
                .with_kv("source", e.to_string())
        };
        let spec = match kind {
            PluginKind::LLM => {
                PluginSpec::LLM(serde_json::from_value(manifest.ext.clone()).map_err(map_ext_err)?)
            }
            PluginKind::TTS => {
                PluginSpec::TTS(serde_json::from_value(manifest.ext.clone()).map_err(map_ext_err)?)
            }
            PluginKind::Image => PluginSpec::Image(
                serde_json::from_value(manifest.ext.clone()).map_err(map_ext_err)?,
            ),
        };

        spec.validate()?;
        let model_ids = spec.model_ids();

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
            model_ids,
        })
    }

    pub fn as_llm(&self) -> Option<&LLMInfo> {
        match &self.spec {
            PluginSpec::LLM(i) => Some(i),
            _ => None,
        }
    }

    pub fn as_tts(&self) -> Option<&TTSInfo> {
        match &self.spec {
            PluginSpec::TTS(i) => Some(i),
            _ => None,
        }
    }

    pub fn as_image(&self) -> Option<&ImageInfo> {
        match &self.spec {
            PluginSpec::Image(i) => Some(i),
            _ => None,
        }
    }

    pub fn models(&self) -> &[String] {
        &self.model_ids
    }

    /// 返回插件声明的完整模型元数据。
    pub fn model_infos(&self) -> &[ModelInfo] {
        match &self.spec {
            PluginSpec::LLM(i) => &i.models,
            PluginSpec::TTS(i) => &i.models,
            PluginSpec::Image(i) => &i.models,
        }
    }

    /// 按模型 ID 查询完整模型元数据。
    pub fn model_info(&self, model_id: &str) -> Option<&ModelInfo> {
        self.model_infos().iter().find(|model| model.id == model_id)
    }

    pub fn default_model(&self) -> Option<&str> {
        match &self.spec {
            PluginSpec::LLM(i) => i.default_model.as_deref(),
            PluginSpec::TTS(i) => i.default_model.as_deref(),
            PluginSpec::Image(i) => i.default_model.as_deref(),
        }
    }

    /// 校验本次 LLM 请求是否符合当前模型声明能力。
    pub fn validate_llm_request(
        &self,
        model: &str,
        thinking_effort: Option<ThinkingEffort>,
    ) -> Result<()> {
        let Some(info) = self.as_llm() else {
            return Err(ClientError::new(
                ErrorCode::PluginKindMismatch,
                format!("插件 '{}' 不是 LLM 插件", self.id),
            )
            .with_kv("plugin_id", self.id.clone())
            .with_kv("expected_kind", "llm")
            .into());
        };
        info.validate_request(model, thinking_effort)
    }
}

impl PluginSpec {
    fn validate(&self) -> Result<()> {
        match self {
            PluginSpec::LLM(info) => info.validate(),
            PluginSpec::Image(info) => info.validate(),
            PluginSpec::TTS(info) => info.validate(),
        }
    }

    fn model_ids(&self) -> Vec<String> {
        match self {
            PluginSpec::LLM(info) => info.models.iter().map(|m| m.id.clone()).collect(),
            PluginSpec::Image(info) => info.models.iter().map(|m| m.id.clone()).collect(),
            PluginSpec::TTS(info) => info.models.iter().map(|m| m.id.clone()).collect(),
        }
    }
}

// ─────────────────────── 通用模型能力 ─────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct ModelInfo {
    pub id: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,

    #[serde(default, skip_serializing_if = "SupportsPatch::is_empty")]
    pub supports: SupportsPatch,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_efforts: Option<Vec<ThinkingEffort>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Supports {
    #[serde(default)]
    pub thinking: bool,

    #[serde(default)]
    pub tools: bool,

    #[serde(default = "default_true")]
    pub stream: bool,

    #[serde(default)]
    pub vision_input: bool,

    #[serde(default)]
    pub structured_output: bool,
}

impl Default for Supports {
    fn default() -> Self {
        Self {
            thinking: false,
            tools: false,
            stream: true,
            vision_input: false,
            structured_output: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct SupportsPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision_input: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<bool>,
}

impl SupportsPatch {
    pub fn is_empty(&self) -> bool {
        self.thinking.is_none()
            && self.tools.is_none()
            && self.stream.is_none()
            && self.vision_input.is_none()
            && self.structured_output.is_none()
    }

    pub fn apply_to(&self, base: &Supports) -> Supports {
        Supports {
            thinking: self.thinking.unwrap_or(base.thinking),
            tools: self.tools.unwrap_or(base.tools),
            stream: self.stream.unwrap_or(base.stream),
            vision_input: self.vision_input.unwrap_or(base.vision_input),
            structured_output: self.structured_output.unwrap_or(base.structured_output),
        }
    }
}

// ─────────────────────── LLM 信息 ────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct LLMInfo {
    #[serde(default)]
    pub models: Vec<ModelInfo>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,

    #[serde(default)]
    pub default_supports: Supports,

    #[serde(default)]
    pub default_thinking_efforts: Vec<ThinkingEffort>,
}

impl LLMInfo {
    fn validate(&self) -> Result<()> {
        validate_models(&self.models)?;
        validate_default_model(self.default_model.as_deref(), &self.models)?;
        validate_unique_efforts("default-thinking-efforts", &self.default_thinking_efforts)?;

        for model in &self.models {
            validate_positive("context-window-tokens", model.context_window_tokens)?;
            validate_positive("max-output-tokens", model.max_output_tokens)?;
            if let Some(efforts) = &model.thinking_efforts {
                validate_unique_efforts(
                    &format!("models[{}].thinking-efforts", model.id),
                    efforts,
                )?;
            }

            let supports = model.supports.apply_to(&self.default_supports);
            let efforts = self.final_thinking_efforts_for_model(model);
            if !supports.thinking && !efforts.is_empty() {
                return Err(ClientError::new(
                    ErrorCode::PluginManifestInvalid,
                    format!(
                        "模型 '{}' 声明了 thinking-efforts 但 supports.thinking 为 false",
                        model.id
                    ),
                )
                .with_kv("model_id", model.id.clone())
                .into());
            }
        }

        Ok(())
    }

    pub fn validate_request(
        &self,
        model_id: &str,
        thinking_effort: Option<ThinkingEffort>,
    ) -> Result<()> {
        let model = self
            .models
            .iter()
            .find(|m| m.id == model_id)
            .ok_or_else(|| {
                ClientError::new(
                    ErrorCode::ValidationFormatError,
                    format!("插件未声明模型 '{}'", model_id),
                )
                .with_kv("model_id", model_id.to_string())
            })?;

        if let Some(effort) = thinking_effort {
            let efforts = self.final_thinking_efforts_for_model(model);
            if !efforts.contains(&effort) {
                return Err(ClientError::new(
                    ErrorCode::ValidationFormatError,
                    format!("模型 '{}' 不支持 thinking_effort {:?}", model_id, effort),
                )
                .with_kv("model_id", model_id.to_string())
                .with_kv("thinking_effort", format!("{:?}", effort))
                .into());
            }
        }

        Ok(())
    }

    fn final_thinking_efforts_for_model(&self, model: &ModelInfo) -> Vec<ThinkingEffort> {
        let supports = model.supports.apply_to(&self.default_supports);
        if !supports.thinking {
            return Vec::new();
        }
        model
            .thinking_efforts
            .clone()
            .unwrap_or_else(|| self.default_thinking_efforts.clone())
    }
}

// ─────────────────────── TTS 信息 ────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct TTSInfo {
    #[serde(default)]
    pub models: Vec<ModelInfo>,

    #[serde(default)]
    pub voices: Vec<VoiceInfo>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_voice: Option<String>,

    #[serde(default)]
    pub supported_formats: Vec<String>,

    #[serde(default)]
    pub supported_languages: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_characters: Option<u64>,

    #[serde(default)]
    pub supports_emotion: bool,

    #[serde(default)]
    pub supports_voice_modify: bool,

    #[serde(default)]
    pub supports_ssml: bool,
}

impl TTSInfo {
    fn validate(&self) -> Result<()> {
        validate_models(&self.models)?;
        validate_default_model(self.default_model.as_deref(), &self.models)?;
        if self.voices.is_empty() {
            return Err(ClientError::new(
                ErrorCode::PluginManifestInvalid,
                "TTS 插件的 voices 不能为空",
            )
            .into());
        }

        let mut seen = BTreeSet::new();
        for voice in &self.voices {
            validate_required("voices[].id", &voice.id)?;
            validate_required("voices[].name", &voice.name)?;
            if !seen.insert(voice.id.as_str()) {
                return Err(ClientError::new(
                    ErrorCode::PluginManifestInvalid,
                    format!("voice id 重复: {}", voice.id),
                )
                .with_kv("voice_id", voice.id.clone())
                .into());
            }
        }

        if let Some(default_voice) = &self.default_voice {
            if !self.voices.iter().any(|voice| voice.id == *default_voice) {
                return Err(ClientError::new(
                    ErrorCode::PluginManifestInvalid,
                    "default-voice 必须匹配某个已声明的 voice id",
                )
                .with_kv("default_voice", default_voice.clone())
                .into());
            }
        }

        validate_positive("max-characters", self.max_characters)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct VoiceInfo {
    pub id: String,
    pub name: String,

    #[serde(default)]
    pub language: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gender: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_url: Option<String>,
}

// ─────────────────────── 图像信息 ────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct ImageInfo {
    #[serde(default)]
    pub models: Vec<ModelInfo>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,

    #[serde(default)]
    pub supported_sizes: Vec<String>,

    #[serde(default)]
    pub supported_formats: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_prompt_length: Option<u64>,

    #[serde(default)]
    pub supports_negative_prompt: bool,

    #[serde(default)]
    pub supports_image_to_image: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_batch_size: Option<u32>,
}

impl ImageInfo {
    fn validate(&self) -> Result<()> {
        validate_models(&self.models)?;
        validate_default_model(self.default_model.as_deref(), &self.models)?;
        validate_positive("max-prompt-length", self.max_prompt_length)?;
        if self.max_batch_size == Some(0) {
            return Err(ClientError::new(
                ErrorCode::PluginManifestInvalid,
                "max-batch-size 必须大于 0",
            )
            .into());
        }
        Ok(())
    }
}

// ─────────────────────── 辅助 ───────────────────────────

fn default_true() -> bool {
    true
}

fn manifest_invalid(message: impl Into<String>) -> ClientError {
    ClientError::new(ErrorCode::PluginManifestInvalid, message)
}

fn validate_required(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(manifest_invalid(format!("{} 不能为空", label))
            .with_kv("field", label.to_string())
            .into());
    }
    Ok(())
}

fn validate_plugin_id(id: &str) -> Result<()> {
    validate_required("meta.id", id)?;
    if id.len() > 64 {
        return Err(manifest_invalid("meta.id 过长，最长 64 字符").into());
    }
    if !id.starts_with(|c: char| c.is_ascii_lowercase()) {
        return Err(manifest_invalid("meta.id 必须以小写 ASCII 字母开头").into());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(manifest_invalid("meta.id 仅允许小写 ASCII 字母、数字和短横线").into());
    }
    if id.ends_with('-') || id.contains("--") {
        return Err(manifest_invalid("meta.id 不能以短横线结尾或包含连续短横线").into());
    }
    Ok(())
}

fn validate_url_policy(raw: &str) -> Result<()> {
    let parsed = Url::parse(raw).map_err(|e| {
        manifest_invalid(format!("无效 URL '{}': {}", raw, e))
            .with_kv("url", raw.to_string())
            .with_kv("source", e.to_string())
    })?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" => {
            let host = parsed
                .host_str()
                .ok_or_else(|| manifest_invalid("HTTP URL 必须包含 host"))?;
            if host.eq_ignore_ascii_case("localhost") {
                return Ok(());
            }
            if let Ok(ip) = host.parse::<IpAddr>() {
                if ip.is_loopback() {
                    return Ok(());
                }
            }
            Err(manifest_invalid("HTTP 仅允许指向 localhost / loopback 端点").into())
        }
        scheme => Err(
            manifest_invalid(format!("不支持的 URL scheme '{}'", scheme))
                .with_kv("scheme", scheme.to_string())
                .into(),
        ),
    }
}

fn validate_models(models: &[ModelInfo]) -> Result<()> {
    if models.is_empty() {
        return Err(manifest_invalid("models 不能为空").into());
    }
    let mut seen = BTreeSet::new();
    for model in models {
        validate_required("models[].id", &model.id)?;
        if !seen.insert(model.id.as_str()) {
            return Err(manifest_invalid(format!("模型 id 重复: {}", model.id))
                .with_kv("model_id", model.id.clone())
                .into());
        }
    }
    Ok(())
}

fn validate_default_model(default_model: Option<&str>, models: &[ModelInfo]) -> Result<()> {
    if let Some(default_model) = default_model {
        if !models.iter().any(|model| model.id == default_model) {
            return Err(
                manifest_invalid("default-model 必须匹配某个已声明的 model id")
                    .with_kv("default_model", default_model.to_string())
                    .into(),
            );
        }
    }
    Ok(())
}

fn validate_unique_efforts(label: &str, efforts: &[ThinkingEffort]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for effort in efforts {
        if !seen.insert(*effort) {
            return Err(
                manifest_invalid(format!("{} 包含重复值 {:?}", label, effort))
                    .with_kv("field", label.to_string())
                    .into(),
            );
        }
    }
    Ok(())
}

fn validate_positive(label: &str, value: Option<u64>) -> Result<()> {
    if value == Some(0) {
        return Err(manifest_invalid(format!("{} 必须大于 0", label))
            .with_kv("field", label.to_string())
            .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn llm_manifest(extra: &str) -> String {
        format!(
            r#"{{
              "meta": {{
                "id": "example-llm",
                "version": "1.0.0",
                "author": "flowcloudai",
                "agreement-version": 1,
                "name": "Example LLM",
                "description": "Example plugin",
                "kind": "llm",
                "url": "https://api.example.com/v1/chat"
              }},
              "default-model": "model-a",
              "models": [{{ "id": "model-a" }}]
              {}
            }}"#,
            extra
        )
    }

    #[test]
    fn rejects_old_abi_manifest() {
        let raw = r#"{
          "meta": {
            "id": "old-llm",
            "version": "1.0.0",
            "author": "flowcloudai",
            "abi-version": 2,
            "name": "Old",
            "description": "Old",
            "kind": "kind/llm",
            "url": "https://api.example.com/v1/chat"
          },
          "models": ["a"]
        }"#;

        let err = PluginManifest::parse(raw).unwrap_err();
        assert!(err.to_string().contains("PLUGIN_VERSION_MISMATCH"));
    }

    #[test]
    fn parses_v1_llm_manifest() {
        let manifest = PluginManifest::parse(&llm_manifest("")).unwrap();
        let meta = PluginMeta::from_manifest(manifest, PathBuf::from("p.fcplug")).unwrap();

        assert_eq!(meta.kind, PluginKind::LLM);
        assert_eq!(meta.models(), &["model-a".to_string()]);
        assert_eq!(meta.default_model(), Some("model-a"));
        assert_eq!(meta.model_infos()[0].id, "model-a");
        assert_eq!(
            meta.model_info("model-a").map(|model| model.id.as_str()),
            Some("model-a")
        );
        assert!(meta.model_info("missing").is_none());
    }

    #[test]
    fn rejects_invalid_thinking_effort_request() {
        let manifest = PluginManifest::parse(&llm_manifest(
            r#",
              "default-supports": { "thinking": true },
              "default-thinking-efforts": ["low"]
            "#,
        ))
        .unwrap();
        let meta = PluginMeta::from_manifest(manifest, PathBuf::from("p.fcplug")).unwrap();

        let err = meta
            .validate_llm_request("model-a", Some(ThinkingEffort::High))
            .unwrap_err();
        assert!(err.to_string().contains("VALIDATION_FORMAT_ERROR"));
    }

    #[test]
    fn rejects_default_voice_not_in_voices() {
        let raw = r#"{
          "meta": {
            "id": "example-tts",
            "version": "1.0.0",
            "author": "flowcloudai",
            "agreement-version": 1,
            "name": "Example TTS",
            "description": "Example plugin",
            "kind": "tts",
            "url": "https://api.example.com/v1/tts"
          },
          "models": [{ "id": "m" }],
          "voices": [{ "id": "a", "name": "A" }],
          "default-voice": "b"
        }"#;

        let manifest = PluginManifest::parse(raw).unwrap();
        let err = PluginMeta::from_manifest(manifest, PathBuf::from("p.fcplug")).unwrap_err();
        assert!(
            err.to_string().contains("default-voice") || err.to_string().contains("default_voice")
        );
    }
}
