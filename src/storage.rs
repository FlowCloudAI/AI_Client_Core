use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::{fs, io::Write};

// ═════════════════════════════════════════════════════════════
//                      存储数据结构
// ═════════════════════════════════════════════════════════════

/// 对话元信息（不含消息体，用于列表展示）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMeta {
    pub id: String,
    pub title: String,
    pub plugin_id: String,
    pub model: String,
    pub created_at: String,
    pub updated_at: String,
}

/// 存储格式中的单条消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<u64>,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<crate::llm::types::ToolCall>>,
}

/// 存储在磁盘上的完整对话（元信息 + 消息列表）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredConversation {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(flatten)]
    pub meta: ConversationMeta,
    pub messages: Vec<StoredMessage>,
    /// 当前 head 节点 ID（v3 新增，旧文件默认为 None）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<u64>,
}

fn default_schema_version() -> u32 {
    3
}

// ═════════════════════════════════════════════════════════════
//                    对话存储（ConversationStore）
// ═════════════════════════════════════════════════════════════

/// 基于 JSON 文件的对话持久化存储。
/// 每条对话存为 {conversation_id}.json。
pub struct ConversationStore {
    dir: PathBuf,
}

impl ConversationStore {
    /// 初始化存储目录（不存在则自动创建）。
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn sanitize_id<'a>(&self, id: &'a str) -> Result<&'a str> {
        let valid = !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
        if valid {
            Ok(id)
        } else {
            Err(anyhow!("invalid conversation id"))
        }
    }

    fn path_for(&self, id: &str) -> Result<PathBuf> {
        let safe_id = self.sanitize_id(id)?;
        Ok(self.dir.join(format!("{}.json", safe_id)))
    }

    /// 保存（覆盖写入）一条对话。
    pub fn save(&self, conv: &StoredConversation) -> Result<()> {
        let path = self.path_for(&conv.meta.id)?;
        let json = serde_json::to_string_pretty(conv)?;
        let temp_path = path.with_extension("json.tmp");

        {
            let mut file = fs::File::create(&temp_path)?;
            file.write_all(json.as_bytes())?;
            file.flush()?;
            file.sync_all()?;
        }

        if path.exists() {
            fs::remove_file(&path)?;
        }
        fs::rename(&temp_path, &path)?;
        Ok(())
    }

    /// 列出所有对话元信息，按 updated_at 降序。
    pub fn list(&self) -> Vec<ConversationMeta> {
        let mut metas = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return metas;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                match serde_json::from_str::<StoredConversation>(&content) {
                    Ok(conv) => metas.push(conv.meta),
                    Err(err) => eprintln!("[storage] 解析对话文件失败: path={} error={}", path.display(), err),
                }
            } else {
                eprintln!("[storage] 读取对话文件失败: path={}", path.display());
            }
        }
        metas.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        metas
    }

    /// 返回完整对话（含消息列表），未找到则返回 None。
    pub fn get(&self, id: &str) -> Option<StoredConversation> {
        let path = self.path_for(id).ok()?;
        let content = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str(&content) {
            Ok(conv) => Some(conv),
            Err(err) => {
                eprintln!("[storage] 解析对话文件失败: path={} error={}", path.display(), err);
                None
            }
        }
    }

    /// 删除指定对话文件。
    pub fn delete(&self, id: &str) -> Result<()> {
        let path = self.path_for(id)?;
        if !path.exists() {
            return Err(anyhow!("conversation '{}' not found", id));
        }
        std::fs::remove_file(path)?;
        Ok(())
    }

    /// 重命名（修改标题）并更新 updated_at。
    pub fn rename(&self, id: &str, title: String) -> Result<()> {
        let mut conv = self
            .get(id)
            .ok_or_else(|| anyhow!("conversation '{}' not found", id))?;
        conv.meta.title = title;
        conv.meta.updated_at = chrono::Utc::now().to_rfc3339();
        self.save(&conv)
    }
}

// ═════════════════════════════════════════════════════════════
//                    StorageCtx（供 session 使用）
// ═════════════════════════════════════════════════════════════

/// session 持有的存储上下文，负责在每轮成功结束后写盘。
pub struct StorageCtx {
    pub conversation_id: String,
    pub plugin_id: String,
    pub store: Arc<ConversationStore>,
    /// 首次创建时的时间戳（ISO 8601）
    pub created_at: String,
}

impl StorageCtx {
    pub fn new(plugin_id: String, store: Arc<ConversationStore>) -> Self {
        let now = chrono::Utc::now();
        Self {
            conversation_id: now.format("%Y%m%d%H%M%S%3f").to_string(),
            plugin_id,
            store,
            created_at: now.to_rfc3339(),
        }
    }

    /// 从已有对话 ID 构建上下文（续聊时使用）。
    pub fn from_existing(
        conversation_id: String,
        plugin_id: String,
        store: Arc<ConversationStore>,
        created_at: String,
    ) -> Self {
        Self {
            conversation_id,
            plugin_id,
            store,
            created_at,
        }
    }

    /// 将消息列表写入磁盘。如果文件已存在，保留原标题；否则从第一条 user 消息自动生成。
    pub fn flush(
        &self,
        messages: Vec<StoredMessage>,
        model: &str,
        head: Option<u64>,
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();

        // 保留已有标题（rename 修改的是文件，这里重读以拿到最新值）
        let (title, created_at) = match self.store.get(&self.conversation_id) {
            Some(existing) => (existing.meta.title, existing.meta.created_at),
            None => {
                let auto_title = messages
                    .iter()
                    .find(|m| m.role == "user")
                    .and_then(|m| m.content.as_deref())
                    .map(|s| {
                        let truncated: String = s.chars().take(50).collect();
                        if s.chars().count() > 50 {
                            format!("{}…", truncated)
                        } else {
                            truncated
                        }
                    })
                    .unwrap_or_else(|| "新对话".to_string());
                (auto_title, self.created_at.clone())
            }
        };

        let conv = StoredConversation {
            schema_version: default_schema_version(),
            meta: ConversationMeta {
                id: self.conversation_id.clone(),
                title,
                plugin_id: self.plugin_id.clone(),
                model: model.to_string(),
                created_at,
                updated_at: now,
            },
            messages,
            head,
        };

        self.store.save(&conv)
    }
}
