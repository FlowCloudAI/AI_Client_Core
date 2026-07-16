pub mod registry;

pub use registry::{ToolAccess, ToolRegistry, arg_i32, arg_str};

tokio::task_local! {
    static AUTO_CONFIRM_WRITES: bool;
}

pub(crate) async fn with_auto_confirm_writes<F, T>(enabled: bool, future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    AUTO_CONFIRM_WRITES.scope(enabled, future).await
}

pub fn auto_confirm_writes_enabled() -> bool {
    AUTO_CONFIRM_WRITES
        .try_with(|enabled| *enabled)
        .unwrap_or(false)
}
