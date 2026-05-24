//! 客户端统一错误类型。
//!
//! 设计目标：
//! - 错误码（[`ErrorCode`]）与人类可读信息（`message`）分离，附加任意结构化 `detail`。
//! - [`ClientError`] 的 `Display` 输出 JSON 字符串，使现有 `format!("{:#}", e)` 链路
//!   （日志、SessionEvent::Error 兼容期、Tauri map_err 兜底）自然得到结构化文本。
//! - 仍可被 `anyhow::Error` 包装，库内多数返回签名 `anyhow::Result<T>` 保持不变；
//!   边界层通过 `error.downcast_ref::<ClientError>()` 还原结构化错误。
//!
//! 约定：构造错误时只能从已声明的 [`ErrorCode`] 中选择；扩展请同步本文件与下游文档。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

macro_rules! define_error_codes {
    ( $( $variant:ident => $wire:literal ),* $(,)? ) => {
        /// 错误码枚举。线缆格式为 SCREAMING_SNAKE_CASE 字符串，见各变体注释。
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum ErrorCode {
            $( $variant, )*
        }

        impl ErrorCode {
            /// 返回线缆使用的稳定字符串值。
            pub fn as_str(&self) -> &'static str {
                match self {
                    $( Self::$variant => $wire, )*
                }
            }

            /// 按线缆字符串解析回枚举。
            pub fn from_wire(s: &str) -> Option<Self> {
                match s {
                    $( $wire => Some(Self::$variant), )*
                    _ => None,
                }
            }
        }

        impl Serialize for ErrorCode {
            fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
                ser.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for ErrorCode {
            fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
                let s = <String as Deserialize>::deserialize(de)?;
                Self::from_wire(&s).ok_or_else(|| {
                    serde::de::Error::custom(format!("未知错误码: {}", s))
                })
            }
        }
    };
}

define_error_codes! {
    // ── 核心客户端 ────────────────────────────────────────────
    CoreClientInitFailed     => "CORE_CLIENT_INIT_FAILED",
    CoreClientConfigInvalid  => "CORE_CLIENT_CONFIG_INVALID",
    CoreClientInternalError  => "CORE_CLIENT_INTERNAL_ERROR",
    CoreClientTimeout        => "CORE_CLIENT_TIMEOUT",
    CoreClientCancelled      => "CORE_CLIENT_CANCELLED",

    // ── 插件 ──────────────────────────────────────────────────
    PluginNotFound           => "PLUGIN_NOT_FOUND",
    PluginNotLoaded          => "PLUGIN_NOT_LOADED",
    PluginLoadFailed         => "PLUGIN_LOAD_FAILED",
    PluginUnloadForbidden    => "PLUGIN_UNLOAD_FORBIDDEN",
    PluginAlreadyExists      => "PLUGIN_ALREADY_EXISTS",
    PluginKindMismatch       => "PLUGIN_KIND_MISMATCH",
    PluginVersionMismatch    => "PLUGIN_VERSION_MISMATCH",
    PluginManifestInvalid    => "PLUGIN_MANIFEST_INVALID",
    PluginRuntimeError       => "PLUGIN_RUNTIME_ERROR",
    PluginToolNotAvailable   => "PLUGIN_TOOL_NOT_AVAILABLE",

    // ── LLM 会话 ──────────────────────────────────────────────
    LlmSessionCreateFailed   => "LLM_SESSION_CREATE_FAILED",
    LlmSessionNotFound       => "LLM_SESSION_NOT_FOUND",
    LlmSessionClosed         => "LLM_SESSION_CLOSED",
    LlmSessionBusy           => "LLM_SESSION_BUSY",
    LlmMessageInvalid        => "LLM_MESSAGE_INVALID",
    LlmRequestBadPayload     => "LLM_REQUEST_BAD_PAYLOAD",
    LlmRequestTimeout        => "LLM_REQUEST_TIMEOUT",
    LlmRequestRateLimited    => "LLM_REQUEST_RATE_LIMITED",
    LlmRequestNetworkError   => "LLM_REQUEST_NETWORK_ERROR",
    LlmResponseBadStatus     => "LLM_RESPONSE_BAD_STATUS",
    LlmResponseParseError    => "LLM_RESPONSE_PARSE_ERROR",
    LlmResponseEmpty         => "LLM_RESPONSE_EMPTY",
    LlmStreamProtocolError   => "LLM_STREAM_PROTOCOL_ERROR",
    LlmToolCallFailed        => "LLM_TOOL_CALL_FAILED",
    LlmToolCallTimeout       => "LLM_TOOL_CALL_TIMEOUT",
    LlmToolCallInvalid       => "LLM_TOOL_CALL_INVALID",

    // ── 图像 ──────────────────────────────────────────────────
    ImageTaskFailed          => "IMAGE_TASK_FAILED",
    ImageTaskInvalidParams   => "IMAGE_TASK_INVALID_PARAMS",
    ImageTaskEmptyResponse   => "IMAGE_TASK_EMPTY_RESPONSE",
    ImageResponseBlocked     => "IMAGE_RESPONSE_BLOCKED",

    // ── TTS ───────────────────────────────────────────────────
    TtsTaskFailed            => "TTS_TASK_FAILED",
    TtsVoiceNotFound         => "TTS_VOICE_NOT_FOUND",
    TtsTextInvalid           => "TTS_TEXT_INVALID",
    TtsAudioTooLong          => "TTS_AUDIO_TOO_LONG",
    TtsFileWriteFailed       => "TTS_FILE_WRITE_FAILED",
    TtsResponseEmpty         => "TTS_RESPONSE_EMPTY",

    // ── 音频解码 / 播放 ───────────────────────────────────────
    AudioDecodeFailed        => "AUDIO_DECODE_FAILED",
    AudioPlaybackFailed      => "AUDIO_PLAYBACK_FAILED",
    AudioDeviceUnavailable   => "AUDIO_DEVICE_UNAVAILABLE",

    // ── HTTP 传输层 ───────────────────────────────────────────
    HttpBadRequest           => "HTTP_BAD_REQUEST",
    HttpUnauthorized         => "HTTP_UNAUTHORIZED",
    HttpNotFound             => "HTTP_NOT_FOUND",
    HttpTooManyRequests      => "HTTP_TOO_MANY_REQUESTS",
    HttpServerError          => "HTTP_SERVER_ERROR",
    HttpTimeout              => "HTTP_TIMEOUT",
    HttpEmptyResponse        => "HTTP_EMPTY_RESPONSE",

    // ── 文件系统 ──────────────────────────────────────────────
    FsOpenFailed             => "FS_OPEN_FAILED",
    FsPermissionDenied       => "FS_PERMISSION_DENIED",
    FsWriteFailed            => "FS_WRITE_FAILED",

    // ── 鉴权 ──────────────────────────────────────────────────
    AuthApiKeyMissing        => "AUTH_API_KEY_MISSING",
    AuthKeyInvalid           => "AUTH_KEY_INVALID",

    // ── 参数校验 ──────────────────────────────────────────────
    ValidationMissingField   => "VALIDATION_MISSING_FIELD",
    ValidationFormatError    => "VALIDATION_FORMAT_ERROR",

    // ── 工具调用 (host 侧) ────────────────────────────────────
    ToolNotFound             => "TOOL_NOT_FOUND",
    ToolDisabled             => "TOOL_DISABLED",

    // ── 兜底 ──────────────────────────────────────────────────
    NotImplemented           => "NOT_IMPLEMENTED",
}

