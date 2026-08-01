//! LLM 请求 token 的本地估算与校准。
//!
//! 这里只做供应商无关的保守估算；真实 usage 用于按会话校准，不引入特定模型 tokenizer。

use crate::llm::types::{ChatRequest, Message};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

const CJK_CHARS_PER_TOKEN: f64 = 1.5;
const OTHER_CHARS_PER_TOKEN: f64 = 3.8;
const MESSAGE_OVERHEAD_TOKENS: u64 = 4;
const EMA_OLD_WEIGHT: f64 = 0.8;
const MIN_CALIBRATION_FACTOR: f64 = 0.5;
const MAX_CALIBRATION_FACTOR: f64 = 2.0;

/// 按字符类型估算文本 token 数。
pub fn estimate_text_tokens(text: &str) -> u64 {
    if text.trim().is_empty() {
        return 0;
    }

    let (cjk, other) = text.chars().fold((0_u64, 0_u64), |(cjk, other), ch| {
        if is_cjk(ch) {
            (cjk + 1, other)
        } else {
            (cjk, other + 1)
        }
    });

    ((cjk as f64 / CJK_CHARS_PER_TOKEN) + (other as f64 / OTHER_CHARS_PER_TOKEN)).ceil() as u64
}

/// 估算消息数组，覆盖正文、思考、工具调用参数与工具结果。
pub fn estimate_messages_tokens(messages: &[Message]) -> u64 {
    messages
        .iter()
        .map(|message| {
            let mut tokens = MESSAGE_OVERHEAD_TOKENS;
            tokens += message.content.as_deref().map_or(0, estimate_text_tokens);
            tokens += message
                .reasoning_content
                .as_deref()
                .map_or(0, estimate_text_tokens);
            tokens += message
                .tool_call_id
                .as_deref()
                .map_or(0, estimate_text_tokens);
            if let Some(calls) = &message.tool_calls {
                for call in calls {
                    tokens += call.id.as_deref().map_or(0, estimate_text_tokens);
                    tokens += estimate_text_tokens(&call.function.name);
                    tokens += estimate_text_tokens(&call.function.arguments);
                }
            }
            tokens
        })
        .sum()
}

/// 估算完整请求，工具 JSON schema 与消息使用同一字符口径。
pub fn estimate_request_tokens(request: &ChatRequest) -> u64 {
    let tools = request.tools.as_deref().map_or(0, |tools| {
        tools
            .iter()
            .filter_map(|tool| serde_json::to_string(tool).ok())
            .map(|tool| estimate_text_tokens(&tool))
            .sum()
    });
    estimate_messages_tokens(&request.messages) + tools
}

/// 将模型校准系数应用到基础估算。
pub fn estimate_with_factor(estimated: u64, factor: f64) -> u64 {
    (estimated as f64 * normalize_factor(factor)).ceil() as u64
}

/// 上一次成功请求的真实 prompt token 基线。
///
/// 仅当当前消息树仍沿着该 head 向后追加，且模型与工具集未变化时使用；
/// `base_estimate` 用来抵消两次请求共有历史的本地估算误差。
#[derive(Clone, Debug)]
pub(crate) struct RequestBaseline {
    pub prompt_tokens: u64,
    pub base_estimate: u64,
    pub message_count: usize,
    pub head_node_id: u64,
    pub tools_fingerprint: u64,
    model: String,
}

impl RequestBaseline {
    pub fn new(prompt_tokens: i64, request: &ChatRequest, head_node_id: u64) -> Option<Self> {
        (prompt_tokens > 0).then(|| Self {
            prompt_tokens: prompt_tokens as u64,
            base_estimate: estimate_request_tokens(request),
            message_count: request.messages.len(),
            head_node_id,
            tools_fingerprint: tools_fingerprint(request),
            model: request.model.clone(),
        })
    }

    pub fn extends_request(
        &self,
        request: &ChatRequest,
        current_head: Option<u64>,
        head_path: &[u64],
    ) -> bool {
        request.messages.len() > self.message_count
            && current_head.is_some_and(|head| head != self.head_node_id)
            && head_path.contains(&self.head_node_id)
            && request.model == self.model
            && tools_fingerprint(request) == self.tools_fingerprint
    }

    /// 用真实历史基线加上本次请求的估算增量，避免重复估算整段长历史。
    pub fn estimate(&self, request: &ChatRequest, calibration_factor: f64) -> u64 {
        let current = estimate_request_tokens(request);
        if current >= self.base_estimate {
            self.prompt_tokens.saturating_add(estimate_with_factor(
                current - self.base_estimate,
                calibration_factor,
            ))
        } else {
            self.prompt_tokens.saturating_sub(estimate_with_factor(
                self.base_estimate - current,
                calibration_factor,
            ))
        }
    }
}

