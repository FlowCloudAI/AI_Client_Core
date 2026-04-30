use serde::{Deserialize, Serialize};

// ─────────────────────── 请求类型 ───────────────────────

/// 图像生成请求。
///
/// 基准接口：火山方舟 Seedream（/api/v3/images/generations）。
/// 其他供应商通过 wasm 插件的 `map_request` 映射到此格式。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageRequest {
    /// 模型 ID
    pub model: String,

    /// 文本提示词（建议不超过 300 汉字 / 600 英文单词）
    pub prompt: String,

    /// 参考图片（单张 URL 字符串或多张 URL 数组）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageInput>,

    /// 输出尺寸："2K"/"3K"/"4K" 或像素值 "2048x2048"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,

    /// 输出文件格式："png" 或 "jpeg"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_format: Option<String>,

    /// 返回格式："url" 或 "b64_json"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<String>,

    /// 是否添加水印
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watermark: Option<bool>,

    /// 是否流式输出
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,

    /// 组图生成模式："auto" 或 "disabled"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequential_image_generation: Option<String>,

    /// 组图生成选项
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequential_image_generation_options: Option<SequentialOptions>,

    /// 提示词优化模式
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optimize_prompt_options: Option<OptimizePromptOptions>,

    /// 工具列表（如联网搜索）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ImageTool>>,
}

/// 参考图输入：单张或多张
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ImageInput {
    Single(String),
    Multiple(Vec<String>),
}

/// 组图生成选项
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequentialOptions {
    /// 最大生成图片数量（参考图 + 生成图 ≤ 15）
    pub max_images: u32,
}

/// 提示词优化选项
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizePromptOptions {
    /// "standard" 或 "fast"
    pub mode: String,
}

/// 工具（如联网搜索）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageTool {
    /// 工具类型，如 "web_search"
    #[serde(rename = "type")]
    pub tool_type: String,
}

// ─────────────────────── 构造器 ─────────────────────────

impl ImageRequest {
    /// 最简构造：文生图
    pub fn text_to_image(model: &str, prompt: &str) -> Self {
        Self {
            model: model.to_string(),
            prompt: prompt.to_string(),
            image: None,
            size: Some("2K".to_string()),
            output_format: Some("png".to_string()),
            response_format: Some("url".to_string()),
            watermark: Some(false),
            stream: Some(false),
            sequential_image_generation: None,
            sequential_image_generation_options: None,
            optimize_prompt_options: None,
            tools: None,
        }
    }

    /// 单图编辑
    pub fn image_to_image(model: &str, prompt: &str, image_url: &str) -> Self {
        let mut req = Self::text_to_image(model, prompt);
        req.image = Some(ImageInput::Single(image_url.to_string()));
        req
    }

    /// 多图融合
    pub fn images_to_image(model: &str, prompt: &str, image_urls: Vec<String>) -> Self {
        let mut req = Self::text_to_image(model, prompt);
        req.image = Some(ImageInput::Multiple(image_urls));
        req
    }

    pub fn size(mut self, size: &str) -> Self {
        self.size = Some(size.to_string());
        self
    }

    pub fn format_png(mut self) -> Self {
        self.output_format = Some("png".to_string());
        self
    }

    pub fn format_jpeg(mut self) -> Self {
        self.output_format = Some("jpeg".to_string());
        self
    }

    pub fn watermark(mut self, enabled: bool) -> Self {
        self.watermark = Some(enabled);
        self
    }

    pub fn response_url(mut self) -> Self {
        self.response_format = Some("url".to_string());
        self
    }

    pub fn response_b64(mut self) -> Self {
        self.response_format = Some("b64_json".to_string());
        self
    }

    /// 启用组图生成
    pub fn sequential(mut self, max_images: u32) -> Self {
        self.sequential_image_generation = Some("auto".to_string());
        self.sequential_image_generation_options = Some(SequentialOptions { max_images });
        self
    }

    /// 启用联网搜索
    pub fn web_search(mut self) -> Self {
        self.tools = Some(vec![ImageTool {
            tool_type: "web_search".to_string(),
        }]);
        self
    }

    /// 提示词优化模式
    pub fn optimize_prompt(mut self, mode: &str) -> Self {
        self.optimize_prompt_options = Some(OptimizePromptOptions {
            mode: mode.to_string(),
        });
        self
    }
}

// ─────────────────────── 响应类型 ───────────────────────

/// 图像生成响应
#[derive(Debug, Clone, Deserialize)]
pub struct ImageResponse {
    pub created: Option<u64>,
    pub data: Option<Vec<ImageData>>,
    pub error: Option<ImageError>,
    pub usage: Option<ImageUsage>,
}

/// 单张图片数据
#[derive(Debug, Clone, Deserialize)]
pub struct ImageData {
    /// 图片 URL（response_format="url" 时）
    pub url: Option<String>,

    /// Base64 编码图片（response_format="b64_json" 时）
    pub b64_json: Option<String>,

    /// 图片尺寸，如 "2048x2048"
    pub size: Option<String>,

    /// 修订后的提示词
    pub revised_prompt: Option<String>,
}

/// 错误信息
#[derive(Debug, Clone, Deserialize)]
pub struct ImageError {
    pub code: Option<String>,
    pub message: Option<String>,
}

/// 用量信息
#[derive(Debug, Clone, Deserialize)]
pub struct ImageUsage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub tool_usage: Option<ToolUsage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolUsage {
    pub web_search: Option<u32>,
}

// ─────────────────────── 便捷结果类型 ───────────────────

/// ImageSession::generate 的返回类型。
#[derive(Debug, Clone)]
pub struct ImageResult {
    /// 生成的图片列表
    pub images: Vec<GeneratedImage>,

    /// 用量信息
    pub usage: Option<ImageUsage>,
}

/// 单张生成图片
#[derive(Debug, Clone)]
pub struct GeneratedImage {
    /// 图片 URL
    pub url: Option<String>,

    /// 原始 bytes（从 b64_json 解码）
    pub data: Vec<u8>,

    /// 图片尺寸
    pub size: Option<String>,

    /// 修订后的提示词
    pub revised_prompt: Option<String>,
}