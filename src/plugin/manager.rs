use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use wasmtime::component::Linker;
use wasmtime::{Config, Engine};
use crate::{LoadedPlugin, PluginScanner, SUPPORTED_ABI_VERSION};
use crate::plugin::host::HostState;
use crate::plugin::types::{PluginKind, PluginMeta};


// ═════════════════════════════════════════════════════════════
//                    插件管理器
// ═════════════════════════════════════════════════════════════

/// 插件管理系统
///
/// 负责：
/// - 扫描和加载本地插件
/// - 管理插件元数据和版本检查
/// - 提供统一的插件操作接口（请求/响应转换）
/// - 支持 LLM、Image、TTS 三种插件类型
///
/// # 示例
/// ```ignore
/// let mut manager = PluginManager::new(PathBuf::from("./plugins"))?;
/// manager.load_llm_plugin("my_llm_adapter")?;
/// let result = manager.map_request(PluginKind::LLM, json_str)?;
/// ```
pub struct PluginManager {
    /// 插件目录路径
    plug_path: PathBuf,

    /// 已加载的插件元数据 (id -> meta)
    pub plugins: HashMap<String, PluginMeta>,

    /// WebAssembly 执行引擎
    pub(crate) engine: Engine,

    /// 导出函数链接器
    pub(crate) linker: Linker<HostState>,

    // ─────── 运行态插件实例 ────────
    llm_plugin: LoadedPlugin,
    image_plugin: LoadedPlugin,
    tts_plugin: LoadedPlugin,
}

// ─────────────────────────────────────────────────────────────
//                    初始化与配置
// ─────────────────────────────────────────────────────────────

impl PluginManager {
    /// 创建新的插件管理器
    ///
    /// # 参数
    /// - `plug_path`: 插件目录路径（自动扫描该目录下的所有插件）
    ///
    /// # 错误
    /// 返回 WebAssembly 配置或插件扫描失败的错误
    pub fn new(plug_path: PathBuf) -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config)
            .map_err(|e| anyhow!("Failed to create WebAssembly engine: {}", e))?;
        let mut linker = Linker::new(&engine);

        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| anyhow!("Failed to add WASI to linker: {}", e))?;

        let plugins = Self::load_plugins(Path::new(&plug_path))?;

        Ok(PluginManager {
            plug_path,
            plugins,
            engine,
            linker,
            llm_plugin: LoadedPlugin::new(PluginKind::LLM),
            image_plugin: LoadedPlugin::new(PluginKind::Image),
            tts_plugin: LoadedPlugin::new(PluginKind::TTS),
        })
    }

    /// 加载插件目录中的所有插件
    ///
    /// 扫描指定目录，过滤有效的插件：
    /// - 验证插件 ID 唯一性
    /// - 检查 ABI 版本兼容性
    /// - 验证插件 URL 非空
    fn load_plugins(path: &Path) -> Result<HashMap<String, PluginMeta>> {
        let mut plugins: HashMap<String, PluginMeta> = HashMap::new();

        for fcplug in PluginScanner::scan_plugins(path)
            .context("Failed to scan plugins directory")? {
            match PluginScanner::read_plugin_info(&fcplug) {
                Ok(info) => {
                    if !Self::validate_plugin(&info, &plugins) {
                        continue;
                    }

                    plugins.insert(
                        info.id.clone(),
                        PluginScanner::build_plugin_meta(info, &fcplug),
                    );
                }
                Err(e) => {
                    eprintln!("⚠️  Invalid plugin {:?}: {}", fcplug, e);
                }
            }
        }

        Ok(plugins)
    }

    /// 验证插件的有效性
    ///
    /// # 检查项
    /// - 插件 ID 未重复
    /// - ABI 版本匹配
    /// - URL 非空
    fn validate_plugin(
        info: &crate::plugin::types::PluginInfo,
        existing: &HashMap<String, PluginMeta>,
    ) -> bool {
        if existing.contains_key(&info.id) {
            eprintln!("⚠️  Duplicate plugin ID: {}", info.id);
            return false;
        }

        if info.abi_version != SUPPORTED_ABI_VERSION {
            eprintln!(
                "⚠️  Skip plugin '{}': ABI version mismatch (expected: {}, got: {})",
                info.id, SUPPORTED_ABI_VERSION, info.abi_version
            );
            return false;
        }

        if info.url.is_empty() {
            eprintln!("⚠️  Skip plugin '{}': empty URL", info.id);
            return false;
        }

        true
    }
}

// ─────────────────────────────────────────────────────────────
//                    插件管理操作
// ─────────────────────────────────────────────────────────────

impl PluginManager {
    /// 检查指定类型的插件是否已加载
    pub fn is_loaded(&self, kind: &PluginKind) -> bool {
        self.get_plugin(kind).is_loaded()
    }

    /// 获取指定插件 ID 的下载 URL
    pub fn get_url(&self, id: &str) -> Result<&str> {
        self.plugins
            .get(id)
            .ok_or_else(|| anyhow!("Plugin not found: {}", id))
            .map(|meta| meta.url.as_str())
    }

