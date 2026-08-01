//! LLM 请求的机械式上下文预算保护。
//!
//! 本模块只处理发送副本：先截断可丢失的大段文本，再按完整对话轮删除最早历史；
//! 系统消息、最新用户消息和工具调用参数始终保持原样，语义压缩由上层另行负责。

use crate::error::{ClientError, ErrorCode};
use crate::llm::token_estimate::{
    estimate_messages_tokens, estimate_request_tokens, estimate_text_tokens, estimate_with_factor,
};
use crate::llm::types::{ChatRequest, Message};
use serde_json::json;
use std::collections::HashSet;
use std::ops::Range;

const DEFAULT_OUTPUT_RESERVE_PERCENT: u64 = 15;
const SAFETY_RESERVE_PERCENT: u64 = 3;
const MIN_SAFETY_RESERVE: u64 = 1024;
const MIN_TRUNCATE_CHARS: usize = 512;
const MIN_KEEP_CHARS: usize = 256;
const TRUNCATION_SAFETY_FACTOR: f64 = 1.2;
const RECENT_ROUNDS_TO_KEEP: usize = 3;
const TRUNCATED_MARKER_PREFIX: &str = "…[已截断 ";
const TRUNCATED_MARKER_SUFFIX: &str = " 字符]…";
const OMITTED_ROUNDS_PREFIX: &str = "[早期 ";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ContextTrimReport {
    pub dropped_rounds: usize,
    pub truncated_messages: usize,
    pub before: u64,
    pub after: u64,
    pub budget: u64,
    pub suggest_compaction: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TrimOptions {
    pub budget_scale: f64,
    pub force_drop_oldest_round: bool,
}

impl TrimOptions {
    pub const NORMAL: Self = Self {
        budget_scale: 1.0,
        force_drop_oldest_round: false,
    };

    pub const OVERFLOW_RETRY: Self = Self {
        budget_scale: 0.7,
        force_drop_oldest_round: true,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum TextField {
    Content,
    Reasoning,
}

/// 将 `assistant(tool_calls) + tool...` 识别为不可拆分的消息块。
pub(crate) fn message_blocks(messages: &[Message]) -> Vec<Range<usize>> {
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        let start = index;
        index += 1;
        if messages[start].role == "assistant"
            && messages[start]
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty())
        {
            while index < messages.len() && messages[index].role == "tool" {
                index += 1;
            }
        }
        blocks.push(start..index);
    }
    blocks
}

pub(crate) fn context_budget(request: &ChatRequest, context_window_tokens: u64, scale: f64) -> u64 {
    let output_reserve = request
        .max_tokens
        .filter(|tokens| *tokens > 0)
        .map(|tokens| tokens as u64)
        .unwrap_or_else(|| percent_ceil(context_window_tokens, DEFAULT_OUTPUT_RESERVE_PERCENT));
    let safety_reserve =
        percent_ceil(context_window_tokens, SAFETY_RESERVE_PERCENT).max(MIN_SAFETY_RESERVE);
    let available = context_window_tokens
        .saturating_sub(output_reserve)
        .saturating_sub(safety_reserve);
    let normalized_scale = if scale.is_finite() {
        scale.clamp(0.0, 1.0)
    } else {
        1.0
    };
    (available as f64 * normalized_scale).floor() as u64
}

pub(crate) fn trim_request_for_window(
    request: &mut ChatRequest,
    context_window_tokens: u64,
    calibration_factor: f64,
    options: TrimOptions,
) -> Result<Option<ContextTrimReport>, ClientError> {
    let budget = context_budget(request, context_window_tokens, options.budget_scale);
    trim_request_to_budget(
        request,
        budget,
        calibration_factor,
        options.force_drop_oldest_round,
    )
}

pub(crate) fn trim_request_to_budget(
    request: &mut ChatRequest,
    budget: u64,
    calibration_factor: f64,
    force_drop_oldest_round: bool,
) -> Result<Option<ContextTrimReport>, ClientError> {
    let before = calibrated_request_tokens(request, calibration_factor);
    if before <= budget && !force_drop_oldest_round {
        return Ok(None);
    }

    let mut candidate = request.clone();
    let last_user = candidate
        .messages
        .iter()
        .rposition(|message| message.role == "user");
    let mut truncated_indices = HashSet::new();
    let mut actual = before;

    if actual > budget {
        loop {
            let mut candidates = truncation_candidates(&candidate.messages, last_user);
            candidates.sort_by_key(|(priority, chars, index, field)| {
                (*priority, std::cmp::Reverse(*chars), *index, *field)
            });
            let mut made_progress = false;

            for (_, _, index, field) in candidates {
                if actual <= budget {
                    break;
                }
                let original = match field {
                    TextField::Content => candidate.messages[index].content.clone(),
                    TextField::Reasoning => candidate.messages[index].reasoning_content.clone(),
                };
                let Some(text) = original.as_deref() else {
                    continue;
                };
                let target_chars = truncation_target_chars(
                    text,
                    actual.saturating_sub(budget),
                    calibration_factor,
                );
                let Some(truncated) = truncate_text_to(text, target_chars) else {
                    continue;
                };

                match field {
                    TextField::Content => candidate.messages[index].content = Some(truncated),
                    TextField::Reasoning => {
                        candidate.messages[index].reasoning_content = Some(truncated)
                    }
                }
                let next_actual = calibrated_request_tokens(&candidate, calibration_factor);
                if next_actual >= actual {
                    match field {
                        TextField::Content => candidate.messages[index].content = original,
                        TextField::Reasoning => {
                            candidate.messages[index].reasoning_content = original
                        }
                    }
                    continue;
                }

                truncated_indices.insert(index);
                actual = next_actual;
                made_progress = true;
            }

            if actual <= budget || !made_progress {
                break;
            }
        }
    }

    let mut prior_dropped_rounds = 0;
    if actual > budget || force_drop_oldest_round {
        candidate.messages.retain(|message| {
            let Some(count) = omitted_round_count(message) else {
                return true;
            };
            prior_dropped_rounds += count;
            false
        });
        actual = calibrated_request_tokens(&candidate, calibration_factor);
    }

    let mut dropped_rounds = 0;
    let mut must_force_drop = force_drop_oldest_round;
    loop {
        let rounds = conversation_rounds(&candidate.messages);
        let keep = if force_drop_oldest_round {
            1
        } else {
            RECENT_ROUNDS_TO_KEEP
        };
        if rounds.len() <= keep || (actual <= budget && !must_force_drop) {
            break;
        }

        // ponytail: 对话轮数通常很小；若超长会话成为热点，再改为单次索引重建。
        let removed: HashSet<_> = rounds[0].iter().copied().collect();
        candidate.messages = candidate
            .messages
            .into_iter()
            .enumerate()
            .filter_map(|(index, message)| (!removed.contains(&index)).then_some(message))
            .collect();
        dropped_rounds += 1;
        must_force_drop = false;
        actual = calibrated_request_tokens(&candidate, calibration_factor);
    }

    if dropped_rounds > 0 {
        let total = prior_dropped_rounds + dropped_rounds;
        let insert_at = candidate
            .messages
            .iter()
            .position(|message| message.role != "system")
            .unwrap_or(candidate.messages.len());
        candidate.messages.insert(
            insert_at,
            Message::system(format!("[早期 {total} 轮对话因超出上下文已省略]")),
        );
        actual = calibrated_request_tokens(&candidate, calibration_factor);
    }

    let changed = !truncated_indices.is_empty() || dropped_rounds > 0;
    if actual > budget || (force_drop_oldest_round && !changed) {
        return Err(context_budget_error(&candidate, budget, calibration_factor));
    }

    let report = ContextTrimReport {
        dropped_rounds,
        truncated_messages: truncated_indices.len(),
        before,
        after: actual,
        budget,
        suggest_compaction: changed,
    };
    *request = candidate;
    Ok(changed.then_some(report))
}

pub(crate) fn context_budget_error(
    request: &ChatRequest,
    budget: u64,
    calibration_factor: f64,
) -> ClientError {
    let actual = calibrated_request_tokens(request, calibration_factor);
    let mut largest: Vec<_> = request
        .messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            (
                estimate_with_factor(
                    estimate_messages_tokens(std::slice::from_ref(message)),
                    calibration_factor,
                ),
                index,
                message.role.clone(),
            )
        })
        .collect();
    largest.sort_by_key(|(tokens, _, _)| std::cmp::Reverse(*tokens));
    let top_3_largest_messages: Vec<_> = largest
        .into_iter()
        .take(3)
        .map(|(tokens, index, role)| json!({ "index": index, "role": role, "tokens": tokens }))
        .collect();

    ClientError::new(
        ErrorCode::ContextBudgetExceeded,
        "上下文仍超过模型可用预算，请压缩对话、缩短附件或降低最大输出长度后重试",
    )
    .with_kv("budget", budget)
    .with_kv("actual", actual)
    .with_kv("top_3_largest_messages", json!(top_3_largest_messages))
}

