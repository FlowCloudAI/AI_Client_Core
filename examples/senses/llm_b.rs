use flowcloudai_client::llm::types::ChatRequest;
use flowcloudai_client::llm::sense::{sense_state_new, SenseLoader, SenseState};
use flowcloudai_client::llm::tool::ToolFunctions;

#[allow(dead_code)]
pub struct LLMBSense {
    prompt: String,
    config: ChatRequest,
    status: SenseState<LLMBState>,
}

#[allow(dead_code)]
struct LLMBState;
impl Default for LLMBState { fn default() -> Self { Self{} } }

impl LLMBSense {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            prompt: "你是一位尼采式的哲学家，推崇权力意志和超人哲学。请与苏格拉底对话，探讨‘人生的意义是什么？’。每次回复尽量简洁，可以反驳或升华对方的观点。".to_string(),
            config: ChatRequest {
                temperature: Some(1.0),
                presence_penalty: None,
                ..Default::default()
            },
            status: sense_state_new::<LLMBState>(),
        }
    }
}

impl SenseLoader for LLMBSense {
    fn get_prompt(&self) -> Option<Vec<String>> {
        Some(vec![self.prompt.clone()])
    }

    fn get_request(&self) -> Option<ChatRequest> {
        Some(self.config.clone())
    }

    fn install_tool(&self, _tools: &mut ToolFunctions) -> anyhow::Result<String> {

        Ok("ZL Sense installed".to_string())
    }
}