use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[derive(PartialEq)]
pub enum PluginKind {
    #[serde(rename = "kind/llm")]
    LLM,
    #[serde(rename = "kind/image")]
    Image,
    #[serde(rename = "kind/tts")]
    TTS,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct PluginInfo {
    pub id: String,
    pub version: String,
    pub author: String,
    pub abi_version: u32,
    pub name: String,
    pub description: String,
    pub kind: PluginKind,
    pub url: String,
    pub model_list: Vec<String>,
}

#[derive(Clone)]
pub struct PluginMeta {
    pub id: String,
    pub name: String,
    pub description: String,
    pub author: String,
    pub version: String,
    pub kind: PluginKind,
    pub url: String,
    pub model_list: Vec<String>,

    pub fcplug_path: PathBuf,
}