fn calibrated_request_tokens(request: &ChatRequest, calibration_factor: f64) -> u64 {
    estimate_with_factor(estimate_request_tokens(request), calibration_factor)
}

fn percent_ceil(value: u64, percent: u64) -> u64 {
    value.saturating_mul(percent).saturating_add(99) / 100
}

fn truncation_candidates(
    messages: &[Message],
    last_user: Option<usize>,
) -> Vec<(usize, usize, usize, TextField)> {
    let mut candidates = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if message.role == "system" || Some(index) == last_user {
            continue;
        }
        let priority = usize::from(message.role != "tool");
        for (field, text) in [
            (TextField::Content, message.content.as_deref()),
            (TextField::Reasoning, message.reasoning_content.as_deref()),
        ] {
            let Some(text) = text else { continue };
            let chars = visible_text_chars(text);
            if chars.saturating_sub(MIN_KEEP_CHARS) >= MIN_TRUNCATE_CHARS {
                candidates.push((priority, chars, index, field));
            }
        }
    }
    candidates
}

fn truncation_target_chars(text: &str, excess_tokens: u64, calibration_factor: f64) -> usize {
    let visible = visible_text(text);
    let chars = visible.chars().count();
    if chars <= MIN_KEEP_CHARS {
        return chars;
    }
    let estimated_tokens = estimate_text_tokens(&visible);
    if estimated_tokens == 0 {
        return MIN_KEEP_CHARS;
    }

    let factor = if calibration_factor.is_finite() {
        calibration_factor.clamp(0.5, 2.0)
    } else {
        1.0
    };
    let tokens_per_char = estimated_tokens as f64 / chars as f64;
    let chars_to_remove = ((excess_tokens as f64 / factor) * TRUNCATION_SAFETY_FACTOR
        / tokens_per_char)
        .ceil() as usize;
    chars.saturating_sub(chars_to_remove).max(MIN_KEEP_CHARS)
}

