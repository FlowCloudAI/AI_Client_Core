pub(crate) mod militech_acs;
pub(crate) mod llm_a;
pub(crate) mod llm_b;

pub struct Senses {
    pub militech_acs: militech_acs::ACSSense,
    pub llm_a: llm_a::LLMASense,
    pub llm_b: llm_b::LLMBSense,
}

impl Senses {
    pub fn new() -> Self {
        Self {
            militech_acs: militech_acs::ACSSense::new(),
            llm_a: llm_a::LLMASense::new(),
            llm_b: llm_b::LLMBSense::new(),
        }
    }
}