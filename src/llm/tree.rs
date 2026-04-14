use std::collections::HashMap;
use crate::llm::types::Message;

// ═════════════════════════════════════════════════════════════
//                      对话消息树
// ═════════════════════════════════════════════════════════════

/// 树节点的唯一标识符。
/// Phase 3 起会通过 TurnBegin / TurnEnd 事件暴露给前端，用于历史导航。
pub type NodeId = u64;

/// 树中的单个消息节点
#[derive(Debug, Clone)]
pub struct ConversationNode {
    pub id: NodeId,
    pub message: Message,
    /// 父节点 ID，根节点为 None
    pub parent: Option<NodeId>,
    /// 所属轮次（Phase 3 由 drive loop 填充，Phase 1/2 填 0）
    pub turn_id: u64,
}

/// 对话消息树
///
/// 以链表形式组织消息历史，支持任意节点的 checkout（分支/回退）。
/// 线性化（root → head）的路径即为发往 API 的消息序列。
///
/// # 生命周期
/// - `append(msg)` 追加消息并推进 head
/// - `checkout(node_id)` 移动 head（不删除任何节点）
/// - `linearize()` 输出当前路径的有序消息列表
#[derive(Debug, Clone)]
pub struct ConversationTree {
    nodes: HashMap<NodeId, ConversationNode>,
    /// 当前活跃叶节点，None 表示树为空
    head: Option<NodeId>,
    next_id: u64,
}

impl Default for ConversationTree {
    fn default() -> Self {
        Self::new()
    }
}

impl ConversationTree {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            head: None,
            next_id: 1,
        }
    }

    // ── 写操作 ──────────────────────────────────────────────

    /// 在当前 head 之后追加一条消息，推进 head，返回新节点 ID。
    pub fn append(&mut self, message: Message, turn_id: u64) -> NodeId {
        let id = self.next_id;
        self.next_id += 1;

        self.nodes.insert(id, ConversationNode {
            id,
            message,
            parent: self.head,
            turn_id,
        });

        self.head = Some(id);
        id
    }

    /// 将 head 移动到指定节点（分支、重说、历史回退的统一原语）。
    ///
    /// 不删除任何节点，原有路径依然完整保留在树中。
    pub fn checkout(&mut self, node_id: NodeId) -> Result<(), String> {
        if self.nodes.contains_key(&node_id) {
            self.head = Some(node_id);
            Ok(())
        } else {
            Err(format!("节点 {} 不存在", node_id))
        }
    }

    // ── 读操作 ──────────────────────────────────────────────

    /// 当前 head 节点 ID。
    pub fn head(&self) -> Option<NodeId> {
        self.head
    }

    /// 当前 head 消息的 role（"user" / "assistant" / "tool" / "system"）。
    /// 树为空时返回 None。
    pub fn head_role(&self) -> Option<&str> {
        self.head
            .and_then(|id| self.nodes.get(&id))
            .map(|n| n.message.role.as_str())
    }

    /// 按 ID 获取节点。
    pub fn get_node(&self, id: NodeId) -> Option<&ConversationNode> {
        self.nodes.get(&id)
    }

    /// 返回从最早祖先到当前 head 的节点 ID 路径（含端点，升序）。
    pub fn path_to_head(&self) -> Vec<NodeId> {
        self.path_to(self.head)
    }

    /// 返回从最早祖先到指定节点的路径（含端点，升序）。
    pub fn path_to(&self, node_id: Option<NodeId>) -> Vec<NodeId> {
        let mut path = Vec::new();
        let mut cur = node_id;
        while let Some(id) = cur {
            path.push(id);
            cur = self.nodes.get(&id).and_then(|n| n.parent);
        }
        path.reverse();
        path
    }

    /// 将 root → head 路径线性化为消息列表，供 API 调用。
    pub fn linearize(&self) -> Vec<Message> {
        self.path_to_head()
            .iter()
            .filter_map(|id| self.nodes.get(id))
            .map(|n| n.message.clone())
            .collect()
    }

    /// 树中节点总数（含所有分支）。
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

