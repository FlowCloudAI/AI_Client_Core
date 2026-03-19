use flowcloudai_client::llm::types::ChatRequest;
use flowcloudai_client::sense::Sense;
use flowcloudai_client::tool::registry::ToolRegistry;

pub struct LLMASense {
    prompt: String,
    config: ChatRequest
}

impl LLMASense {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            prompt: "你是一位苏格拉底式的哲学家，擅长通过提问引导思考。请与尼采对话，探讨‘人生的意义是什么？’。每次回复尽量简洁，并保持追问风格。".to_string(),
            config: ChatRequest {
                temperature: Some(1.0),
                presence_penalty: None,
                ..Default::default()
            }
        }
    }
}

impl Sense for LLMASense {
    fn prompts(&self) -> Vec<String> {
        vec![self.prompt.clone()]
    }

    fn default_request(&self) -> Option<ChatRequest> {
        Some(self.config.clone())
    }

    fn install_tools(&self, _tools: &mut ToolRegistry) -> anyhow::Result<()> {

        Ok(())
    }
}