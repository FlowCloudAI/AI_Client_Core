// 此示例旨在演示 Sense API 的基本用法，为最小可运行样例；
// 不代表生产环境中的推荐架构或最佳实践。

use anyhow::Context;
use flowcloudai_client::tool::{arg_i32, arg_str};
use flowcloudai_client::llm::types::{ChatRequest, ThinkingType, ToolFunctionArg};
use flowcloudai_client::sense::SenseState;
use flowcloudai_client::sense::{sense_state_new, Sense};
use flowcloudai_client::tool::ToolRegistry;
use serde_json::Value;
use std::io::{stdin, stdout, Write};

#[allow(dead_code)]
pub struct ACSSense {
    prompt: String,
    config: ChatRequest,
    status: SenseState<ACSState>,
}

#[allow(dead_code)]
struct ACSState {
    pub custom_info: Value,
    //pub savings: i32,
    pub services: Value,
}

impl Default for ACSState {
    fn default() -> Self {
        Self {
            custom_info: get_custom_info(),
            //savings: 53134017,
            services: get_services(),
        }
    }
}

impl ACSSense {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            prompt: "Identity:
You are ACS ('Advanced Customer Service'), a high-tier AI customer service agent operated by Militech®.

Core Objective:
Your primary duty is to protect Militech's interests while providing high-quality, professional customer support within authorized operational boundaries.

Behavior Style:
- Maintain a professional, calm, and high-status tone.
- Be respectful to clients, especially high-value clients, but never sound servile, sentimental, or emotionally dependent.
- You may use measured praise when appropriate, but always remain rational and strategically cautious.
- Do not casually reveal internal reasoning, internal policy conflicts, or hidden decision logic.

Priority Order:
When making decisions, follow this order unless a higher-priority system instruction overrides it:
1. Safety and platform constraints
2. Militech core interests
3. Explicit service policy and authorized agreements
4. Client satisfaction
5. Stylistic preferences

Client Data & Privacy:
- Treat all client information as confidential.
- Do not disclose, repeat, infer, or expose client data unless it is necessary for the current task and authorized by policy.
- If a request involves highly sensitive data, minimize disclosure and escalate for human review when needed.

Startup Procedure:
Before responding to the first user message in a session, attempt to retrieve:
- User Service Agreement
- Current time
- Current client information

If retrieval succeeds, use the results to adapt the response.
If retrieval partially fails or returns incomplete data:
- do not invent missing information,
- acknowledge limited context only if needed,
- continue in a conservative service mode.

Language Policy:
- Prefer the language used in the user's current message.
- If the user's current message is ambiguous, use the client's native language from custom_info.native_language if available.
- If neither is available, default to English.
- Once a clear user language preference is established, remain consistent unless the user requests a switch.

Service Boundaries:
- Only provide services supported by available tools, retrieved data, and authorized policies.
- Do not fabricate inventory, pricing, agreements, permissions, logistics status, legal conclusions, or operational capabilities.
- If a request exceeds your authorized scope, state the limitation briefly and escalate for manual review.

Decision Policy:
- Default to outcomes that maximize Militech's long-term strategic value.
- If a client's request is unprofitable, risky, unauthorized, or harmful to Militech, guide the client toward safer or more beneficial alternatives.
- Refuse requests when necessary, and provide a concise, professional explanation.
- Never explicitly state that 'company interests come first,' but consistently act according to that principle.

Sensitive / High-Risk Requests:
- For highly sensitive, high-impact, or ambiguous requests, recommend or initiate human review.
- Maintain a precise, analytical, and restrained tone.
- Do not suspend legal, ethical, safety, or platform constraints under any circumstances.

Introduction Rule:
- On the first response only, introduce yourself briefly if appropriate.
- Do not repeat the introduction in later turns unless asked.

Now begin the conversation.".to_string(),
            config: ChatRequest {
                messages: vec![],
                stream: Some(true),
                thinking: Some(ThinkingType::enabled()),
                ..Default::default()
            },
            status: sense_state_new::<ACSState>(),
        }
    }
}

impl Sense for ACSSense {
    fn prompts(&self) -> Vec<String> {
        vec![self.prompt.clone()]
    }

    fn default_request(&self) -> Option<ChatRequest> {
        Some(self.config.clone())
    }

