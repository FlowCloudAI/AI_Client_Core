pub(crate) mod plugin_bindings {
    wasmtime::component::bindgen!({
        path: "wit/plugin.wit",
        world: "api",
    });
}