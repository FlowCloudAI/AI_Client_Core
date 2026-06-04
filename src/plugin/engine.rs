use crate::error::{ClientError, ErrorCode};
use wasmtime::{Config, Engine};

/// 构建插件用的 wasmtime 引擎。
///
/// 移动端（Android / iOS）强制走 **Pulley 解释器**，而非 Cranelift 原生 JIT：
/// - Android 新机与新模拟器镜像普遍使用 16KB（部分 arm64 内核甚至 64KB）内存页。
///   JIT 把编译出的机器码写进 mmap 后，要按整页把代码段标记为可执行，但代码段起点
///   是按 4KB 对齐的，与运行期页大小对不齐，wasmtime 会在 `Mmap::make_executable`
///   里 `panic`；该 panic 跨过 C/FFI 边界无法回退栈，直接 `abort()`（SIGABRT），
///   `catch_unwind` 也兜不住。
/// - iOS 不允许第三方应用申请可执行内存（无 JIT）。
///
/// Pulley 把 wasm 编译成可移植字节码并解释执行，编译产物的代码段带有
/// `SH_WASMTIME_NOT_EXECUTED` 标记，wasmtime 因此**完全跳过**置为可执行那一步，
/// 与页大小无关，也满足 iOS 限制。代价是执行更慢，但 LLM / Image / TTS 插件的热点
/// 在远端网络调用，本地映射逻辑很轻，几乎无感。
///
/// 桌面端保持 Cranelift 原生 JIT，以获得最佳性能。
pub(crate) fn build_plugin_engine() -> anyhow::Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);

    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        // Pulley 目标的指针宽度与字节序必须与宿主一致，否则 wasmtime 会判定与宿主不兼容
        // 而拒绝运行（见 wasmtime engine.rs 的 check_compatible_with_native_host）。
        let pulley_target = if cfg!(target_pointer_width = "64") {
            "pulley64"
        } else {
            "pulley32"
        };
        config.target(pulley_target).map_err(|e| {
            ClientError::new(ErrorCode::CoreClientInitFailed, "设置 Pulley 目标失败")
                .with_kv("target", pulley_target)
                .with_kv("source", e.to_string())
        })?;
    }

    let engine = Engine::new(&config).map_err(|e| {
        ClientError::new(ErrorCode::CoreClientInitFailed, "创建 WebAssembly 引擎失败")
            .with_kv("source", e.to_string())
    })?;
    Ok(engine)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证本次构建确实接通了 Pulley：能以 pulley 目标建引擎，并用 Pulley 后端把组件
    /// 编成可移植字节码、走完发布流程。这是移动端不再 SIGABRT 的地基——若依赖特性没接通
    /// （缺 all-arch / pulley），这里会在宿主上先于真机暴露。
    #[test]
    fn pulley_target_compiles_component() {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let pulley_target = if cfg!(target_pointer_width = "64") {
            "pulley64"
        } else {
            "pulley32"
        };
        config.target(pulley_target).expect("配置 Pulley 目标");
        let engine = Engine::new(&config).expect("创建 Pulley 引擎");
        // 空组件足以触发编译 + 发布路径；若 Pulley 仍要求可执行内存，这里就会崩。
        wasmtime::component::Component::new(&engine, "(component)")
            .expect("用 Pulley 后端编译空组件");
    }
}
