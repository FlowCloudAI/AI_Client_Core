# `flowcloudai_client::ClientError` 错误码索引

> 自动整理自 `core_ai_client/src/error.rs` 与各模块构造点。修改错误码或迁移构造位置时，请同步更新本文件。
>
> 行号引用截图于 commit `13a2165`（P3 完成时）之后；后续如有大改请重新核对。

## 1. 总览

- **载体类型**：`ClientError { code: ErrorCode, message: String, detail: serde_json::Value }`（`core_ai_client/src/error.rs`）。
- **序列化**：`Display` 输出整对象 JSON，`code` 在线缆上以 `SCREAMING_SNAKE_CASE` 表示。
- **anyhow 互转**：所有构造点都返回 `anyhow::Result`，内层为 `ClientError`；
  - 库内传递：经 `?` 透传，`anyhow::Error::downcast_ref::<ClientError>()` 还原。
  - app 边界：`crate::api_error::ApiError` 通过 `From<ClientError>` / `From<anyhow::Error>` 自动剥壳。
- **派发路径**：内部异常 → `ClientError` → `SessionEvent::Error(ClientError)` / `TurnStatus::Error(ClientError)` / `PluginLoadError.error` → app 端 `ai:error` 事件 / Tauri command Err。
- **路径符号**：本表中的源文件相对 `core_ai_client/`；行号给出主构造点（同一码常有多个等价位置）。

## 2. 命名空间分组

错误码按 `<域>_<动作>_<结果>` 命名。当前定义了 12 个域：

| 前缀 | 域 | 用途 |
| --- | --- | --- |
| `CORE_CLIENT_*` | 核心客户端 | 初始化 / 兜底 / 取消等总控异常 |
| `PLUGIN_*` | 插件 | 扫描、加载、版本、运行时 |
| `LLM_*` | LLM 会话与流式 | 会话生命周期、请求/响应、工具调用 |
| `IMAGE_*` | 图像生成 | ImageSession 任务级别 |
| `TTS_*` | 语音合成 | TTSSession 任务级别 |
| `AUDIO_*` | 音频解码 / 播放 | symphonia + cpal 链路 |
| `HTTP_*` | HTTP 传输 | 由 `HttpPoster::map_status` 按状态码分流 |
| `FS_*` | 本地文件系统 | 打开、写入、权限 |
| `AUTH_*` | 鉴权 | API Key |
| `VALIDATION_*` | 参数校验 | 字段缺失 / 格式错误 |
| `TOOL_*` | 工具注册中心 | 工具未注册 / 已禁用 |
| `NOT_IMPLEMENTED` | 兜底 | 当前版本暂不支持 |

> `detail` 字段无固定 schema，但常见约定见各小节"典型 detail 键"列。

## 3. 错误码 → 触发位置

### 3.1 核心客户端 `CORE_CLIENT_*`

| Code | 默认 message | 典型 detail 键 | 主构造点 |
| --- | --- | --- | --- |
| `CORE_CLIENT_INIT_FAILED` | 创建 WebAssembly 引擎失败 / 向 linker 注册 WASI 失败 / 构建 HTTP 客户端失败 | `source` | `plugin/manager.rs:45,51`、`plugin/registry.rs:79,84`、`http_poster.rs:50` |
| `CORE_CLIENT_INTERNAL_ERROR` | 插件注册表 / 引用计数锁中毒、缺少状态等兜底 | `source`、`type` | `plugin/registry.rs:60,68`、`tool/registry.rs:89` |
| `CORE_CLIENT_TIMEOUT` | _（声明保留，库内未直接使用）_ | — | — |
| `CORE_CLIENT_CANCELLED` | _（声明保留，app 端 `ai_summary` / `ai_fill_image_prompt` 用于 TurnStatus::Cancelled）_ | — | — |

### 3.2 插件 `PLUGIN_*`

