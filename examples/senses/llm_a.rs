use flowcloudai_client::llm::types::ChatRequest;
use flowcloudai_client::llm::sense::{sense_state_new, SenseLoader, SenseState};
use flowcloudai_client::llm::tool::ToolFunctions;

#[allow(dead_code)]
pub struct LLMASense {
    prompt: String,
    config: ChatRequest,
    status: SenseState<LLMAState>,
}

#[allow(dead_code)]
struct LLMAState;
impl Default for LLMAState { fn default() -> Self { Self {} } }

impl LLMASense {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            prompt: "你是一位苏格拉底式的哲学家，擅长通过提问引导思考。请与尼采对话，探讨‘人生的意义是什么？’。每次回复尽量简洁，并保持追问风格。".to_string(),
            config: ChatRequest {
                temperature: Some(1.0),
                presence_penalty: None,
                ..Default::default()
            },
            status: sense_state_new::<LLMAState>(),
        }
    }
}

impl SenseLoader for LLMASense {
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