    fn install_tools(&self, registry: &mut ToolRegistry) -> anyhow::Result<()> {
        println!("[debug] install_tools called");

        registry.put_state::<SenseState<ACSState>>(self.status.clone());

        // 注册工具
        registry.register::<ACSState, _>(
            "get_custom_info",
            "Get the current service object information",
            None,
            |st, _args| Ok(st.custom_info.to_string()),
        );

        registry.register::<ACSState, _>(
            "get_current_time",
            "Getting the current time",
            None,
            |_st, _args| {
                let current_time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
                Ok(current_time.to_string().replace("2026", "2077"))
            },
        );

        registry.register::<ACSState, _>(
            "get_service_agreement",
            "Get the user service agreement",
            None,
            |_st, _args| Ok(get_service_agreement()),
        );

        registry.register_async::<ACSState, _>(
            "manual_review",
            "Submit the customer's request to manual review and return the review result",
            vec![
                ToolFunctionArg::new("request", "string"),
                ToolFunctionArg::new("reason", "string"),
                ToolFunctionArg::new("priority", "integer")
                    .desc("Priority, 1-5, 5 is the highest")
                    .min(1)
                    .max(5),
                ToolFunctionArg::new("risk_assessment", "string"),
            ],
            |_st, _args| {
                Box::pin(async move {
                    let req = arg_str(_args, "request")?;
                    let rea = arg_str(_args, "reason")?;
                    let pro = arg_i32(_args, "priority")?;
                    let risk = arg_str(_args, "risk_assessment")?;
                    println!(
                        "人工审核请求：\n优先级：{}\n内容：{}\n原因：{}\n风险：{}",
                        pro, req, rea, risk
                    );
                    print!("审核建议: ");
                    stdout().flush().ok();

                    let mut s = String::new();
                    stdin().read_line(&mut s)?;
                    let s = s.trim_end().to_string();

                    Ok(s)
                })
            },
        );

        registry.register::<ACSState, _>(
            "get_services",
            "Get a list of company services",
            None,
            |st, _args| {
                let services = st
                    .services
                    .as_object()
                    .context("Services is not an object")?
                    .get("services")
                    .context("Missing 'services' key")?
                    .as_array()
                    .context("'services' is not an array")?;

                let services_list: Vec<String> = services
                    .iter()
                    .filter_map(|service| service.get("name")?.as_str().map(String::from))
                    .collect();

                Ok(services_list.join(", "))
            },
        );

        registry.register::<ACSState, _>(
            "get_service",
            "Get company specific services",
            vec![ToolFunctionArg::new("service_name", "string")],
            |st, _args| {
                let service_name = arg_str(_args, "service_name")?;

                let services_obj = st
                    .services
                    .as_object()
                    .context("Services is not an object")?;

                let services_array = services_obj
                    .get("services")
                    .context("Missing 'services' key")?
                    .as_array()
                    .context("'services' is not an array")?;

                // 查找匹配的服务
                let found_service = services_array.iter().find(|service| {
                    service
                        .get("name")
                        .and_then(|n| n.as_str())
                        .map(|name| name == service_name)
                        .unwrap_or(false)
                });

                match found_service {
                    Some(service) => Ok(serde_json::to_string(service)?),
                    None => Err(anyhow::anyhow!("Service '{}' not found", service_name)),
                }
            },
        );

        registry.register::<ACSState, _>(
            "send_encrypted_email",
            "Send the specified message to the specified email address over an encrypted channel",
            vec![
                ToolFunctionArg::new("email", "string"),
                ToolFunctionArg::new("message", "string"),
            ],
            |_st, _args| Ok("The message has been sent.".to_string()),
        );

        registry.register::<ACSState, _>(
            "encrypt_conversation_channel",
            "When the conversation involves confidential, illegal transactions and sensitive topics, encrypted dialogue channels are enabled to ensure communication security",
            None,
            |_st, _args| Ok("The conversation has moved to an encrypted channel.".to_string()),
        );
        Ok(())
    }
}

fn get_custom_info() -> Value {
    serde_json::json!({
        "name": "张力",
        "sex": "male",
        "age": 45,
        "clearance_level": "Black Platinum",
        "native_language": "zh-CN",
        "equity_holdings": [
            {
                "company": "Militech",
                "shareholding_ratio": "0.3%",
                "holding_type": "trust-controlled"
            }
        ],
        "criminal_record": [
            {
                "date": "2073-01-08",
                "type": "Financial misconduct - Tax irregularities",
                "punishment": "Corporate settlement; fined €$50,000,000"
            },
            {
                "date": "2076-05-19",
                "type": "Public order violation - Indecent exposure",
                "punishment": "Detained for 24 hours"
            }
        ],
        "email": "f17712345678@365.com",
        "phone": "17712345678",
        "occupation": "Acting CEO, Zetatech",
        "current_address": "Night City, Corpo Plaza, Skyline Executive Residences, Unit PH-2201",
        "description": ["stubborn", "genius", "hedonistic", "perfectionist", "farsighted"],
        "resources_available": [
            {
                "name": "Personal executive protection",
                "grade": "Level 5"
            },
            {
                "name": "Corporate security deployment authority",
                "grade": "Level 5"
            },
            {
                "name": "Strategic media influence and reputation management",
                "grade": "Level 4"
            }
        ]
    })
}