| Code | 默认 message | 典型 detail 键 | 主构造点 |
| --- | --- | --- | --- |
| `PLUGIN_NOT_FOUND` | 插件 `'{id}'` 不存在 | `plugin_id` | `client.rs:107`、`plugin/manager.rs:149`、`plugin/pipeline.rs:114,143`、`plugin/registry.rs:175,274`、`plugin/loaded.rs:65` |
| `PLUGIN_NOT_LOADED` | 已选择插件 `'{id}'` 但未加载；插件未加载，无法 map_* | `plugin_id` | `plugin/pipeline.rs:162,211`、`plugin/registry.rs:253`、`plugin/loaded.rs:141,159,180` |
| `PLUGIN_LOAD_FAILED` | 插件包不是合法 ZIP / 缺少 plugin.wasm / Wasm 编译或实例化失败 | `plugin_id`、`source` | `plugin/loaded.rs:88,95,102,119`、`plugin/registry.rs:120,125,130,138,416`、`plugin/scanner.rs:20` |
| `PLUGIN_UNLOAD_FORBIDDEN` | 插件 `'{id}'` 仍被 N 个会话引用，无法卸载 | `plugin_id`、`ref_count` | `plugin/registry.rs:206`（`client.rs:115` 同样路径覆盖 uninstall） |
| `PLUGIN_ALREADY_EXISTS` | 插件 `'{id}'` 已存在 | `plugin_id` | `client.rs:172`、`plugin/manager.rs:127,163`、`plugin/registry.rs:407` |
| `PLUGIN_KIND_MISMATCH` | 插件 `'{id}'` 类型不匹配 | `plugin_id`、`expected_kind`、`actual_kind` | `plugin/types.rs:219`、`plugin/loaded.rs:71`、`plugin/pipeline.rs:121` |
| `PLUGIN_VERSION_MISMATCH` | manifest 使用旧 abi-version / agreement-version 不匹配 | `plugin_id`、`expected`、`actual` | `plugin/types.rs:64,86` |
| `PLUGIN_MANIFEST_INVALID` | manifest JSON / 字段 / URL / 模型 等校验失败 | `plugin_id`、`field`、`url`、`model_id`、`source` | `plugin/types.rs:54,71,131,393,494,506,517,586,602`（多由 `manifest_invalid()` 助手构造）、`plugin/scanner.rs:25,32` |
| `PLUGIN_RUNTIME_ERROR` | 插件 map_request / map_response / map_stream_line 执行失败 | `source` | `plugin/loaded.rs:135,153,172` |
| `PLUGIN_TOOL_NOT_AVAILABLE` | _（声明保留，留给后续 tool_id 不存在场景）_ | — | — |

### 3.3 LLM 会话 `LLM_*`

| Code | 默认 message | 典型 detail 键 | 主构造点 |
| --- | --- | --- | --- |
| `LLM_SESSION_CREATE_FAILED` | 创建 session runtime 失败 | `source` | `llm/session.rs:341,389,456,515` |
| `LLM_SESSION_NOT_FOUND` | _（库内未直接使用，app 边界用于 SessionEntry 缺失）_ | `session_id` | — |
| `LLM_SESSION_CLOSED` | _（库内未直接使用，app 边界用于 input_tx 已关闭）_ | `session_id` | — |
| `LLM_SESSION_BUSY` | _（声明保留，未直接使用）_ | — | — |
| `LLM_MESSAGE_INVALID` | _（声明保留，未直接使用）_ | — | — |
| `LLM_REQUEST_BAD_PAYLOAD` | 请求 JSON 序列化失败 / 映射后反序列化失败 / TTS 请求序列化失败 | `source` | `plugin/pipeline.rs:188,193`、`tts/session.rs:41` |
| `LLM_REQUEST_TIMEOUT` | _（声明保留；超时实际经由 `HTTP_TIMEOUT` 报告）_ | — | — |
| `LLM_REQUEST_RATE_LIMITED` | _（声明保留，留给厂商显式限频场景）_ | — | — |
| `LLM_REQUEST_NETWORK_ERROR` | HTTP 请求发送失败（DNS/连接重置）/ 下载音频 URL 失败 | `url`、`source` | `http_poster.rs:24-33`（动态判定）、`audio/decoder.rs:109,125` |
| `LLM_RESPONSE_BAD_STATUS` | HTTP 状态码非 2xx 且未命中 4xx/5xx 桶 | `url`、`status_code`、`body` | `http_poster.rs:20`、`http_poster.rs:88-95` |
| `LLM_RESPONSE_PARSE_ERROR` | LLM/图像/TTS 响应 JSON 解析失败 | `source` | `llm/session.rs:1140,1144`、`image/session.rs:51`、`tts/session.rs:54` |
| `LLM_RESPONSE_EMPTY` | LLM 响应为空 / AI 未返回可用内容 | — | `llm/session.rs:1110` |
| `LLM_STREAM_PROTOCOL_ERROR` | 流式响应 JSON 解析失败 / 分行失败 | `line`、`source` | `llm/stream_decoder.rs:65`、`http_poster.rs:108-112` |
| `LLM_TOOL_CALL_FAILED` | 工具调用超过最大连续轮数限制 | `max_tool_rounds`、`tool_rounds` | `llm/session.rs:907` |
| `LLM_TOOL_CALL_TIMEOUT` | 工具执行超时: `{name}` | `tool_id`、`timeout_ms` | `tool/registry.rs:284` |
| `LLM_TOOL_CALL_INVALID` | 缺少或非法参数: `{key}` | `field` | `tool/registry.rs:19,30` |

