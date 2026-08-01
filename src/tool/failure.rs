//! 工具执行失败的稳定分类与模型提示。
//!
//! 分类只决定重试、安全拒绝和本轮禁用策略；具体工具仍通过 `anyhow` 返回原始错误，
//! 以兼容现有注册接口和上层工具实现。

use crate::error::{ClientError, ErrorCode};
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolFailure {
    /// 网络、超时、限流和服务端故障，可由执行层短暂重试。
    Transient { retry_after_ms: Option<u64> },
    /// 用户或安全策略拒绝，绝不允许模型自动重试。
    Denied { reason: String },
    /// 参数或业务逻辑错误，模型可根据提示修正后重新规划。
    Recoverable { hint: String },
    /// 工具不存在或内部状态损坏，本轮不应再次向模型暴露。
    Fatal,
}

impl ToolFailure {
    pub fn classify(error: &anyhow::Error) -> Self {
        if let Some(failure) = error.downcast_ref::<Self>() {
            return failure.clone();
        }
        if let Some(error) = ClientError::from_anyhow(error) {
            let retryable = error
                .detail
                .get("retryable")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                || matches!(
                    error.code,
                    ErrorCode::CoreClientTimeout
                        | ErrorCode::LlmRequestTimeout
                        | ErrorCode::LlmRequestNetworkError
                        | ErrorCode::LlmToolCallTimeout
                        | ErrorCode::HttpTooManyRequests
                        | ErrorCode::HttpServerError
                        | ErrorCode::HttpTimeout
                );
            if retryable {
                return Self::Transient {
                    retry_after_ms: error
                        .detail
                        .get("retry_after_ms")
                        .and_then(serde_json::Value::as_u64),
                };
            }
            if matches!(
                error.code,
                ErrorCode::ToolNotFound
                    | ErrorCode::ToolDisabled
                    | ErrorCode::CoreClientInternalError
            ) {
                return Self::Fatal;
            }
        }
        if let Some(error) = error.downcast_ref::<reqwest::Error>()
            && (error.is_timeout()
                || error.is_connect()
                || error.status().is_some_and(|status| {
                    status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error()
                }))
        {
            return Self::Transient {
                retry_after_ms: None,
            };
        }
        Self::Recoverable {
            hint: error.to_string(),
        }
    }

    pub fn model_message(&self) -> String {
        match self {
            Self::Transient { .. } => {
                "工具因超时或暂时性故障未完成（已重试 2 次）。请向用户说明当前故障，不要假装工具已经成功。"
                    .to_string()
            }
            Self::Denied { reason } => format!(
                "用户拒绝了此操作。不要重试或改写参数再次尝试。请向用户说明并等待新指示。原因：{reason}"
            ),
            Self::Recoverable { hint } => format!("工具执行失败: {hint}"),
            Self::Fatal => {
                "工具不可用，已从本轮后续工具集中移除。请使用已有信息继续回答。".to_string()
            }
        }
    }
}

impl fmt::Display for ToolFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.model_message())
    }
}

impl std::error::Error for ToolFailure {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_client_error_maps_to_transient() {
        let error: anyhow::Error = ClientError::new(ErrorCode::HttpServerError, "服务异常")
            .with_kv("retryable", true)
            .with_kv("retry_after_ms", 250_u64)
            .into();
        assert_eq!(
            ToolFailure::classify(&error),
            ToolFailure::Transient {
                retry_after_ms: Some(250)
            }
        );
        assert!(
            ToolFailure::classify(&error)
                .model_message()
                .contains("超时")
        );
    }

    #[test]
    fn denied_message_explicitly_forbids_retry() {
        let failure = ToolFailure::Denied {
            reason: "用户取消".to_string(),
        };
        assert!(failure.model_message().contains("不要重试"));
    }
}
