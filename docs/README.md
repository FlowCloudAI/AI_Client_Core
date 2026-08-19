# core_ai_client 文档索引

> 更新日期：2026-08-19
>
> 本目录 3 篇文档全部停在 2026-04-25，**均未针对此后的实现变更做过复核**。引用前请对照源码确认。

## 状态含义

| 状态 | 含义 |
| --- | --- |
| `现行` | 当前有效，可直接作为判断依据 |
| `待复核` | 写作时正确，但已长期未随实现更新，引用前必须对照源码 |
| `归档` | 已完成工作的过程记录，只用于追溯 |

## 文档

| 文档 | 状态 | 日期 | 说明 |
| --- | --- | --- | --- |
| [orchestrator.md](orchestrator.md) | 待复核 | 2026-04-25 | 装配层（Orchestrator）设计与用法，自述「描述当前实现，可直接对照源码阅读」。含 4 处未完成标记。本目录质量最高的一篇 |
| [API_QUICK_REFERENCE.md](API_QUICK_REFERENCE.md) | 待复核 | 2026-04-25 | 插件管理 API 快速参考：`list_all_plugins` 等核心方法签名 |
| [PLUGIN_MANAGEMENT_IMPLEMENTATION.md](PLUGIN_MANAGEMENT_IMPLEMENTATION.md) | 归档 | 2026-04-25 | `FlowCloudAIClient` 插件生命周期管理的实现总结（元数据查询、动态安装、安全卸载） |

## 相关的跨仓文档（在根 `docs/`）

- `docs/client_error.md` — `ClientError` 错误码索引。行号基准为 commit `13a2165`，大改后需重新核对。**P2 下沉到本目录**
- `docs/agreement-version-1.md` — 插件协议 `agreement-version = 1` 规范本体。跨 `tool_fcplug` / `core_ai_client` / `plugins`，留在根目录
- `docs/mobile_plugin_pulley_issue.md` — 移动端 wasmtime 页对齐 `SIGABRT` 根因。**结论是硬约束：移动端目标必须用 Pulley 解释器，不能用 JIT**
- `docs/Architecture_Core_AI_Client_Audit.md` — 2026-06-12 全库架构审查（归档）