fn tools_fingerprint(request: &ChatRequest) -> u64 {
    let mut hasher = DefaultHasher::new();
    serde_json::to_vec(&request.tools)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

/// 单会话 EMA 校准器；跨会话持久化由上层按供应商与模型负责。
#[derive(Clone, Debug)]
pub struct TokenCalibrator {
    factor: f64,
}

impl TokenCalibrator {
    pub fn new(factor: Option<f64>) -> Self {
        Self {
            factor: normalize_factor(factor.unwrap_or(1.0)),
        }
    }

    pub fn factor(&self) -> f64 {
        self.factor
    }

    pub fn estimate(&self, base_estimate: u64) -> u64 {
        estimate_with_factor(base_estimate, self.factor)
    }

    /// 用供应商返回的 prompt usage 更新系数；输入无效时保持不变。
    pub fn observe(&mut self, prompt_tokens: i64, base_estimate: u64) -> Option<f64> {
        if prompt_tokens <= 0 || base_estimate == 0 {
            return None;
        }

        let observed = normalize_factor(prompt_tokens as f64 / base_estimate as f64);
        self.factor =
            normalize_factor(EMA_OLD_WEIGHT * self.factor + (1.0 - EMA_OLD_WEIGHT) * observed);
        Some(self.factor)
    }
}

impl Default for TokenCalibrator {
    fn default() -> Self {
        Self::new(None)
    }
}

fn normalize_factor(factor: f64) -> f64 {
    if factor.is_finite() {
        factor.clamp(MIN_CALIBRATION_FACTOR, MAX_CALIBRATION_FACTOR)
    } else {
        1.0
    }
}

fn is_cjk(ch: char) -> bool {
    matches!(
        ch,
        '\u{3000}'..='\u{303f}'
            | '\u{3040}'..='\u{30ff}'
            | '\u{3400}'..='\u{4dbf}'
            | '\u{4e00}'..='\u{9fff}'
            | '\u{ac00}'..='\u{d7af}'
            | '\u{f900}'..='\u{faff}'
            | '\u{ff00}'..='\u{ffef}'
            | '\u{20000}'..='\u{2fa1f}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::{ToolCall, ToolFunctionCall};

    fn assert_relative_error(estimated: u64, actual: u64, max_error: f64) {
        let error = estimated.abs_diff(actual) as f64 / actual as f64;
        assert!(
            error < max_error,
            "estimated={estimated}, actual={actual}, error={error}"
        );
    }

    #[test]
    fn language_baselines_are_within_cold_start_tolerance() {
        assert_relative_error(estimate_text_tokens(&"世".repeat(150)), 100, 0.15);
        assert_relative_error(estimate_text_tokens(&"a".repeat(380)), 100, 0.15);
        let mixed = format!("{}{}", "世".repeat(75), "a".repeat(190));
        assert_relative_error(estimate_text_tokens(&mixed), 100, 0.15);
    }

    #[test]
    fn calibration_converges_within_five_observations() {
        let mut calibrator = TokenCalibrator::default();
        for _ in 0..5 {
            calibrator.observe(110, 100);
        }
        assert_relative_error(calibrator.estimate(100), 110, 0.05);
    }

    #[test]
    fn tool_calls_increase_message_estimate() {
        let plain = Message::assistant(Some("完成"), None::<String>, None);
        let with_call = Message::assistant(
            Some("完成"),
            None::<String>,
            Some(vec![ToolCall {
                id: Some("call-1".to_string()),
                call_type: Some("function".to_string()),
                function: ToolFunctionCall {
                    name: "search_entries".to_string(),
                    arguments: r#"{"query":"天空城"}"#.to_string(),
                },
                index: 0,
            }]),
        );

        assert!(estimate_messages_tokens(&[with_call]) > estimate_messages_tokens(&[plain]));
    }

    #[test]
    fn unusual_text_inputs_do_not_panic() {
        assert_eq!(estimate_text_tokens(""), 0);
        assert_eq!(estimate_text_tokens("   "), 0);
        assert!(estimate_text_tokens("😀🚀✨") > 0);
        assert!(estimate_text_tokens(&"x".repeat(100_000)) > 0);
    }

    #[test]
    fn 前缀成立时使用真实基线() {
        let first = ChatRequest {
            messages: vec![Message::system("系统"), Message::user("第一问")],
            model: "test-model".to_string(),
            ..ChatRequest::default()
        };
        let baseline = RequestBaseline::new(100, &first, 1).unwrap();
        let mut next = first.clone();
        next.messages
            .push(Message::assistant(Some("第一答"), None::<String>, None));

        assert!(baseline.extends_request(&next, Some(2), &[1, 2]));
        assert_eq!(
            baseline.estimate(&next, 1.0),
            100 + estimate_messages_tokens(&next.messages[2..])
        );
    }

    #[test]
    fn checkout_和工具变化会使基线失效() {
        let first = ChatRequest {
            messages: vec![Message::user("第一问")],
            model: "test-model".to_string(),
            tools: Some(vec![serde_json::json!({"name": "search"})]),
            ..ChatRequest::default()
        };
        let baseline = RequestBaseline::new(100, &first, 1).unwrap();
        let mut next = first.clone();
        next.messages
            .push(Message::assistant(Some("第一答"), None::<String>, None));

        assert!(!baseline.extends_request(&next, Some(3), &[2, 3]));
        next.tools = Some(vec![serde_json::json!({"name": "write"})]);
        assert!(!baseline.extends_request(&next, Some(2), &[1, 2]));
    }
}
