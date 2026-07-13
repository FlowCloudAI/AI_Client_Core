pub mod audio;
pub mod client;
pub mod error;
mod http_poster;
pub mod image;
pub mod llm;
pub mod orchestrator;
pub mod plugin;
pub mod sense;
pub mod tool;
pub mod tts;

pub const SUPPORTED_AGREEMENT_VERSION: u32 = 1;

pub use audio::{AudioDecoder, AudioSource};
pub use client::FlowCloudAIClient;
pub use error::{ClientError, ErrorCode};
pub use image::ImageSession;
pub use llm::handle::SessionHandle;
pub use llm::session::LLMSession;
pub use llm::tree::{ConversationNode, ConversationNodeSeed};
pub use llm::types::{Message, SessionEvent, ThinkingType, ToolCall, TurnStatus, Usage};
pub use orchestrator::{AssembledTurn, DefaultOrchestrator, Orchestrate, TaskContext};
pub use plugin::manager::{PluginLoadError, PluginLoadReport};
pub use plugin::types::{ModelInfo, PluginKind, ThinkingEffort};
pub use tool::ToolRegistry;
pub use tts::TTSSession;