// ═════════════════════════════════════════════════════════════
//                          测试
// ═════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Message { Message::user(s) }
    fn a(s: &str) -> Message { Message::assistant(Some(s), None::<&str>, None) }

    // ── 基础操作 ──

    #[test]
    fn empty_tree() {
        let tree = ConversationTree::new();
        assert!(tree.is_empty());
        assert_eq!(tree.head(), None);
        assert_eq!(tree.head_role(), None);
        assert!(tree.linearize().is_empty());
        assert!(tree.path_to_head().is_empty());
    }

    #[test]
    fn single_append() {
        let mut tree = ConversationTree::new();
        let id = tree.append(u("hello"), 1);

        assert_eq!(tree.len(), 1);
        assert_eq!(tree.head(), Some(id));
        assert_eq!(tree.head_role(), Some("user"));

        let msgs = tree.linearize();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content.as_deref(), Some("hello"));
    }

    #[test]
    fn linear_append_preserves_order() {
        let mut tree = ConversationTree::new();
        tree.append(u("msg1"), 1);
        tree.append(a("msg2"), 1);
        tree.append(u("msg3"), 2);
        tree.append(a("msg4"), 2);

        let msgs = tree.linearize();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0].content.as_deref(), Some("msg1"));
        assert_eq!(msgs[1].content.as_deref(), Some("msg2"));
        assert_eq!(msgs[2].content.as_deref(), Some("msg3"));
        assert_eq!(msgs[3].content.as_deref(), Some("msg4"));
    }

    // ── checkout ──

    #[test]
    fn checkout_to_earlier_node() {
        let mut tree = ConversationTree::new();
        let n1 = tree.append(u("msg1"), 1);
        let _n2 = tree.append(a("msg2"), 1);
        let _n3 = tree.append(u("msg3"), 2);

        tree.checkout(n1).unwrap();
        assert_eq!(tree.head(), Some(n1));

        let msgs = tree.linearize();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content.as_deref(), Some("msg1"));
    }

    #[test]
    fn checkout_nonexistent_node_returns_error() {
        let mut tree = ConversationTree::new();
        tree.append(u("msg1"), 1);

        assert!(tree.checkout(999).is_err());
    }

    // ── 分支 ──

    #[test]
    fn branch_after_checkout() {
        let mut tree = ConversationTree::new();
        let n1 = tree.append(u("question"), 1);
        let _n2 = tree.append(a("answer_A"), 1);

        // 回退到 n1，从同一个问题出发走不同分支
        tree.checkout(n1).unwrap();
        let n3 = tree.append(a("answer_B"), 2);

        // 当前路径是 n1 → n3
        let msgs = tree.linearize();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content.as_deref(), Some("question"));
        assert_eq!(msgs[1].content.as_deref(), Some("answer_B"));

        // 树里仍有 3 个节点（原路径没有丢失）
        assert_eq!(tree.len(), 3);

        // 切回 n2 路径依然可用
        tree.checkout(n1).unwrap();
        tree.checkout(_n2).unwrap();
        let msgs_a = tree.linearize();
        assert_eq!(msgs_a[1].content.as_deref(), Some("answer_A"));

        // 切回 n3 路径
        tree.checkout(n3).unwrap();
        let msgs_b = tree.linearize();
        assert_eq!(msgs_b[1].content.as_deref(), Some("answer_B"));
    }

    // ── path_to ──

    #[test]
    fn path_to_returns_correct_sequence() {
        let mut tree = ConversationTree::new();
        let n1 = tree.append(u("a"), 1);
        let n2 = tree.append(a("b"), 1);
        let n3 = tree.append(u("c"), 2);

        assert_eq!(tree.path_to_head(), vec![n1, n2, n3]);
    }

    #[test]
    fn path_to_arbitrary_node() {
        let mut tree = ConversationTree::new();
        let n1 = tree.append(u("a"), 1);
        let n2 = tree.append(a("b"), 1);
        let _n3 = tree.append(u("c"), 2);

        // n2 的路径只到 n2
        assert_eq!(tree.path_to(Some(n2)), vec![n1, n2]);
    }

    // ── get_node ──

    #[test]
    fn get_node_returns_correct_message() {
        let mut tree = ConversationTree::new();
        let id = tree.append(u("hello"), 5);

        let node = tree.get_node(id).unwrap();
        assert_eq!(node.turn_id, 5);
        assert_eq!(node.parent, None);
        assert_eq!(node.message.content.as_deref(), Some("hello"));
    }

    #[test]
    fn get_node_parent_links_are_correct() {
        let mut tree = ConversationTree::new();
        let n1 = tree.append(u("a"), 1);
        let n2 = tree.append(a("b"), 1);
        let n3 = tree.append(u("c"), 2);

        assert_eq!(tree.get_node(n1).unwrap().parent, None);
        assert_eq!(tree.get_node(n2).unwrap().parent, Some(n1));
        assert_eq!(tree.get_node(n3).unwrap().parent, Some(n2));
    }

    // ── 连续 checkout + append（模拟重说场景）──

    #[test]
    fn regenerate_pattern() {
        let mut tree = ConversationTree::new();
        let n_user = tree.append(u("问题"), 1);
        let _n_assistant_v1 = tree.append(a("回答v1"), 1);

        // 重说：checkout 到 user 节点，追加新回答
        tree.checkout(n_user).unwrap();
        let n_assistant_v2 = tree.append(a("回答v2"), 2);

        let msgs = tree.linearize();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].content.as_deref(), Some("回答v2"));
        assert_eq!(tree.head(), Some(n_assistant_v2));

        // 原来的 v1 分支依然存在
        assert_eq!(tree.len(), 3);
    }
}
