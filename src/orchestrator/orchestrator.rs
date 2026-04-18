use anyhow::Result;
use std::sync::Arc;

use crate::orchestrator::context::{AssembledTurn, TaskContext};
use crate::orchestrator::orchestrate::Orchestrate;
use crate::tool::registry::ToolRegistry;

// ═════════════════════════════════════════════════════════════
//                    DefaultOrchestrator
// ═════════════════════════════════════════════════════════════

/// 内置默认编排器。
///
/// 不持有 `Sense`——`Sense` 只通过 `session.load_sense()` 进入静态配置层，
/// 两者职责不重叠：
/// - `Sense`：系统提示、工具安装、默认参数（静态声明，初始化时一次性写入）
/// - `DefaultOrchestrator`：每轮装配（上下文注入、工具裁剪、参数覆盖）
///
/// # 工具白名单
/// 若 `Sense` 声明了白名单，在创建 `DefaultOrchestrator` 时显式传入：
/// ```rust
/// DefaultOrchestrator::new(registry)
///     .with_whitelist(sense.tool_whitelist())
/// ```
///
/// # 工具决策优先级
/// `Sense::tool_whitelist()` 提供静态默认范围（初始化时传入） →
/// `DefaultOrchestrator` 在此范围内做每轮最终裁决 →
/// `AssembledTurn::tool_schemas` 是 Session 实际使用的值。
pub struct DefaultOrchestrator {
    /// 全局工具库（共享引用）
    registry: Arc<ToolRegistry>,

    /// 工具白名单（`None` = 启用 registry 全量工具）。
    /// 通常从 `Sense::tool_whitelist()` 获取后通过 `with_whitelist` 传入。
    whitelist: Option<Vec<String>>,
}

impl DefaultOrchestrator {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self {
            registry,
            whitelist: None,
        }
    }

    /// 设置工具白名单（builder 风格）。
    ///
    /// 传 `None` 表示不限制（使用全量工具）；
    /// 传 `Some(vec)` 表示仅启用指定名称的工具。
    ///
    /// 推荐用法：
    /// ```rust
    /// DefaultOrchestrator::new(registry).with_whitelist(sense.tool_whitelist())
    /// ```
    pub fn with_whitelist(mut self, whitelist: Option<Vec<String>>) -> Self {
        self.whitelist = whitelist;
        self
    }

    // ── 内部步骤 ──

    fn inject_context(&self, ctx: &TaskContext, turn: &mut AssembledTurn) {
        // 遗留字段注入
        if !ctx.task_type.is_empty() {
            turn.context_messages
                .push(format!("[Task type: {}]", ctx.task_type));
        }
        if let Some(ref sel) = ctx.selection {
            turn.context_messages
                .push(format!("[Current selection]\n{}", sel));
        }
        if !ctx.entities.is_empty() {
            turn.context_messages
                .push(format!("[Related entities: {}]", ctx.entities.join(", ")));
        }

        // 中性 attributes 注入
        for (k, v) in &ctx.attributes {
            turn.context_messages.push(format!("[{}]\n{}", k, v));
        }
    }

    /// 工具筛选：白名单为 None 时启用全量工具，有白名单时取交集。
    ///
    /// `AssembledTurn::tool_schemas` 三态约定：
    /// - `None`         → 不干预，沿用 Session 当前配置（此方法不会返回 None，见下）
    /// - `Some(vec![])` → 显式禁用全部工具
    /// - `Some(schemas)` → 覆盖为给定工具集
    fn select_tools(&self, turn: &mut AssembledTurn) {
        match &self.whitelist {
            Some(whitelist) => {
                let enabled: Vec<String> = whitelist
                    .iter()
                    .filter(|name| self.registry.has_tool(name))
                    .cloned()
                    .collect();
                turn.tool_schemas = self.registry.schemas_filtered(&enabled);
                turn.enabled_tools = enabled;
            }
            None => {
                turn.tool_schemas = self.registry.schemas();
                turn.enabled_tools = self.registry.tool_names();
            }
        }
    }

    fn apply_overrides(&self, ctx: &TaskContext, turn: &mut AssembledTurn) {
        // read_only 优先级：
        //   1. flags["read_only"] 存在 → 以它为准
        //   2. 无 flags 但有遗留字段 → 走兼容回退
        //   3. 均未设置 → false（AssembledTurn::default()）
        turn.read_only = ctx
            .flags
            .get("read_only")
            .copied()
            .unwrap_or(ctx.read_only);

        // 参数覆盖：根据任务类型选择最佳参数
        match ctx.task_type.as_str() {
            "creative_writing" => {
                turn.temperature_override = Some(0.85);
            }
            "proofreading" | "translation" => {
                turn.temperature_override = Some(0.1);
            }
            "code_generation" => {
                turn.temperature_override = Some(0.0);
            }
            _ => {}
        }
    }
}

impl Orchestrate for DefaultOrchestrator {
    fn assemble(&self, ctx: &TaskContext) -> Result<AssembledTurn> {
        let mut turn = AssembledTurn::default();
        self.inject_context(ctx, &mut turn);
        self.select_tools(&mut turn);
        self.apply_overrides(ctx, &mut turn);
        Ok(turn)
    }
}