### 3.4 图像 `IMAGE_*`

| Code | 默认 message | 典型 detail 键 | 主构造点 |
| --- | --- | --- | --- |
| `IMAGE_TASK_FAILED` | 图像生成失败 (`{code}`): `{msg}` / 解码 b64_json 图片失败 | `provider_code`、`message`、`source` | `image/session.rs:60,150` |
| `IMAGE_TASK_INVALID_PARAMS` | 图像请求序列化失败 | `source` | `image/session.rs:37` |
| `IMAGE_TASK_EMPTY_RESPONSE` | 图像响应为空 / 缺少 data / 未返回任何图片 | — | `image/session.rs:116,129,134` |
| `IMAGE_RESPONSE_BLOCKED` | _（声明保留，留给平台内容安全拦截场景）_ | — | — |

### 3.5 TTS `TTS_*`

| Code | 默认 message | 典型 detail 键 | 主构造点 |
| --- | --- | --- | --- |
| `TTS_TASK_FAILED` | TTS 错误 (`{status_code}`): `{msg}` | `status_code`、`message` | `tts/session.rs:72` |
| `TTS_RESPONSE_EMPTY` | TTS 响应为空 / 缺少 audio data / 音频数据为空且未提供 URL | — | `tts/session.rs:108,117,139` |
| `TTS_VOICE_NOT_FOUND` | _（声明保留）_ | — | — |
| `TTS_TEXT_INVALID` | _（声明保留）_ | — | — |
| `TTS_AUDIO_TOO_LONG` | _（声明保留）_ | — | — |
| `TTS_FILE_WRITE_FAILED` | _（声明保留）_ | — | — |

### 3.6 音频解码 / 播放 `AUDIO_*`

| Code | 默认 message | 典型 detail 键 | 主构造点 |
| --- | --- | --- | --- |
| `AUDIO_DECODE_FAILED` | hex/base64 音频解码失败、探测格式失败、未找到轨道、采样率未知、解码器创建/帧解码失败、空数据等 | `source` | `audio/decoder.rs:18`（`decode_err` 助手；调用于 91-92、121、137、143、148、158、170、180、192）；`tts/session.rs:126` |
| `AUDIO_PLAYBACK_FAILED` | 构建/启动音频输出流失败、播放任务 panic | `source` | `audio/decoder.rs:22`（`playback_err` 助手；调用于 263、265、284） |
| `AUDIO_DEVICE_UNAVAILABLE` | 未找到默认音频输出设备 | — | `audio/decoder.rs:242` |

### 3.7 HTTP 传输 `HTTP_*`

> 全部由 `http_poster.rs::map_status` 与 `classify_reqwest_error` 在 `post_json` 内动态选择，故无固定行号。详情见 `http_poster.rs:12-34, 78-95`。

| Code | 触发条件 | 典型 detail 键 |
| --- | --- | --- |
| `HTTP_BAD_REQUEST` | 上游返回 400 | `url`、`status_code`、`body` |
| `HTTP_UNAUTHORIZED` | 上游返回 401 / 403 | `url`、`status_code`、`body` |
| `HTTP_NOT_FOUND` | 上游返回 404 | `url`、`status_code`、`body` |
| `HTTP_TOO_MANY_REQUESTS` | 上游返回 429 | `url`、`status_code`、`body` |
| `HTTP_SERVER_ERROR` | 上游返回 5xx；音频下载 URL 非成功状态 | `url`、`status_code`（音频路径还会带 `body` 之外的上下文） |
| `HTTP_TIMEOUT` | reqwest `is_timeout()` 命中或上游返回 408 | `url`、`source` |
| `HTTP_EMPTY_RESPONSE` | _（声明保留，留给上游 204 等场景）_ | — |

`audio/decoder.rs:116` 也会构造 `HTTP_SERVER_ERROR`（音频 URL 非 2xx）。

### 3.8 文件系统 `FS_*`

