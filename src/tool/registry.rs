use crate::error::{ClientError, ErrorCode};
use crate::llm::types::ToolFunctionArg;
use futures_util::{FutureExt, future::BoxFuture};
use serde_json::Value;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// ─────────────────────── 辅助函数 ───────────────────────────

pub fn arg_i32(args: &Value, key: &str) -> anyhow::Result<i32> {
    args.get(key)
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
        .ok_or_else(|| {
            ClientError::new(
                ErrorCode::LlmToolCallInvalid,
                format!("缺少或非法参数: {}", key),
            )
            .with_kv("field", key.to_string())
            .into()
        })
}

pub fn arg_str<'a>(args: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    args.get(key).and_then(|v| v.as_str()).ok_or_else(|| {
        ClientError::new(
            ErrorCode::LlmToolCallInvalid,
            format!("缺少或非法参数: {}", key),
        )
        .with_kv("field", key.to_string())
        .into()
    })
}

// ─────────────────────── Handler 类型 ───────────────────────

/// 关键变化：`&ToolRegistry`（不是 `&mut`）。
/// 因为 state 已经在 `Arc<Mutex<T>>` 后面，handler 只需要 &self 就能拿到 state。
type Handler = Arc<
    dyn for<'a> Fn(&'a ToolRegistry, &'a Value) -> BoxFuture<'a, anyhow::Result<String>>
        + Send
        + Sync,
>;

// ─────────────────────── 工具规格 ──────────────────────────

/// 工具读写属性（显式标注，不做名字嗅探）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolAccess {
    Read,
    Write,
}

pub struct ToolSpec {
    pub schema: Value,
    handler: Handler,
    enabled: AtomicBool,
    /// None = 未标注。read_only 会话中未标注按写处理（禁止）。
    access: Option<ToolAccess>,
    /// 等待用户主动操作的工具不受统一执行时限约束。
    interactive: bool,
}

// ─────────────────────── 工具注册中心 ───────────────────────

