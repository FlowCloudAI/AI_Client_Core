use std::path::PathBuf;
use anyhow::Context;
use flowcloudai_client::FlowCloudAIClient;
use flowcloudai_client::image::ImageRequest;

mod apis;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut client = FlowCloudAIClient::new(PathBuf::from("./plugins"), None)?;
    client.load_plugin("qwen-image")?;

    let img = client.create_image_session("qwen-image", apis::QWEN_LLM.key, None)?;

    // 文生图
    let result = img.text_to_image("qwen-image-plus", "一只猫在月光下散步").await?;

    for image in &result.images {
        println!("URL: {:?}, size: {:?}", image.url, image.size);
    }

    let reference_image_url = result
        .images
        .iter()
        .find_map(|image| image.url.clone())
        .context("首张文生图结果缺少 URL，无法继续执行图生图示例")?;

    println!("使用参考图进行编辑: {}", reference_image_url);

    // 图文生图
    let result = img.edit_image(
        "qwen-image-2.0",
        "将背景改为雪景",
        &reference_image_url,
    ).await?;

    for image in &result.images {
        println!("URL: {:?}, size: {:?}", image.url, image.size);
    }

    // 组图生成
    let req = ImageRequest::text_to_image("qwen-image-plus", "四季庭院变迁")
        .sequential(4)
        .size("2K")
        .format_png();
    let result = img.generate(&req).await?;
    println!("生成 {} 张图片", result.images.len());
    for image in &result.images {
        println!("URL: {:?}, size: {:?}", image.url, image.size);
    }
    Ok(())
}