/// 结构化错误对象。
///
/// - `code`：稳定错误码，供上层程序分支。
/// - `message`：默认中文展示文案。
/// - `detail`：可选附加字段，例如 `plugin_id`、`status_code`、`request_id`、`retry_after_ms` 等。
///
/// `Display` 实现输出整对象的 JSON 字符串，因此 `format!("{}", err)` 与 `format!("{:#}", err)`
/// （无 anyhow context 链时）都会产生上层可解析的 JSON。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub detail: Value,
}

impl ClientError {
    /// 构造一个新错误。`message` 建议使用中文。
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: Value::Null,
        }
    }

    /// 用整个 `serde_json::Value` 覆盖 `detail`。
    pub fn with_detail(mut self, detail: Value) -> Self {
        self.detail = detail;
        self
    }

    /// 向 `detail` 写入一个键值；若 `detail` 当前不是对象会被替换为新对象。
    pub fn with_kv(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        if !self.detail.is_object() {
            self.detail = Value::Object(serde_json::Map::new());
        }
        if let Value::Object(map) = &mut self.detail {
            map.insert(key.into(), value.into());
        }
        self
    }

    /// 兜底构造：未分类的内部错误。
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::CoreClientInternalError, message)
    }

    /// 序列化为 JSON 字符串。失败时退回到 `message`，保证总有返回。
    pub fn to_json_string(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| self.message.clone())
    }

    /// 从 `anyhow::Error` 尝试 downcast 出 `ClientError` 引用。
    pub fn from_anyhow(err: &anyhow::Error) -> Option<&ClientError> {
        err.downcast_ref::<ClientError>()
    }

    /// 将任意 `anyhow::Error` 还原为 `ClientError`：
    /// 若内层已是 `ClientError` 直接 clone，否则用
    /// `CoreClientInternalError` 包装 `Display` 文本。
    pub fn from_anyhow_owned(err: anyhow::Error) -> Self {
        if let Some(ce) = err.downcast_ref::<ClientError>() {
            ce.clone()
        } else {
            Self::new(ErrorCode::CoreClientInternalError, err.to_string())
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_json_string())
    }
}

impl std::error::Error for ClientError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 错误码序列化为线缆字符串() {
        let v = serde_json::to_value(ErrorCode::PluginNotFound).unwrap();
        assert_eq!(v, json!("PLUGIN_NOT_FOUND"));
    }

    #[test]
    fn 错误码可从字符串反序列化() {
        let c: ErrorCode = serde_json::from_str("\"LLM_SESSION_NOT_FOUND\"").unwrap();
        assert_eq!(c, ErrorCode::LlmSessionNotFound);
    }

    #[test]
    fn display_输出_json_字符串() {
        let err = ClientError::new(ErrorCode::PluginNotFound, "插件不存在")
            .with_kv("plugin_id", "deepseek");
        let text = format!("{}", err);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["code"], "PLUGIN_NOT_FOUND");
        assert_eq!(parsed["message"], "插件不存在");
        assert_eq!(parsed["detail"]["plugin_id"], "deepseek");
    }

    #[test]
    fn detail_为空时不序列化() {
        let err = ClientError::new(ErrorCode::CoreClientInternalError, "x");
        let text = serde_json::to_string(&err).unwrap();
        assert!(!text.contains("detail"));
    }

    #[test]
    fn 可被_anyhow_包装并_downcast_回结构化错误() {
        let err: anyhow::Error =
            ClientError::new(ErrorCode::LlmSessionNotFound, "会话不存在").into();
        let recovered = ClientError::from_anyhow(&err).expect("应能 downcast");
        assert_eq!(recovered.code, ErrorCode::LlmSessionNotFound);
    }

    #[test]
    fn anyhow_display_保持_json_格式() {
        let err: anyhow::Error =
            ClientError::new(ErrorCode::PluginLoadFailed, "插件加载失败").into();
        let text = format!("{:#}", err);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["code"], "PLUGIN_LOAD_FAILED");
    }
}
