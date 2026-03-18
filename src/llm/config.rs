// llm/config.rs

use anyhow::anyhow;
use anyhow::Result;

#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// API 端点
    pub base_url: String,

    /// API 密钥
    pub api_key: String,

    /// 事件 channel 缓冲大小
    pub event_buffer: usize,

    /// 单次请求超时（秒）
    pub request_timeout: u64,

    /// 工具调用最大连续轮数（防无限循环）
    pub max_tool_rounds: usize,

    /// 流式响应单行最大字节数（防异常数据撑爆内存）
    pub max_line_bytes: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
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
            return Err(anyhow!("base_url is empty"));
        }
        if !self.base_url.starts_with("http") {
            return Err(anyhow!("base_url must start with http"));
        }
        if self.api_key.is_empty() {
            return Err(anyhow!("api_key is empty"));
        }
        Ok(())
    }
}