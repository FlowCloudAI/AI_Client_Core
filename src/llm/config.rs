// llm/config.rs——会话配置

use anyhow::Result;
use std::fmt;
use zeroize::Zeroize;

use crate::error::{ClientError, ErrorCode};

#[derive(Clone, Default, Eq, PartialEq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(\"***\")")
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// API 端点
    pub base_url: String,

    /// API 密钥
    pub api_key: SecretString,

    /// 事件 channel 缓冲大小
    pub event_buffer: usize,

    /// 单次请求超时（秒），0 表示不限制
    pub request_timeout: u64,

    /// 工具调用最大连续轮数（防无限循环）
    pub max_tool_rounds: usize,

    /// 流式响应单行最大字节数（防异常数据撑爆内存），0 表示不限制
    pub max_line_bytes: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: SecretString::default(),
            event_buffer: 256,
            request_timeout: 120,
            max_tool_rounds: 20,
            max_line_bytes: 1024 * 512, // 512KB
        }
    }
}

impl SessionConfig {
    pub fn validate(&self) -> Result<()> {
        if self.base_url.is_empty() {
            return Err(
                ClientError::new(ErrorCode::ValidationMissingField, "base_url 不能为空")
                    .with_kv("field", "base_url")
                    .into(),
            );
        }
        if !self.base_url.starts_with("http") {
            return Err(ClientError::new(
                ErrorCode::ValidationFormatError,
                "base_url 必须以 http 开头",
            )
            .with_kv("field", "base_url")
            .with_kv("value", self.base_url.clone())
            .into());
        }
        if self.api_key.is_empty() {
            return Err(
                ClientError::new(ErrorCode::AuthApiKeyMissing, "api_key 不能为空")
                    .with_kv("field", "api_key")
                    .into(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{SecretString, SessionConfig};

    #[test]
    fn session_config_debug_redacts_api_key() {
        let config = SessionConfig {
            base_url: "https://example.test".to_string(),
            api_key: SecretString::from("sk-test-secret"),
            ..SessionConfig::default()
        };

        let output = format!("{config:?}");
        assert!(output.contains("api_key"));
        assert!(output.contains("***"));
        assert!(!output.contains("sk-test-secret"));
    }

    #[test]
    fn secret_string_exposes_value_for_request_auth() {
        let secret = SecretString::from("sk-test-secret");
        assert_eq!(secret.expose(), "sk-test-secret");
        assert!(!secret.is_empty());
    }
}
