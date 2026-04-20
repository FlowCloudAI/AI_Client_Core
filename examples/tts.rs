use std::path::PathBuf;
use flowcloudai_client::audio::{AudioDecoder, AudioSource};
use flowcloudai_client::tts::{TTSRequest, AudioFormat};
use flowcloudai_client::FlowCloudAIClient;
mod apis;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut client = FlowCloudAIClient::new(PathBuf::from("./plugins"), None)?;
    client.load_plugin("qwen-tts")?;

    let tts = client.create_tts_session("qwen-tts", apis::QWEN_LLM.key, None)?;

    // ── 方式 1：最简调用 + 播放 ──
    let result = tts.speak("qwen3-tts-flash", "你好，世界。", "Ethan").await?;

    if result.audio.is_empty() {
        if let Some(url) = &result.url {
            println!("Audio URL: {}", url);
            // 用 HTTP client 下载 url 内容
        }
    } else {
        std::fs::write("output.mp3", &result.audio)?;
    }

    if let Some(ref url) = result.url {
        AudioDecoder::play_source(&AudioSource::Url(url.clone()), Some("wav")).await?;
    } else if !result.audio.is_empty() {
        AudioDecoder::play_source(&AudioSource::Raw(result.audio.clone()), Some(&result.format)).await?;
    }

    // ── 方式 2：完整参数 + 保存文件 ──
    let req = TTSRequest::new("qwen3-tts-flash", "今天天气真不错！", "Cherry")
        .format(AudioFormat::Mp3)
        .speed(2.0)
        .language("Chinese");

    let result = tts.synthesize(&req).await?;

    if !result.audio.is_empty() {
        std::fs::write("output.mp3", &result.audio)?;
        println!("saved {} bytes, duration {:?}ms", result.audio.len(), result.duration_ms);

        let pcm = AudioDecoder::decode(&result.audio, Some("mp3"))?;
        println!("{}Hz, {} channels, {} samples", pcm.sample_rate, pcm.channels, pcm.samples.len());
    } else if let Some(ref url) = result.url {
        println!("Audio URL: {}", url);
        AudioDecoder::play_source(&AudioSource::Url(url.clone()), Some("wav")).await?;
    } else {
        println!("No audio data received");
    }

    // ── 方式 3：只解码不播放 ──
    if !result.audio.is_empty() {
        // 方式 3
        let pcm = AudioDecoder::decode(&result.audio, Some("mp3"))?;
        println!("{}Hz, {} channels, {} samples", pcm.sample_rate, pcm.channels, pcm.samples.len());
    }

    // ── 方式 4：从字符串自动检测编码 ──
    let source = AudioSource::detect("68656c6c6f");  // hex
    let bytes = AudioDecoder::resolve(&source).await?;
    println!("resolved {} bytes", bytes.len());

    Ok(())
}