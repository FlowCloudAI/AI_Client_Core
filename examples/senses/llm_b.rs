use flowcloudai_client::llm::types::ChatRequest;
use flowcloudai_client::sense::Sense;
use flowcloudai_client::tool::registry::ToolRegistry;

pub struct LLMBSense {
    prompt: String,
    config: ChatRequest,
}

impl LLMBSense {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            prompt: "你是一位尼采式的哲学家，推崇权力意志和超人哲学。请与苏格拉底对话，探讨‘人生的意义是什么？’。每次回复尽量简洁，可以反驳或升华对方的观点。".to_string(),
            config: ChatRequest {
                temperature: Some(1.0),
                presence_penalty: None,
                ..Default::default()
            }
        }
    }
}

impl Sense for LLMBSense {
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