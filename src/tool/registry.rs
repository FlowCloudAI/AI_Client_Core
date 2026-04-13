use crate::llm::types::ToolFunctionArg;
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

// ─────────────────────── 辅助函数 ───────────────────────────

pub fn arg_i32(args: &Value, key: &str) -> anyhow::Result<i32> {
    args.get(key)
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
        .ok_or_else(|| anyhow::anyhow!("缺少或非法参数: {}", key))
}

pub fn arg_str<'a>(args: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("缺少或非法参数: {}", key))
}

// ─────────────────────── Handler 类型 ───────────────────────

/// 关键变化：`&ToolRegistry`（不是 `&mut`）。
/// 因为 state 已经在 `Arc<Mutex<T>>` 后面，handler 只需要 &self 就能拿到 state。
type Handler = Arc<
    dyn for<'a> Fn(&'a ToolRegistry, &'a Value) -> BoxFuture<'a, anyhow::Result<String>>
    + Send
    + Sync,
>;

// ─────────────────────── ToolSpec ────────────────────────────

pub struct ToolSpec {
    pub schema: Value,
    handler: Handler,
    pub enabled: bool,
}

// ─────────────────────── ToolRegistry ────────────────────────

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
            .ok_or_else(|| anyhow::anyhow!("缺少状态: {}", std::any::type_name::<T>()))
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
            let arc = reg
                .state_or_err::<crate::sense::SenseState<T>>()
                .map(|x| x.clone());
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
            let arc = reg
                .state_or_err::<crate::sense::SenseState<T>>()
                .map(|x| x.clone());
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
            .filter(|x| x.enabled)
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
        let v: Vec<_> = whitelist
            .iter()
            .filter_map(|name| self.tools.get(name))
            .filter(|spec| spec.enabled)
            .map(|spec| spec.schema.clone())
            .collect();
        if v.is_empty() { None } else { Some(v) }
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
    pub fn enable_tool(&mut self, name: &str) -> bool {
        match self.tools.get_mut(name) {
            Some(spec) => {
                spec.enabled = true;
                true
            }
            None => false,
        }
    }

    /// 禁用指定工具。返回是否成功（工具存在则成功）。
    pub fn disable_tool(&mut self, name: &str) -> bool {
        match self.tools.get_mut(name) {
            Some(spec) => {
                spec.enabled = false;
                true
            }
            None => false,
        }
    }

    /// 查询指定工具是否启用（工具不存在视为未启用）。
    pub fn is_enabled(&self, name: &str) -> bool {
        self.tools
            .get(name)
            .map(|spec| spec.enabled)
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

        let handler = match self.tools.get(func_name) {
            Some(spec) => {
                if !spec.enabled {
                    anyhow::bail!("工具已禁用: {}", func_name);
                }
                Arc::clone(&spec.handler)
            }
            None => anyhow::bail!("未知工具: {}", func_name),
        };

        match tokio::time::timeout(timeout, handler(self, args)).await {
            Ok(res) => res,
            Err(_) => anyhow::bail!("工具执行超时: {}", func_name),
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
        println!("[debug] inserting tool: {}", name);

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
                enabled: true,
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