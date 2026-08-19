# core_ai_client 文档索引

> 更新日期：2026-08-19

## 状态含义

| 状态 | 含义 |
| --- | --- |
| `现行` | 当前有效，可直接作为判断依据 |
| `待复核` | 写作时正确，但已长期未随实现更新，引用前必须对照源码 |
| `归档` | 已完成工作的过程记录，只用于追溯 |

## 文档

| 文档 | 状态 | 日期 | 说明 |
| --- | --- | --- | --- |
| [client_error.md](client_error.md) | 现行 | 2026-05-24 | `ClientError` 错误码索引，含载体类型、序列化方式与 anyhow 互转规则。**行号引用基准为 commit `13a2165`，大改后需重新核对**。修改错误码或迁移构造位置时同步更新本文件 |
| [orchestrator.md](orchestrator.md) | 待复核 | 2026-04-25 | 装配层（Orchestrator）设计与用法，自述「描述当前实现，可直接对照源码阅读」。含 4 处未完成标记 |
| [API_QUICK_REFERENCE.md](API_QUICK_REFERENCE.md) | 待复核 | 2026-04-25 | 插件管理 API 快速参考：`list_all_plugins` 等核心方法签名 |
| [PLUGIN_MANAGEMENT_IMPLEMENTATION.md](PLUGIN_MANAGEMENT_IMPLEMENTATION.md) | 归档 | 2026-04-25 | `FlowCloudAIClient` 插件生命周期管理的实现总结（元数据查询、动态安装、安全卸载） |

> 标为 `待复核` 的 3 篇均停在 2026-04-25，此后未随实现复核过。

## 相关的跨仓文档（在根 `docs/`）

- `docs/agreement-version-1.md` — 插件协议 `agreement-version = 1` 规范本体。同时约束 `tool_fcplug` 与 `plugins`，因此留在根目录
- `docs/devlog/2026-06-04-移动端插件-页对齐-sigabrt.md` — 移动端 wasmtime 页对齐 `SIGABRT` 根因。**结论是硬约束：移动端目标必须用 Pulley 解释器，不能用 JIT**（iOS 禁 JIT / 16KB 页）
- `docs/archive/architecture-2026-05/Architecture_Core_AI_Client_Audit.md` — 2026-06-12 全库架构审查（归档）