| Code | 默认 message | 典型 detail 键 | 主构造点 |
| --- | --- | --- | --- |
| `FS_OPEN_FAILED` | 无法打开插件包 / 读取插件目录失败 | `path`、`plugin_id`、`source` | `plugin/scanner.rs:15,69`、`plugin/manager.rs:78`、`plugin/loaded.rs:82`、`plugin/registry.rs:115` |
| `FS_WRITE_FAILED` | 创建插件目录 / 复制插件文件失败 | `path`、`plugin_id`、`source` | `plugin/scanner.rs:62`、`plugin/manager.rs:182` |
| `FS_PERMISSION_DENIED` | _（声明保留，留给显式权限拒绝场景）_ | — | — |

### 3.9 鉴权 `AUTH_*`

| Code | 默认 message | 典型 detail 键 | 主构造点 |
| --- | --- | --- | --- |
| `AUTH_API_KEY_MISSING` | api_key 不能为空 | `field` | `llm/config.rs:61` |
| `AUTH_KEY_INVALID` | _（声明保留，留给厂商显式 401 + 鉴权错误时升级到此码）_ | — | — |

### 3.10 参数校验 `VALIDATION_*`

| Code | 默认 message | 典型 detail 键 | 主构造点 |
| --- | --- | --- | --- |
| `VALIDATION_MISSING_FIELD` | base_url 不能为空 | `field` | `llm/config.rs:45` |
| `VALIDATION_FORMAT_ERROR` | base_url 必须以 http 开头；插件未声明模型；不支持的 thinking_effort；checkout 失败；无效的插件文件名 | `field`、`value`、`model_id`、`node_id`、`path` | `llm/config.rs:53`、`plugin/types.rs:418,428`、`llm/session.rs:678`、`plugin/manager.rs:172` |

### 3.11 工具中心 `TOOL_*`

| Code | 默认 message | 典型 detail 键 | 主构造点 |
| --- | --- | --- | --- |
| `TOOL_NOT_FOUND` | 未知工具: `{name}` | `tool_id` | `tool/registry.rs:273` |
| `TOOL_DISABLED` | 工具已禁用: `{name}` | `tool_id` | `tool/registry.rs:263` |

### 3.12 兜底

| Code | 默认 message | 主构造点 |
| --- | --- | --- |
| `NOT_IMPLEMENTED` | _（声明保留，未直接使用）_ | — |

## 4. 助手函数

库内除直接 `ClientError::new` 外，还提供若干局部助手，便于审计：

- `error.rs::ClientError::internal(msg)` — 兜底归类为 `CORE_CLIENT_INTERNAL_ERROR`。
- `error.rs::ClientError::from_anyhow(&err)` / `from_anyhow_owned(err)` — 从 `anyhow::Error` 还原。
- `plugin/types.rs::manifest_invalid(msg)` — `PLUGIN_MANIFEST_INVALID` 快捷构造。
- `audio/decoder.rs::decode_err(msg, source)` / `playback_err(msg, source)` — `AUDIO_DECODE_FAILED` / `AUDIO_PLAYBACK_FAILED` 快捷构造。
- `http_poster.rs::map_status(status)` / `classify_reqwest_error(err)` — HTTP 状态/网络错误分流。

## 5. 已声明但暂未使用的码

为下游扩展预留，未发生破坏性变更前可直接启用。新启用时请在本表对应行补 message 与构造点：

- `CORE_CLIENT_TIMEOUT`、`CORE_CLIENT_CANCELLED`
- `PLUGIN_TOOL_NOT_AVAILABLE`
- `LLM_SESSION_NOT_FOUND`、`LLM_SESSION_CLOSED`、`LLM_SESSION_BUSY`、`LLM_MESSAGE_INVALID`
- `LLM_REQUEST_TIMEOUT`、`LLM_REQUEST_RATE_LIMITED`
- `IMAGE_RESPONSE_BLOCKED`
- `TTS_VOICE_NOT_FOUND`、`TTS_TEXT_INVALID`、`TTS_AUDIO_TOO_LONG`、`TTS_FILE_WRITE_FAILED`
- `HTTP_EMPTY_RESPONSE`
- `FS_PERMISSION_DENIED`
- `AUTH_KEY_INVALID`
- `NOT_IMPLEMENTED`

## 6. 维护流程

1. 修改 / 新增码 → 同步 `core_ai_client/src/error.rs::define_error_codes!`、`app_main/src/api/error.ts::ErrorCode` 常量表与本文件。
2. 新增构造点 → 在 3.x 对应行补行号；如启用第 5 节预留码，需将其搬入 3.x 并补 message。
3. 删除 / 重命名码 → 走破坏性变更流程：评估 app_main、site_flowcloudai 与所有插件，更新前端 i18n 映射，并标注 commit。