/// 全局工具注册中心。
///
/// 与旧 `ToolFunctions` 的区别：
/// - handler 签名是 `&ToolRegistry`（不是 `&mut`），因此 `conduct` 只需 `&self`
/// - 可以用 `Arc<ToolRegistry>` 在多个 session 间共享
/// - 所有权归 `FlowCloudAIClient`，Session 通过 `Arc` 引用
pub struct ToolRegistry {
    tools: HashMap<String, ToolSpec>,
    state: HashMap<TypeId, Box<dyn Any + Send + Sync + 'static>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            state: HashMap::new(),
        }
    }

    // ── 状态管理 ──

    pub fn put_state<T: Any + Send + Sync + 'static>(&mut self, v: T) {
        self.state.insert(TypeId::of::<T>(), Box::new(v));
    }

    pub fn state_or_err<T: Any + Send + 'static>(&self) -> anyhow::Result<&T> {
        self.state
            .get(&TypeId::of::<T>())
            .and_then(|b| b.downcast_ref::<T>())
            .ok_or_else(|| {
                ClientError::new(
                    ErrorCode::CoreClientInternalError,
                    format!("缺少状态: {}", std::any::type_name::<T>()),
                )
                .with_kv("type", std::any::type_name::<T>().to_string())
                .into()
            })
    }

    // ── 工具注册（同步 handler） ──

    pub fn register<T, F>(
        &mut self,
        name: &str,
        description: &str,
        properties: impl Into<Option<Vec<ToolFunctionArg>>>,
        handler: F,
    ) where
        T: Any + Send + 'static,
        F: Fn(&mut T, &Value) -> anyhow::Result<String> + Send + Sync + 'static,
    {
        let handler = Arc::new(handler);

        let wrapped: Handler = Arc::new(move |reg, args| {
            let arc = reg.state_or_err::<crate::sense::SenseState<T>>().cloned();
            let handler = Arc::clone(&handler);

            Box::pin(async move {
                let arc = arc?;
                let mut state = arc.lock().await;
                handler(&mut *state, args)
            })
        });

        let props_vec: Option<Vec<ToolFunctionArg>> = properties.into();
        let (required, props_vec) = Self::schema_required(props_vec);
        let properties = Self::schema_properties(props_vec);

        self.insert_tool(name, description, properties, required, wrapped);
    }

    // ── 工具注册（异步 handler） ──

    pub fn register_async<T, F>(
        &mut self,
        name: &str,
        description: &str,
        properties: impl Into<Option<Vec<ToolFunctionArg>>>,
        handler: F,
    ) where
        T: Any + Send + 'static,
        F: for<'a> Fn(&'a mut T, &'a Value) -> BoxFuture<'a, anyhow::Result<String>>
            + Send
            + Sync
            + 'static,
    {
        let handler = Arc::new(handler);

        let wrapped: Handler = Arc::new(move |reg, args| {
            let arc = reg.state_or_err::<crate::sense::SenseState<T>>().cloned();
            let handler = Arc::clone(&handler);

            Box::pin(async move {
                let arc = arc?;
                let mut state = arc.lock().await;
                handler(&mut *state, args).await
            })
        });

        let props_vec: Option<Vec<ToolFunctionArg>> = properties.into();
        let (required, props_vec) = Self::schema_required(props_vec);
        let properties = Self::schema_properties(props_vec);

        self.insert_tool(name, description, properties, required, wrapped);
    }

    // ── Schema 查询 ──

    /// 获取所有已启用工具的 JSON Schema。
    pub fn schemas(&self) -> Option<Vec<Value>> {
        let mut v: Vec<_> = self
            .tools
            .values()
            .filter(|x| x.enabled.load(Ordering::SeqCst))
            .map(|x| x.schema.clone())
            .collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by_key(|s| s["function"]["name"].as_str().unwrap_or("").to_string());
        Some(v)
    }

    /// 只获取指定工具名的 Schema（白名单筛选），且仅返回启用的工具。
    pub fn schemas_filtered(&self, whitelist: &[String]) -> Option<Vec<Value>> {
        let v = self.schemas_filtered_strict(whitelist);
        if v.is_empty() { None } else { Some(v) }
    }

    /// 严格白名单筛选。
    ///
    /// 与兼容方法不同，空白名单或全无效白名单会返回空 Vec，
    /// 由调用方包装为 `Some(vec![])` 表示“显式禁用全部工具”。
    pub fn schemas_filtered_strict(&self, whitelist: &[String]) -> Vec<Value> {
        whitelist
            .iter()
            .filter_map(|name| self.tools.get(name))
            .filter(|spec| spec.enabled.load(Ordering::SeqCst))
            .map(|spec| spec.schema.clone())
            .collect()
    }

    /// 获取所有已注册的工具名。
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// 是否有指定工具。
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// 启用指定工具。返回是否成功（工具存在则成功）。
    pub fn enable_tool(&self, name: &str) -> bool {
        match self.tools.get(name) {
            Some(spec) => {
                spec.enabled.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    /// 禁用指定工具。返回是否成功（工具存在则成功）。
    pub fn disable_tool(&self, name: &str) -> bool {
        match self.tools.get(name) {
            Some(spec) => {
                spec.enabled.store(false, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    // ── 读写标注 ──
    //
    // 三层开关优先级：disable_tool（全局禁用）> read_only 拦截 > 每轮白名单裁剪。
    // 标注与 auto_confirm_writes（task_local，工具 handler 内部消费）互不影响。

    /// 批量标注为读工具（read_only 会话放行）。名字不存在时告警，暴露拼写漂移。
    pub fn mark_read(&mut self, names: &[&str]) {
        self.mark_access(names, ToolAccess::Read);
    }

    /// 批量标注为写工具。与未标注行为一致（read_only 禁止），价值在于显式文档化。
    pub fn mark_write(&mut self, names: &[&str]) {
        self.mark_access(names, ToolAccess::Write);
    }

    /// 标记会等待用户主动操作的工具；调用方负责在取消会话时结束其 future。
    pub fn mark_interactive(&mut self, names: &[&str]) {
        for name in names {
            match self.tools.get_mut(*name) {
                Some(spec) => spec.interactive = true,
                None => log::warn!("[tool] mark_interactive 目标不存在: name={}", name),
            }
        }
    }

    fn mark_access(&mut self, names: &[&str], access: ToolAccess) {
        for name in names {
            match self.tools.get_mut(*name) {
                Some(spec) => spec.access = Some(access),
                None => log::warn!(
                    "[tool] mark_access 目标不存在: name={} access={:?}",
                    name,
                    access
                ),
            }
        }
    }

    /// 是否为显式标注的读工具。未标注 / Write / 不存在均返回 false——
    /// read_only 会话据此拦截，安全边界不做名字猜测。
    pub fn is_read_tool(&self, name: &str) -> bool {
        self.tools
            .get(name)
            .is_some_and(|spec| spec.access == Some(ToolAccess::Read))
    }

    /// 查询指定工具是否启用（工具不存在视为未启用）。
    pub fn is_enabled(&self, name: &str) -> bool {
        self.tools
            .get(name)
            .map(|spec| spec.enabled.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    // ── 工具执行 ──

    /// 执行工具调用。注意：只需要 `&self`。
    ///
    /// 这是和旧 `ToolFunctions::conduct` 的关键区别：
    /// 不需要 `&mut self`，因此 `Arc<ToolRegistry>` 可以直接并发使用。
    pub async fn conduct(
        &self,
        func_name: &str,
        args: Option<&Value>,
        timeout: Duration,
    ) -> anyhow::Result<String> {
        let empty = serde_json::json!({});
        let args = args.unwrap_or(&empty);

        let (handler, interactive) = match self.tools.get(func_name) {
            Some(spec) => {
                if !spec.enabled.load(Ordering::SeqCst) {
                    return Err(ClientError::new(
                        ErrorCode::ToolDisabled,
                        format!("工具已禁用: {}", func_name),
                    )
                    .with_kv("tool_id", func_name.to_string())
                    .into());
                }
                (Arc::clone(&spec.handler), spec.interactive)
            }
            None => {
                return Err(ClientError::new(
                    ErrorCode::ToolNotFound,
                    format!("未知工具: {}", func_name),
                )
                .with_kv("tool_id", func_name.to_string())
                .into());
            }
        };

        let guarded = std::panic::AssertUnwindSafe(handler(self, args)).catch_unwind();
        let execution = if interactive {
            guarded.await
        } else {
            match tokio::time::timeout(timeout, guarded).await {
                Ok(result) => result,
                Err(_) => {
                    return Err(ClientError::new(
                        ErrorCode::LlmToolCallTimeout,
                        format!("工具执行超时: {}", func_name),
                    )
                    .with_kv("tool_id", func_name.to_string())
                    .with_kv("timeout_ms", timeout.as_millis() as u64)
                    .into());
                }
            }
        };
        match execution {
            Ok(result) => result,
            Err(_) => Err(ClientError::new(
                ErrorCode::CoreClientInternalError,
                format!("工具内部异常: {}", func_name),
            )
            .with_kv("tool_id", func_name.to_string())
            .into()),
        }
    }

    // ── 内部方法 ──

    fn insert_tool(
        &mut self,
        name: &str,
        description: &str,
        properties: Option<Value>,
        required: Vec<String>,
        handler: Handler,
    ) {
        log::debug!("[tool] inserting tool: {}", name);

        let pros = properties.unwrap_or(serde_json::json!({}));

        self.tools.insert(
            name.to_string(),
            ToolSpec {
                schema: serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": description,
                        "parameters": {
                            "type": "object",
                            "properties": pros,
                            "required": required
                        }
                    }
                }),
                handler,
                enabled: AtomicBool::new(true),
                access: None,
                interactive: false,
            },
        );
    }

    fn schema_properties(properties: Option<Vec<ToolFunctionArg>>) -> Option<Value> {
        properties.map(|x| {
            let mut v = serde_json::json!({});
            for arg in x {
                v[arg.name] = arg.schema();
            }
            v
        })
    }

    fn schema_required(
        properties: Option<Vec<ToolFunctionArg>>,
    ) -> (Vec<String>, Option<Vec<ToolFunctionArg>>) {
        let mut required = Vec::new();
        if let Some(ref props) = properties {
            for a in props {
                if a.required.unwrap_or(false) {
                    required.push(a.name.clone());
                }
            }
        }
        (required, properties)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sense::sense_state_new;

    fn registry_with_tool() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.put_state(sense_state_new::<()>());
        registry.register::<(), _>(
            "alpha",
            "测试工具 alpha",
            None::<Vec<ToolFunctionArg>>,
            |_state, _args| Ok("alpha".to_string()),
        );
        registry
    }

    #[test]
    fn strict_filtered_keeps_empty_result() {
        let registry = registry_with_tool();

        assert!(registry.schemas_filtered_strict(&[]).is_empty());
        assert!(
            registry
                .schemas_filtered_strict(&["missing".to_string()])
                .is_empty()
        );
    }

    #[test]
    fn compat_filtered_still_returns_none_for_empty_result() {
        let registry = registry_with_tool();

        assert!(registry.schemas_filtered(&[]).is_none());
    }

    #[test]
    fn tool_arg_schema_keeps_descriptions_optional_required_and_structured_constraints() {
        let mut registry = ToolRegistry::new();
        registry.put_state(sense_state_new::<()>());
        registry.register::<(), _>(
            "schema_probe",
            "测试工具 schema",
            vec![
                ToolFunctionArg::new("name", "string")
                    .required(true)
                    .desc("名称"),
                ToolFunctionArg::new("mode", "string")
                    .desc("模式")
                    .enum_values(["preview", "apply"]),
                ToolFunctionArg::new("items", "array")
                    .desc("条目列表")
                    .items(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" }
                        },
                        "required": ["id"],
                        "additionalProperties": false
                    })),
                ToolFunctionArg::new("callback", "string").format("uri"),
            ],
            |_state, _args| Ok("ok".to_string()),
        );

        let schema = registry.schemas().expect("应生成工具 schema");
        let params = &schema[0]["function"]["parameters"];
        assert_eq!(params["required"], serde_json::json!(["name"]));
        assert_eq!(params["properties"]["name"]["description"], "名称");
        assert_eq!(
            params["properties"]["mode"]["enum"],
            serde_json::json!(["preview", "apply"])
        );
        assert_eq!(params["properties"]["items"]["items"]["type"], "object");
        assert_eq!(params["properties"]["callback"]["format"], "uri");
    }

    #[test]
    fn unmarked_tool_is_not_read() {
        let r = registry_with_tool();
        assert!(!r.is_read_tool("alpha"));
        assert!(!r.is_read_tool("missing"));
    }

    #[test]
    fn mark_read_and_write() {
        let mut r = registry_with_tool();
        r.mark_read(&["alpha", "missing"]);
        assert!(r.is_read_tool("alpha"));
        r.mark_write(&["alpha"]);
        assert!(!r.is_read_tool("alpha"));
    }

    #[tokio::test]
    async fn handler_panic_is_converted_to_fatal_client_error() {
        let mut registry = ToolRegistry::new();
        registry.put_state(sense_state_new::<()>());
        registry.register::<(), _>(
            "panic_tool",
            "panic 测试工具",
            None::<Vec<ToolFunctionArg>>,
            |_state, _args| panic!("测试 panic"),
        );

        let error = registry
            .conduct("panic_tool", None, Duration::from_secs(1))
            .await
            .unwrap_err();
        let client_error = ClientError::from_anyhow(&error).unwrap();
        assert_eq!(client_error.code, ErrorCode::CoreClientInternalError);
    }

    #[tokio::test]
    async fn interactive_tool_is_not_cut_off_by_default_timeout() {
        let mut registry = ToolRegistry::new();
        registry.put_state(sense_state_new::<()>());
        registry.register_async::<(), _>(
            "review_tool",
            "等待用户审阅",
            None::<Vec<ToolFunctionArg>>,
            |_state, _args| {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Ok("confirmed".to_string())
                })
            },
        );
        registry.mark_interactive(&["review_tool"]);

        let result = registry
            .conduct("review_tool", None, Duration::from_millis(1))
            .await
            .unwrap();
        assert_eq!(result, "confirmed");
    }
}