    /// 添加新插件到管理器
    ///
    /// 执行以下操作：
    /// 1. 读取插件元数据并验证
    /// 2. 检查 ABI 版本和 ID 唯一性
    /// 3. 复制插件文件到插件目录
    /// 4. 更新插件元数据映射
    ///
    /// # 参数
    /// - `plugin_path`: 要添加的插件文件路径
    ///
    /// # 错误
    /// 返回版本不匹配、ID 重复、文件复制等错误
    pub fn add_plugin(&mut self, plugin_path: &str) -> Result<()> {
        let info = PluginScanner::read_plugin_info(plugin_path.as_ref())
            .context("Failed to read plugin metadata")?;

        // 检查 ID 唯一性
        if self.plugins.contains_key(&info.id) {
            return Err(anyhow!("Plugin already exists: {}", info.id));
        }

        // 检查 ABI 版本
        if info.abi_version != SUPPORTED_ABI_VERSION {
            return Err(anyhow!(
                "Plugin '{}' ABI version mismatch: expected {}, got {}",
                info.id,
                SUPPORTED_ABI_VERSION,
                info.abi_version
            ));
        }

        // 获取目标路径并复制文件
        let filename = Path::new(plugin_path)
            .file_name()
            .ok_or_else(|| anyhow!("Invalid plugin filename: {}", plugin_path))?;

        let dst = Path::new(&self.plug_path).join(filename);

        fs::copy(plugin_path, &dst)
            .context(format!("Failed to copy plugin '{}' to {:?}", info.id, dst))?;

        // 更新插件元数据
        self.plugins.insert(
            info.id.clone(),
            PluginScanner::build_plugin_meta(info, &dst),
        );

        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────
//                    插件加载
// ─────────────────────────────────────────────────────────────

impl PluginManager {
    /// 加载 LLM 插件
    ///
    /// # 参数
    /// - `id`: 插件 ID（必须已在 `plugins` 中注册）
    pub fn load_llm_plugin(&mut self, id: &str) -> Result<()> {
        self.llm_plugin.load(&self.plugins, &self.linker, &self.engine, id)
            .context(format!("Failed to load LLM plugin '{}'", id))
    }

    /// 加载 Image 插件
    ///
    /// # 参数
    /// - `id`: 插件 ID（必须已在 `plugins` 中注册）
    pub fn load_image_plugin(&mut self, id: &str) -> Result<()> {
        self.image_plugin
            .load(&self.plugins, &self.linker, &self.engine, id)
            .context(format!("Failed to load Image plugin '{}'", id))
    }

    /// 加载 TTS 插件
    ///
    /// # 参数
    /// - `id`: 插件 ID（必须已在 `plugins` 中注册）
    pub fn load_tts_plugin(&mut self, id: &str) -> Result<()> {
        self.tts_plugin
            .load(&self.plugins, &self.linker, &self.engine, id)
            .context(format!("Failed to load TTS plugin '{}'", id))
    }
}

// ─────────────────────────────────────────────────────────────
//                    插件操作接口
// ─────────────────────────────────────────────────────────────

impl PluginManager {
    /// 转换请求 JSON
    ///
    /// 通过已加载的插件转换请求数据（如果插件已加载）。
    /// 若插件未加载，直接返回原始 JSON。
    ///
    /// # 参数
    /// - `kind`: 插件类型（LLM、Image、TTS）
    /// - `json`: 原始请求 JSON 字符串
    ///
    /// # 返回
    /// 转换后的 JSON 字符串
    pub fn map_request(&mut self, kind: PluginKind, json: &str) -> Result<String> {
        self.get_plugin_mut(&kind).map_request(json)
    }

    /// 转换响应 JSON
    ///
    /// 通过已加载的插件转换响应数据（如果插件已加载）。
    /// 若插件未加载，直接返回原始 JSON。
    ///
    /// # 参数
    /// - `kind`: 插件类型（LLM、Image、TTS）
    /// - `json`: 原始响应 JSON 字符串
    ///
    /// # 返回
    /// 转换后的 JSON 字符串
    pub fn map_response(&mut self, kind: PluginKind, json: &str) -> Result<String> {
        self.get_plugin_mut(&kind).map_response(json)
    }

    /// 转换流式响应行
    ///
    /// 通过已加载的插件转换流式响应的单行数据（如果插件已加载）。
    /// 若插件未加载，直接返回原始行。
    ///
    /// # 参数
    /// - `kind`: 插件类型（LLM、Image、TTS）
    /// - `line`: 原始流式响应行
    ///
    /// # 返回
    /// 转换后的响应行
    pub fn map_stream_line(&mut self, kind: PluginKind, line: &str) -> Result<String> {
        self.get_plugin_mut(&kind).map_stream_line(line)
    }
}

// ─────────────────────────────────────────────────────────────
//                    内部辅助方法
// ─────────────────────────────────────────────────────────────

impl PluginManager {
    /// 根据插件类型获取不可变引用
    #[inline]
    fn get_plugin(&self, kind: &PluginKind) -> &LoadedPlugin {
        match kind {
            PluginKind::LLM => &self.llm_plugin,
            PluginKind::Image => &self.image_plugin,
            PluginKind::TTS => &self.tts_plugin,
        }
    }

    /// 根据插件类型获取可变引用
    #[inline]
    fn get_plugin_mut(&mut self, kind: &PluginKind) -> &mut LoadedPlugin {
        match kind {
            PluginKind::LLM => &mut self.llm_plugin,
            PluginKind::Image => &mut self.image_plugin,
            PluginKind::TTS => &mut self.tts_plugin,
        }
    }
}