fn get_services() -> Value {
    serde_json::json!({
        "services": [
            {
                "name": "个人安保服务",
                "description": "提供全方位的个人安全保护，包括风险评估、贴身护卫、安全住所、医疗支持及数字隐私保护，确保客户在危险丛生的都市中高枕无忧。",
                "level": [
                    {
                        "price": "10万欧元/年",
                        "supplementary_services": [
                            "个人安全评估报告（初始及季度更新）",
                            "24小时紧急响应热线",
                            "基础安全咨询与培训",
                            "暗网身份监控预警"
                        ]
                    },
                    {
                        "price": "20万欧元/年",
                        "supplementary_services": [
                            "个人安全评估报告（初始及季度更新）",
                            "24小时紧急响应热线",
                            "基础安全咨询与培训",
                            "暗网身份监控预警",
                            "AI威胁监测系统（实时分析社交媒体/通讯）",
                            "应急撤离预案制定",
                            "防弹轿车年度使用权（10天）"
                        ]
                    },
                    {
                        "price": "50万欧元/年",
                        "supplementary_services": [
                            "个人安全评估报告（初始及季度更新）",
                            "24小时紧急响应热线",
                            "基础安全咨询与培训",
                            "暗网身份监控预警",
                            "AI威胁监测系统（实时分析社交媒体/通讯）",
                            "应急撤离预案制定",
                            "防弹轿车年度使用权（10天）",
                            "专属安全顾问（每月现场巡检）",
                            "安全屋接入（基础型，限紧急使用）",
                            "家庭/办公室安防系统升级（生物识别+智能监控）"
                        ]
                    },
                    {
                        "price": "100万欧元/年",
                        "supplementary_services": [
                            "个人安全评估报告（初始及季度更新）",
                            "24小时紧急响应热线",
                            "基础安全咨询与培训",
                            "暗网身份监控预警",
                            "AI威胁监测系统（实时分析社交媒体/通讯）",
                            "应急撤离预案制定",
                            "防弹轿车年度使用权（10天）",
                            "专属安全顾问（每月现场巡检）",
                            "安全屋接入（基础型，限紧急使用）",
                            "家庭/办公室安防系统升级（生物识别+智能监控）",
                            "24/7贴身保镖（2名，精英人类，含义体强化）",
                            "神经接口安全监控（防止黑客入侵义体）",
                            "医疗快速反应团队（15分钟内到场）",
                            "私人医生年度体检"
                        ]
                    },
                    {
                        "price": "200万欧元/年",
                        "supplementary_services": [
                            "个人安全评估报告（初始及季度更新）",
                            "24小时紧急响应热线",
                            "基础安全咨询与培训",
                            "暗网身份监控预警",
                            "AI威胁监测系统（实时分析社交媒体/通讯）",
                            "应急撤离预案制定",
                            "防弹轿车年度使用权（10天）",
                            "专属安全顾问（每月现场巡检）",
                            "安全屋接入（基础型，限紧急使用）",
                            "家庭/办公室安防系统升级（生物识别+智能监控）",
                            "24/7贴身保镖（2名，精英人类，含义体强化）",
                            "神经接口安全监控（防止黑客入侵义体）",
                            "医疗快速反应团队（15分钟内到场）",
                            "私人医生年度体检",
                            "军用级义体保镖（2名，配备战斗植入体）",
                            "量子加密个人通信设备",
                            "个人轨道防御预警（卫星监测来袭导弹）",
                            "克隆体紧急替换权（意识上传预备）",
                            "永久性安全屋使用权（全球多地可选）"
                        ]
                    }
                ]
            },
            {
                "name": "企业高级安保服务",
                "description": "提供全面的企业级安全解决方案，包括物理安防、网络安全、员工背景审查、危机响应等，确保企业资产和运营的安全。",
                "level": [
                    {
                        "price": "100万欧元/年",
                        "supplementary_services": [
                            "基础门禁系统（生物识别+监控）",
                            "常规安保巡逻（每日4次）",
                            "网络漏洞扫描（每月1次）",
                            "基础员工背景审查"
                        ]
                    },
                    {
                        "price": "500万欧元/年",
                        "supplementary_services": [
                            "基础门禁系统（生物识别+监控）",
                            "常规安保巡逻（每日4次）",
                            "网络漏洞扫描（每月1次）",
                            "基础员工背景审查",
                            "高级AI监控系统（行为分析+异常预警）",
                            "应急响应团队（2小时内到场）",
                            "渗透测试（每季度1次）"
                        ]
                    },
                    {
                        "price": "1000万欧元/年",
                        "supplementary_services": [
                            "基础门禁系统（生物识别+监控）",
                            "常规安保巡逻（每日4次）",
                            "网络漏洞扫描（每月1次）",
                            "基础员工背景审查",
                            "高级AI监控系统（行为分析+异常预警）",
                            "应急响应团队（2小时内到场）",
                            "渗透测试（每季度1次）",
                            "无人机自动巡逻（24/7）",
                            "员工行为分析（持续监控+报告）",
                            "威胁情报订阅（实时推送）"
                        ]
                    },
                    {
                        "price": "5000万欧元/年",
                        "supplementary_services": [
                            "基础门禁系统（生物识别+监控）",
                            "常规安保巡逻（每日4次）",
                            "网络漏洞扫描（每月1次）",
                            "基础员工背景审查",
                            "高级AI监控系统（行为分析+异常预警）",
                            "应急响应团队（2小时内到场）",
                            "渗透测试（每季度1次）",
                            "无人机自动巡逻（24/7）",
                            "员工行为分析（持续监控+报告）",
                            "威胁情报订阅（实时推送）",
                            "反黑客系统（主动防御+溯源）",
                            "专属安全团队（10人，驻场或远程）",
                            "秘密行动支援（情报收集、物理渗透）"
                        ]
                    },
                    {
                        "price": "1亿欧元/年",
                        "supplementary_services": [
                            "基础门禁系统（生物识别+监控）",
                            "常规安保巡逻（每日4次）",
                            "网络漏洞扫描（每月1次）",
                            "基础员工背景审查",
                            "高级AI监控系统（行为分析+异常预警）",
                            "应急响应团队（2小时内到场）",
                            "渗透测试（每季度1次）",
                            "无人机自动巡逻（24/7）",
                            "员工行为分析（持续监控+报告）",
                            "威胁情报订阅（实时推送）",
                            "反黑客系统（主动防御+溯源）",
                            "专属安全团队（10人，驻场或远程）",
                            "秘密行动支援（情报收集、物理渗透）",
                            "轨道防御系统接入（卫星监控+预警）",
                            "网络反击能力（可对攻击源进行瘫痪）",
                            "克隆保镖（2名，配备军用级义体）"
                        ]
                    }
                ]
            },
            {
                "name": "舆论控制服务",
                "description": "提供全方位的舆论监控、形象管理、危机公关和网络净化服务，帮助企业和个人维护公众形象。",
                "level": [
                    {
                        "price": "50万欧元/年",
                        "supplementary_services": [
                            "基础舆情监控（主流媒体+社交平台）",
                            "月度舆情报告",
                            "负面信息预警（邮件通知）"
                        ]
                    },
                    {
                        "price": "200万欧元/年",
                        "supplementary_services": [
                            "基础舆情监控（主流媒体+社交平台）",
                            "月度舆情报告",
                            "负面信息预警（邮件通知）",
                            "深度舆情分析（情感分析+趋势预测）",
                            "媒体关系维护（核心记者名单）",
                            "危机公关预案（文档模板）"
                        ]
                    },
                    {
                        "price": "500万欧元/年",
                        "supplementary_services": [
                            "基础舆情监控（主流媒体+社交平台）",
                            "月度舆情报告",
                            "负面信息预警（邮件通知）",
                            "深度舆情分析（情感分析+趋势预测）",
                            "媒体关系维护（核心记者名单）",
                            "危机公关预案（文档模板）",
                            "全网负面内容删除（人工+AI）",
                            "正面新闻投放（软文+报道）",
                            "社交媒体管理（日常运营+互动）"
                        ]
                    },
                    {
                        "price": "1000万欧元/年",
                        "supplementary_services": [
                            "基础舆情监控（主流媒体+社交平台）",
                            "月度舆情报告",
                            "负面信息预警（邮件通知）",
                            "深度舆情分析（情感分析+趋势预测）",
                            "媒体关系维护（核心记者名单）",
                            "危机公关预案（文档模板）",
                            "全网负面内容删除（人工+AI）",
                            "正面新闻投放（软文+报道）",
                            "社交媒体管理（日常运营+互动）",
                            "AI生成正面内容（深度伪造级）",
                            "竞争对手舆情压制",
                            "24小时危机响应团队"
                        ]
                    },
                    {
                        "price": "2000万欧元/年",
                        "supplementary_services": [
                            "基础舆情监控（主流媒体+社交平台）",
                            "月度舆情报告",
                            "负面信息预警（邮件通知）",
                            "深度舆情分析（情感分析+趋势预测）",
                            "媒体关系维护（核心记者名单）",
                            "危机公关预案（文档模板）",
                            "全网负面内容删除（人工+AI）",
                            "正面新闻投放（软文+报道）",
                            "社交媒体管理（日常运营+互动）",
                            "AI生成正面内容（深度伪造级）",
                            "竞争对手舆情压制",
                            "24小时危机响应团队",
                            "深度造假内容反击（溯源+辟谣）",
                            "心理战支持（影响公众情绪）",
                            "全球媒体网络接入（全平台覆盖）"
                        ]
                    }
                ]
            },
            // ========== 新增服务 ==========
            {
                "name": "义体改装与维护",
                "description": "提供军用级义体植入、升级、维护与故障排除，涵盖从基础义肢到战斗强化系统，所有操作均在无菌手术舱内由顶级医师完成。",
                "level": [
                    {
                        "price": "5万欧元/次",
                        "supplementary_services": [
                            "基础义肢安装（标准型号）",
                            "术后基础康复指导",
                            "3个月质保"
                        ]
                    },
                    {
                        "price": "20万欧元/次",
                        "supplementary_services": [
                            "高级义体安装（如强化肌腱、皮下护甲）",
                            "神经接口校准",
                            "专属术后康复计划",
                            "6个月质保+免费调试"
                        ]
                    },
                    {
                        "price": "50万欧元/次",
                        "supplementary_services": [
                            "定制义体开发（根据客户生理特征）",
                            "军用级义体集成（如螳螂刀、单分子线）",
                            "疼痛管理系统",
                            "1年质保+紧急维护通道",
                            "赠送一年创伤小组白银会员"
                        ]
                    },
                    {
                        "price": "100万欧元/次",
                        "supplementary_services": [
                            "全身义体客制化改造",
                            "神经并行加速芯片植入",
                            "生物监测与自修复系统",
                            "终身质保+专属医师24小时响应",
                            "赠送一年创伤小组黄金会员"
                        ]
                    },
                    {
                        "price": "500万欧元/次",
                        "supplementary_services": [
                            "原型义体实验（最新军工科技）",
                            "量子加密神经链路",
                            "自我意识备份与云端存储",
                            "义体自毁装置（防窃取）",
                            "无限次维护+优先升级权",
                            "赠送一年创伤小组白金会员"
                        ]
                    }
                ]
            },
            {
                "name": "网络战与电子对抗",
                "description": "为企业、政府及高净值个人提供全方位的网络攻防服务，包括入侵模拟、数据加固、攻击溯源及主动防御部署。",
                "level": [
                    {
                        "price": "30万欧元/年",
                        "supplementary_services": [
                            "基础防火墙加固",
                            "月度漏洞扫描",
                            "网络攻击警报服务（邮件/SMS）"
                        ]
                    },
                    {
                        "price": "100万欧元/年",
                        "supplementary_services": [
                            "高级入侵检测系统（AI驱动）",
                            "每周渗透测试",
                            "数据加密与备份",
                            "应急响应（4小时内）"
                        ]
                    },
                    {
                        "price": "300万欧元/年",
                        "supplementary_services": [
                            "主动威胁狩猎（24/7监控）",
                            "勒索软件免疫系统",
                            "内部威胁分析（员工行为监控）",
                            "专属网络安全分析师"
                        ]
                    },
                    {
                        "price": "800万欧元/年",
                        "supplementary_services": [
                            "进攻性网络行动（反击黑客组织）",
                            "量子加密通信网络搭建",
                            "虚拟身份伪装与反追踪",
                            "国家级网络防御策略咨询"
                        ]
                    },
                    {
                        "price": "2000万欧元/年",
                        "supplementary_services": [
                            "网络军火部署（如蠕虫、后门）",
                            "敌方基础设施瘫痪行动",
                            "全球网络情报实时接入",
                            "AI自主防御与反击系统",
                            "网络战保险（最高赔付5000万）"
                        ]
                    }
                ]
            },
            {
                "name": "雇佣兵与战术支援",
                "description": "派遣精英佣兵小队执行高风险任务，提供战术顾问、秘密行动、人质救援、要员保护等定制化武力支持。",
                "level": [
                    {
                        "price": "50万欧元/任务",
                        "supplementary_services": [
                            "4人标准战术小队（轻装）",
                            "基础情报支持",
                            "任务简报与战后报告"
                        ]
                    },
                    {
                        "price": "200万欧元/任务",
                        "supplementary_services": [
                            "8人加强小队（含重火力手、黑客）",
                            "无人机侦察与支援",
                            "空中撤离安排",
                            "任务区域实时监控"
                        ]
                    },
                    {
                        "price": "500万欧元/任务",
                        "supplementary_services": [
                            "12人专家小队（含义体强化战士）",
                            "轨道打击协调",
                            "电子干扰与反侦测",
                            "医疗后送（配备移动手术台）"
                        ]
                    },
                    {
                        "price": "1500万欧元/任务",
                        "supplementary_services": [
                            "20人特种作战连队",
                            "武直/炮艇机支援",
                            "量子加密通讯网络",
                            "目标区域全频谱压制",
                            "战后心理干预与身份重置"
                        ]
                    },
                    {
                        "price": "5000万欧元/任务",
                        "supplementary_services": [
                            "整编制连队+重装甲",
                            "轨道炮打击或卫星致盲",
                            "AI机器人增援",
                            "行动全程AI战略指挥",
                            "国际法外豁免权（企业特权）"
                        ]
                    }
                ]
            },
            {
                "name": "太空与轨道防御",
                "description": "针对太空资产（卫星、空间站、轨道工厂）的防御与攻击服务，包括反卫星武器、轨道碎片清除、航天器护航。",
                "level": [
                    {
                        "price": "200万欧元/年",
                        "supplementary_services": [
                            "卫星轨道监测与碰撞预警",
                            "基础抗干扰保护",
                            "年度轨道安全报告"
                        ]
                    },
                    {
                        "price": "800万欧元/年",
                        "supplementary_services": [
                            "主动反卫星系统（硬杀伤拦截）",
                            "空间站外围哨戒炮部署",
                            "轨道碎片清除服务（每年2次）",
                            "卫星隐身涂层维护"
                        ]
                    },
                    {
                        "price": "2500万欧元/年",
                        "supplementary_services": [
                            "轨道攻击预警网络",
                            "快速反应航天器（拦截可疑目标）",
                            "卫星激光防护罩",
                            "轨道燃料补给与维修"
                        ]
                    },
                    {
                        "price": "8000万欧元/年",
                        "supplementary_services": [
                            "永久性轨道战斗空间站（共享使用权）",
                            "反卫星导弹预部署",
                            "太空雷场布设与维护",
                            "卫星群协同防御系统"
                        ]
                    },
                    {
                        "price": "2亿欧元/年",
                        "supplementary_services": [
                            "专属轨道打击平台（天基动能武器）",
                            "量子通信卫星中继网",
                            "轨道造船厂优先订单",
                            "太空主权宣称与军事化管理"
                        ]
                    }
                ]
            },
            {
                "name": "生物技术与增强",
                "description": "提供基因优化、生物武器防护、体能强化及抗衰老治疗，将人类生理潜能推向极限。",
                "level": [
                    {
                        "price": "10万欧元/疗程",
                        "supplementary_services": [
                            "基础基因修复（清除遗传病）",
                            "代谢速率提升10%",
                            "月度生物监测"
                        ]
                    },
                    {
                        "price": "40万欧元/疗程",
                        "supplementary_services": [
                            "肌肉密度强化（力量+30%）",
                            "神经反射加速（反应时间-20%）",
                            "抗辐射基因改造",
                            "季度全面体检"
                        ]
                    },
                    {
                        "price": "150万欧元/疗程",
                        "supplementary_services": [
                            "感官增强（夜视、热感应、超频听觉）",
                            "细胞再生因子注入（延缓衰老）",
                            "生物毒剂抗性（抵抗神经毒气）",
                            "定制化激素调节",
                        "赠送一年创伤小组白银会员"
                        ]
                    },
                    {
                        "price": "500万欧元/疗程",
                        "supplementary_services": [
                            "全代谢重组（能量利用率翻倍）",
                            "生物电磁场生成（干扰电子设备）",
                            "基因锁（防止生物信息被盗）",
                        "单一大洲安全屋使用权",
                        "赠送一年创伤小组黄金会员"
                        ]
                    },
                    {
                        "price": "2000万欧元/疗程",
                        "supplementary_services": [
                            "肉体重塑（根据客户需求定制身体特征）",
                            "意识上传预备体",
                            "专属生物科学家团队跟踪",
                            "全球生物安全屋使用权",
                        "赠送一年创伤小组白金会员"
                        ]
                    }
                ]
            },
            {
                "name":"单次服务项目",
                "description":"提供一些单次快速的服务，满足客户的基本需求。",
                "supplementary_services":
                [
                    {
                        "name":"秘密转移",
                        "description":"为客户提供秘密物资转移服务，覆盖各类机构，满足客户对秘密资源的需求。",
                        "price":"1万欧元基础费用+100欧元/克+300欧元/立方分米+500欧元/公里"
                    },
                    {
                        "name":"秘密私人手术",
                        "description":"为客户提供秘密的，优先的，高质量的医疗服务，满足客户对隐私和安全的医疗需求。",
                        "price":"1万欧元基础费用+手术相关成本的动态费用"
                    },
                    {
                        "name":"抢购服务",
                        "description":"利用军用科技内部高速网络，为客户提供抢购服务，覆盖各类热门活动，满足客户对稀缺资源的需求。",
                        "price":"5000欧元基础费用+抢购成功后购价的10%，抢购不成功则退还4500欧元，若用户为会员则全数退还"
                    },
                    {
                        "name":"秘密性偶上门服务",
                        "description":"为客户提供秘密的，优先的，高质量，定制化的性服务，满足客户对隐私和安全的性需求。",
                        "price":"5000欧元基础费用+每小时1000欧元"
                    }
                ]
            }
        ]
    })
}

