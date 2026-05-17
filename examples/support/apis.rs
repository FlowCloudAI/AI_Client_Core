#![allow(dead_code)]

/// 示例 API 配置。
///
/// 默认不保存真实密钥。需要实际运行示例时，可在编译前设置对应环境变量。
pub struct ApiProfile {
    pub key: &'static str,
}

pub const DEEPSEEK: ApiProfile = ApiProfile {
    key: match option_env!("DEEPSEEK_API_KEY") {
        Some(key) => key,
        None => "",
    },
};

pub const QWEN_LLM: ApiProfile = ApiProfile {
    key: match option_env!("QWEN_API_KEY") {
        Some(key) => key,
        None => "",
    },
};