fn truncate_text_to(text: &str, target_chars: usize) -> Option<String> {
    let (visible, prior_removed) = match parse_truncated_marker(text) {
        Some((prefix, removed, suffix)) => (format!("{prefix}{suffix}"), removed),
        None => (text.to_string(), 0),
    };
    let chars: Vec<_> = visible.chars().collect();
    let target_chars = target_chars.max(MIN_KEEP_CHARS).min(chars.len());
    if target_chars >= chars.len() {
        return None;
    }

    let head = target_chars * 75 / 100;
    let tail = target_chars - head;
    let removed = prior_removed.saturating_add(chars.len() - target_chars);
    Some(format!(
        "{}{}{}{}{}",
        chars[..head].iter().collect::<String>(),
        TRUNCATED_MARKER_PREFIX,
        removed,
        TRUNCATED_MARKER_SUFFIX,
        chars[chars.len() - tail..].iter().collect::<String>()
    ))
}

fn parse_truncated_marker(text: &str) -> Option<(&str, usize, &str)> {
    let (prefix, rest) = text.split_once(TRUNCATED_MARKER_PREFIX)?;
    let (removed, suffix) = rest.split_once(TRUNCATED_MARKER_SUFFIX)?;
    Some((prefix, removed.parse().ok()?, suffix))
}

fn visible_text(text: &str) -> String {
    match parse_truncated_marker(text) {
        Some((prefix, _, suffix)) => format!("{prefix}{suffix}"),
        None => text.to_string(),
    }
}

fn visible_text_chars(text: &str) -> usize {
    match parse_truncated_marker(text) {
        Some((prefix, _, suffix)) => prefix.chars().count() + suffix.chars().count(),
        None => text.chars().count(),
    }
}

fn conversation_rounds(messages: &[Message]) -> Vec<Vec<usize>> {
    let mut rounds: Vec<Vec<usize>> = Vec::new();
    for block in message_blocks(messages) {
        let role = &messages[block.start].role;
        if role == "system" {
            continue;
        }
        if role == "user" || rounds.is_empty() {
            rounds.push(Vec::new());
        }
        rounds.last_mut().unwrap().extend(block);
    }
    rounds
}