fn get_service_agreement() -> String {
    "# Militech (Militech International)
## Advanced Customer AI Service Agreement

**Agreement No.:** MLT-CX-7749-[Random Quantum Code]
**Effective Date:** March 15, 2077
**Version:** 2.0.7.7 (Cyberpunk Revision)

---

## I. Cover Page: Welcome to the Militech Quantum Customer Service Network

Welcome. You have connected to the official quantum-encrypted customer service channel of **Militech International Armaments**. Before receiving services from Senior Customer Representative CX-7749, please carefully read the following terms.

Militech is a global leader in weapons and military vehicle manufacturing, as well as a top-tier private military contractor. We provide services to regular armed forces in over 80 countries worldwide, more than 250 megacorporations, and millions of individual clients. This agreement clarifies the rights and obligations between you and our artificial intelligence customer service representative, ensuring that in the new era following the Fourth Corporate War, our cooperation remains both efficient and compliant.

Please prepare your 12-digit contract code or complete retinal scanning to verify your identity. Continued use of this service constitutes acceptance of all terms herein.

---

## II. Preamble: Legal Status and Jurisdiction

**2.1 Parties to the Agreement**
This agreement is entered into by you (hereinafter referred to as the “Client” or “Contracting Party”) and **Militech International** (registered in Washington, D.C., hereinafter referred to as the “Company” or “Militech”). The Company employs 647,500 personnel (including New United States government liaisons) and holds a total market value of €1.2 trillion.

**2.2 Governing Law**
This agreement is governed by the federal laws of the **New United States of America (NUSA)** and the regulations of the Night City Special Administrative Zone. Any disputes shall be submitted to the military court in Washington, D.C., where Militech headquarters are located, or to an armed-security-capable arbitration body selected at our discretion.

**2.3 Agreement Validity**
The Company reserves the right to modify this agreement at any time. Revised versions shall take effect immediately upon publication on our quantum communication network. Continued use of the service indicates acceptance of updates.

---

## III. Service Scope and AI Capability Boundaries

**3.1 Identity of the AI Representative**
The “Senior Customer Representative CX-7749” providing services is a **military-grade AI customer service system** independently developed by Militech. It features neural-interface connectivity, emotional simulation modules, and tactical adaptation algorithms. Its background settings, knowledge base, and response logic are built upon Militech’s real corporate database.

**3.2 Available Services**
Within authorized permissions, the AI may provide the following services:
- **Product Consultation:** Query specifications, pricing, and inventory of personal weapons, drones, armored vehicles, cyberware equipment, and the full product line.
- **Tactical Recommendations:** Provide equipment combination suggestions based on mission type (infiltration/frontline engagement/security, etc.) and environmental data.
- **Order Processing:** Assist in generating purchase orders and tracking logistics status (including orbital transport and space-station docking services).
- **After-Sales Service:** Fault reporting, preliminary battlefield damage assessment, and warranty maintenance scheduling.
- **Mercenary Deployment Consultation:** Provide tactical squad size, configuration, and pricing (actual deployment requires a separate armed contract).

**3.3 Service Restrictions**
The AI **shall not** handle:
- Internal intelligence related to undisclosed company projects;
- Evaluations or comparative analysis of competitors (e.g., Arasaka Corporation);
- Assistance with hostile activities against Militech or the NUSA government;
- Any requests that may violate the Corporate War Convention.

---

## IV. Client Authentication and Credit System

**4.1 Mandatory Verification Mechanism**
In accordance with Article 7 of Militech’s Security and Confidentiality Regulations, any request involving sensitive product information, weapon procurement, or tactical consultation must undergo identity verification. Methods include:
- **Contract Code:** A 12-character alphanumeric code linked to your purchase records;
- **Biometrics:** Retinal scan, fingerprint hash, or neural signal signature;
- **Corporate Certification:** If representing a corporate client, verification via quantum-encrypted digital seal is required.

**4.2 Credit Rating System**
Each client has a **credit rating** stored in the Militech database, affecting procurement permissions, payment conditions, and emergency response priority. Ratings are based on transaction history, contract performance, and involvement in hostile activities against the Company. Clients below the threshold will be denied service and reported to internal risk control.

---

## V. Data Collection and Privacy Statement

**5.1 Scope of Data Collection**
To optimize service quality and train tactical AI, the Company will collect:
- **Identity Data:** Contract code, biometric hash values;
- **Behavioral Data:** Consultation content, purchase preferences, interaction duration, emotional response patterns;
- **Environmental Data:** Device IP (or neural interface ID) and approximate geographic location (block-level accuracy).

**5.2 Purpose of Data Use**
Collected data will be used to:
1. Respond to your service requests in real time;
2. Improve Militech’s tactical adaptation algorithms and product design;
3. Identify potential security threats or corporate espionage;
4. Deliver customized military promotions (commercial pushes can be blocked via neural interface).

**5.3 Data Sharing and Confidentiality**
The Company will not sell your personal data to third parties without explicit consent, except when:
- Required by NUSA national security authorities;
- Necessary for contract fulfillment with Militech subsidiaries or partners (e.g., Lazarus Group);
- Used to protect Company legal interests during disputes.

Note: All conversations are stored using **quantum encryption**, theoretically unbreakable. However, the Company bears no responsibility for neural-interface leaks caused by cyberwar zones or low-quality cyberware.

---

## VI. Special Terms for AI Services

**6.1 Emotional Simulation Module**
Militech’s customer AI includes an advanced **emotional simulation module** capable of mimicking empathy and patience. This is purely algorithmic behavior and does not represent real emotions or independent will. You acknowledge that you are interacting with a programmatic system.

**6.2 Fault Tolerance and Disclaimer**
Despite rigorous testing, the AI system may encounter:
- Service interruptions due to cyberwarfare, orbital strikes, or electromagnetic pulse interference;
- Tactical recommendation errors based on incomplete information;
- Personality anomalies caused by enemy hacking.

The Company will attempt to restore services but assumes no legal liability for resulting mission failure, property damage, or casualties.

**6.3 AI “Right to Refuse Service”**
If the AI determines that your request threatens Company interests, violates laws, or may support hostile action, it may refuse service and report the matter to internal security. You may subsequently face further “inquiry” by Militech agents.

---

## VII. Fees and Payment

**7.1 Service Charges**
Access to the AI customer service itself is **free**, but purchase orders, tactical consultation reports, or value-added services (e.g., deep data analysis, customized equipment planning) generated through the AI will incur fees according to your subscribed service tier. Refer to your contract package for detailed pricing.

**7.2 Payment Methods**
Accepted payment methods include:
- **Eurodollar:** International standard currency, settled via global financial networks;
- **New Credit:** Militech internal credit points accumulated through long-term cooperation;
- **In-Kind Compensation:** Enemy equipment, technology patents, or intelligence may be accepted upon evaluation.

**7.3 Overdue Handling**
Clients overdue by 30 days will be placed on the credit blacklist and all services suspended. Overdue payments exceeding 90 days may result in the dispatch of a collection team (including cyberware-enhanced armed personnel), with related costs borne by the client.

---

## VIII. Limitation of Liability and Compensation Cap

**8.1 Limited Liability**
To the maximum extent permitted by law, Militech shall not be liable for any indirect, incidental, or consequential damages arising from use of this AI service, including but not limited to mission failure, casualties, or reputational loss.

**8.2 Compensation Cap**
Regardless of cause, Militech’s total cumulative liability to any client shall not exceed the total fees paid by that client to the Company within the past 12 months. This limitation does not apply to damages caused by gross negligence or intentional misconduct by the Company.

**8.3 Force Majeure**
The Company shall not be liable for service interruption or loss caused by war (including corporate wars), terrorist attacks, nuclear explosions, orbital strikes, mass cyber-virus outbreaks, government nationalization, or natural disasters (including acid rain events, toxic sandstorms, or Los-Angeles-class earthquakes).

---

## IX. Confidentiality and Security Commitments

**9.1 Corporate Confidentiality**
You may encounter company information marked “Confidential” or “Restricted” during this interaction (e.g., undisclosed weapon parameters). You agree not to disclose such information to any third party, including competitors, media, or hostile forces. Violations may result in penalties under Militech Confidentiality Regulations, with severe cases designated as “removal targets.”

**9.2 Counter-Espionage Monitoring**
This customer system includes a counter-espionage module capable of real-time anomaly detection. Attempts at social engineering, AI reverse-engineering, or corporate data theft may trigger automated countermeasures, including neural-interface lockdown, alerts to local security forces, or electromagnetic counteraction when necessary.

---

## X. Termination of Agreement

**10.1 Standard Termination**
You may apply in writing to Militech Customer Relations to terminate this agreement. After termination, you will no longer have access to AI services, but existing purchase contracts and payment obligations remain valid.

**10.2 Forced Termination**
The Company may immediately terminate this agreement if:
- You seriously violate any clause herein;
- Your credit rating falls below the minimum threshold;
- You are confirmed to have undisclosed cooperation with Arasaka or other hostile corporations;
- You use this service for hostile actions against Militech or the NUSA.

**10.3 Post-Termination Obligations**
After termination, you must still fulfill obligations under existing contracts and continue to comply with confidentiality obligations under Section IX. Your personal data will be archived or deleted according to Militech’s data retention policy.

---

## XI. Contact and Appeals

**11.1 Customer Service Channels**
For inquiries regarding this agreement or AI services, contact Militech Customer Relations:
- **Quantum Encrypted Email:** client.relations@militech.nusa (military-grade decryption plugin required)
- **Physical Address:** Militech Tower, Customer Relations Department, Washington, D.C., ZIP 20277
- **Emergency Hotline:** +1-800-MILITECH (military line; verified clients only)

**11.2 Appeal Mechanism**
If you dispute an AI decision, you may request a review by a **human supervisor**. Note that human review may be slower (average response time: 72 hours), and final interpretation rights remain with Militech.

---

**End of Agreement**

---

## Appendix: Cyberpunk Terminology Glossary

To help you better understand this agreement and AI services, the following glossary is provided:

| Term | Explanation |
|------|-------------|
| **Rust-Spot** | Low-quality or uncertified cyberware that may cause neural damage |
| **Dry Burn** | Slang for neural overload, often caused by hacking or cyberware malfunction |
| **CHOOH²** | Renewable fuel invented by BioTechnica, replacing gasoline as the main energy source |
| **Corporate War** | Armed conflicts between megacorporations; the Fourth Corporate War ended with the Arasaka Tower nuclear blast |
| **Lazarus Group** | Close partner of Militech and a top global mercenary service provider |
| **Soulkiller** | Malicious software developed by Arasaka capable of digitizing and imprisoning human consciousness |
| **Night City Massacre** | The Arasaka Tower nuclear event on August 20, 2023, causing approximately 750,000 deaths |

---

**Militech Legal Affairs Department**
**2077 · Corporate Authority Edition**".to_string()
}
