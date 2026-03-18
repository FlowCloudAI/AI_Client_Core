use futures_util::future::BoxFuture;
use serde_json::Value;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;
use crate::llm::sense::SenseState;
use crate::llm::types::ToolFunctionArg;

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

type Handler = Arc<
    dyn for<'a> Fn(&'a mut ToolFunctions, &'a Value) -> BoxFuture<'a, anyhow::Result<String>>
    + Send
    + Sync,
>;

pub struct ToolSpec {
    pub schema: Value,
    pub handler: Handler,
}

pub struct ToolFunctions {
    registry: HashMap<String, ToolSpec>,
    state: HashMap<TypeId, Box<dyn Any + Send + Sync + 'static>>,
}

impl ToolFunctions {
    pub fn new() -> Self {
        let this = Self {
            registry: HashMap::new(),
            state: HashMap::new(),
        };
        this
    }

    pub fn state_or_err<T: Any + Send + 'static>(&self) -> anyhow::Result<&T> {
        self.state
            .get(&TypeId::of::<T>())
            .and_then(|b| b.downcast_ref::<T>())
            .ok_or_else(|| anyhow::anyhow!("缺少状态: {}", std::any::type_name::<T>()))
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

    fn schema_required(properties: Option<Vec<ToolFunctionArg>>) -> (Vec<String>, Option<Vec<ToolFunctionArg>>) {
        let mut required = Vec::new();

        if let Some(ref props) = properties {
            for a in props {
                if a.required.unwrap_or(false) {
                    required.push(a.name.clone());
                }
            }
        }

        //println!("required: {:?}", required);

        (required, properties)
    }
    
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

        let wrapped: Handler = Arc::new(move |tf, args| {
            let arc = tf.state_or_err::<SenseState<T>>().map(|x| x.clone());
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

        self._register(name, description, properties, required, wrapped);
    }
    
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

        let wrapped: Handler = Arc::new(move |tf, args| {
            let arc = tf.state_or_err::<SenseState<T>>().map(|x| x.clone());
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

        self._register(name, description, properties, required, wrapped);
    }

    fn _register(
        &mut self,
        name: &str,
        description: &str,
        properties: Option<Value>,
        required: Vec<String>,
        handler: Handler,
    ) {
        let pros = properties.unwrap_or(serde_json::json!({}));

        self.registry.insert(
            name.to_string(),
            ToolSpec {
                schema: serde_json::json!({
                    "type":"function",
                    "function":{
                        "name":name,
                        "description":description,
                        "parameters":{
                            "type":"object",
                            "properties":pros,
                            "required":required
                        }
                    }
                }),
                handler,
            },
        );
    }
    
    pub fn put_state<T: Any + Send + 'static + Sync>(&mut self, v: T) {
        self.state.insert(TypeId::of::<T>(), Box::new(v));
    }
    
    pub fn schemas(&self) -> Option<Vec<Value>> {
        if self.registry.is_empty() {
            return None;
        }
        let mut v: Vec<_> = self.registry.values().map(|x| x.schema.clone()).collect();
        v.sort_by_key(|s| s["function"]["name"].as_str().unwrap_or("").to_string());
        Some(v)
    }
    
    pub async fn conduct(
        &mut self,
        func_name: &str,
        args: Option<&Value>,
    ) -> anyhow::Result<String> {
        let empty = serde_json::json!({});
        let args = args.unwrap_or(&empty);

        // ✅ 先把 handler clone 出来，结束对 registry 的借用
        let handler = match self.registry.get(func_name) {
            Some(spec) => Arc::clone(&spec.handler),
            None => anyhow::bail!("未知工具"),
        };

        // ✅ 现在可以安全地 &mut self
        let result = handler(self, args).await;

        result
    }
}