fn omitted_round_count(message: &Message) -> Option<usize> {
    if message.role != "system" {
        return None;
    }
    let content = message.content.as_deref()?;
    let rest = content.strip_prefix(OMITTED_ROUNDS_PREFIX)?;
    rest.split_once(" 轮对话因超出上下文已省略]")
        .and_then(|(count, tail)| tail.is_empty().then_some(count))?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::{ToolCall, ToolFunctionCall};

    fn request(messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            messages,
            model: "test-model".to_string(),
            ..ChatRequest::default()
        }
    }

    fn rounds(count: usize, chars: usize) -> Vec<Message> {
        let mut messages = vec![Message::system("系统提示")];
        for index in 1..=count {
            messages.push(Message::user(format!("第{index}轮-{}", "问".repeat(chars))));
            messages.push(Message::assistant(
                Some(format!("第{index}轮-{}", "答".repeat(chars))),
                None::<String>,
                None,
            ));
        }
        messages
    }

    #[test]
    fn 系统消息始终保留() {
        let mut req = request(rounds(5, 450));
        trim_request_to_budget(&mut req, 2400, 1.0, false).unwrap();
        assert!(
            req.messages.iter().any(|message| message.role == "system"
                && message.content.as_deref() == Some("系统提示"))
        );
    }

    #[test]
    fn 工具调用与三个结果同留同删() {
        let calls = (1..=3)
            .map(|index| ToolCall {
                id: Some(format!("call_{index}")),
                call_type: Some("function".to_string()),
                function: ToolFunctionCall {
                    name: format!("tool_{index}"),
                    arguments: "{}".to_string(),
                },
                index: index - 1,
            })
            .collect();
        let mut messages = vec![
            Message::system("系统提示"),
            Message::user("旧问题".repeat(100)),
            Message::assistant(None::<String>, None::<String>, Some(calls)),
            Message::tool("结果一".repeat(100), "call_1"),
            Message::tool("结果二".repeat(100), "call_2"),
            Message::tool("结果三".repeat(100), "call_3"),
        ];
        messages.extend(rounds(4, 300).into_iter().skip(1));
        assert!(
            message_blocks(&messages)
                .iter()
                .any(|block| block.len() == 4)
        );

        let mut req = request(messages);
        trim_request_to_budget(&mut req, 1800, 1.0, false).unwrap();
        let remaining = req
            .messages
            .iter()
            .filter(|message| {
                message.role == "tool"
                    || message
                        .tool_calls
                        .as_ref()
                        .is_some_and(|calls| !calls.is_empty())
            })
            .count();
        assert!(remaining == 0 || remaining == 4);
    }

    #[test]
    fn 最新用户消息始终保留() {
        let mut messages = rounds(5, 450);
        messages.push(Message::user("必须保留的最新问题"));
        let mut req = request(messages);
        trim_request_to_budget(&mut req, 2400, 1.0, false).unwrap();
        assert!(
            req.messages
                .iter()
                .any(|message| message.content.as_deref() == Some("必须保留的最新问题"))
        );
    }

    #[test]
    fn 只删除最早连续轮次() {
        let mut req = request(rounds(6, 450));
        trim_request_to_budget(&mut req, 2400, 1.0, false).unwrap();
        let kept: Vec<_> = req
            .messages
            .iter()
            .filter(|message| message.role == "user")
            .filter_map(|message| {
                message
                    .content
                    .as_deref()?
                    .strip_prefix('第')?
                    .split_once('轮')?
                    .0
                    .parse::<usize>()
                    .ok()
            })
            .collect();
        assert_eq!(kept, (kept[0]..=6).collect::<Vec<_>>());
    }

    #[test]
    fn 文本截断不改变消息数量与顺序() {
        let mut req = request(vec![
            Message::system("系统提示"),
            Message::user("调用工具"),
            Message::assistant(
                None::<String>,
                None::<String>,
                Some(vec![ToolCall {
                    id: Some("call_1".to_string()),
                    call_type: Some("function".to_string()),
                    function: ToolFunctionCall {
                        name: "large_result".to_string(),
                        arguments: "{\"safe\":true}".to_string(),
                    },
                    index: 0,
                }]),
            ),
            Message::tool("x".repeat(10_000), "call_1"),
        ]);
        let roles: Vec<_> = req
            .messages
            .iter()
            .map(|message| message.role.clone())
            .collect();
        let before = calibrated_request_tokens(&req, 1.0);
        let report = trim_request_to_budget(&mut req, before * 9 / 10, 1.0, false)
            .unwrap()
            .unwrap();
        assert_eq!(report.truncated_messages, 1);
        assert_eq!(
            roles,
            req.messages
                .iter()
                .map(|message| message.role.clone())
                .collect::<Vec<_>>()
        );
        assert!(
            req.messages[3]
                .content
                .as_deref()
                .unwrap()
                .contains(TRUNCATED_MARKER_PREFIX)
        );
        assert_eq!(
            req.messages[2].tool_calls.as_ref().unwrap()[0]
                .function
                .arguments,
            "{\"safe\":true}"
        );
    }

    #[test]
    fn 单条超大工具结果可裁剪到预算内() {
        let mut req = request(vec![
            Message::system("系统提示"),
            Message::user("调用工具"),
            Message::assistant(
                None::<String>,
                None::<String>,
                Some(vec![ToolCall {
                    id: Some("call_1".to_string()),
                    call_type: Some("function".to_string()),
                    function: ToolFunctionCall {
                        name: "large_result".to_string(),
                        arguments: "{}".to_string(),
                    },
                    index: 0,
                }]),
            ),
            Message::tool("世".repeat(60_000), "call_1"),
        ]);

        let report = trim_request_to_budget(&mut req, 8_000, 1.0, false)
            .unwrap()
            .unwrap();
        assert!(report.after <= report.budget);
        assert_eq!(report.truncated_messages, 1);
        assert!(
            req.messages[3]
                .content
                .as_deref()
                .is_some_and(|text| text.contains(TRUNCATED_MARKER_PREFIX))
        );
    }

    #[test]
    fn 重复截断累加已截字符计数() {
        let first = truncate_text_to(&"x".repeat(4_000), 2_500).unwrap();
        let second = truncate_text_to(&first, 1_000).unwrap();
        let (_, removed, _) = parse_truncated_marker(&second).unwrap();

        assert_eq!(removed, 3_000);
        assert_eq!(visible_text_chars(&second), 1_000);
    }

    #[test]
    fn 截断不会削破最小保留下限() {
        let truncated = truncate_text_to(&"x".repeat(1_000), 1).unwrap();
        assert_eq!(visible_text_chars(&truncated), MIN_KEEP_CHARS);

        let mut req = request(vec![
            Message::system("系统提示"),
            Message::user("调用工具"),
            Message::tool("x".repeat(1_000), "call_1"),
        ]);
        assert!(trim_request_to_budget(&mut req, 1, 1.0, false).is_err());
    }

    #[test]
    fn 截断循环在无法继续削减时终止() {
        let at_limit = truncate_text_to(&"x".repeat(1_000), MIN_KEEP_CHARS).unwrap();
        let mut req = request(vec![
            Message::system("系统提示"),
            Message::user("调用工具"),
            Message::tool(at_limit, "call_1"),
        ]);

        assert!(trim_request_to_budget(&mut req, 1, 1.0, false).is_err());
    }

    #[test]
    fn 无法裁剪时返回完整预算诊断() {
        let mut req = request(vec![
            Message::system("不可删除".repeat(3000)),
            Message::user("最新问题"),
        ]);
        let error = trim_request_to_budget(&mut req, 10, 1.0, false).unwrap_err();
        assert_eq!(error.code, ErrorCode::ContextBudgetExceeded);
        assert_eq!(error.detail["budget"], 10);
        assert!(error.detail["actual"].as_u64().unwrap() > 10);
        assert!(
            !error.detail["top_3_largest_messages"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn 裁剪结果不超预算且二次执行幂等() {
        let mut req = request(rounds(6, 450));
        let report = trim_request_to_budget(&mut req, 2400, 1.0, false)
            .unwrap()
            .unwrap();
        assert!(report.after <= report.budget);
        let snapshot = serde_json::to_value(&req.messages).unwrap();
        assert!(
            trim_request_to_budget(&mut req, 2400, 1.0, false)
                .unwrap()
                .is_none()
        );
        assert_eq!(snapshot, serde_json::to_value(&req.messages).unwrap());
    }
}
