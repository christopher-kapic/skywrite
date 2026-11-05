//! Narration and music use the shared durable paid-job store.
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::Args;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    time::Duration,
};
pub(crate) const MAX_AUDIO: usize = 32 * 1024 * 1024;
const MAX_WIRE: usize = 64 * 1024 * 1024;
#[derive(Debug, Args)]
pub struct SpeechArgs {
    /// Exact narration text, at most 64 KiB. Text longer than one request for the
    /// selected model is split at paragraph breaks and joined into one output.
    #[arg(long, required_unless_present = "file", conflicts_with = "file")]
    pub text: Option<String>,
    /// UTF-8 script, protected from output overwrite.
    #[arg(long, required_unless_present = "text", conflicts_with = "text")]
    pub file: Option<PathBuf>,
    /// Provider voice ID from models list --task speech-generation.
    #[arg(long)]
    pub voice: Option<String>,
    /// Delivery guidance; support varies by provider.
    #[arg(long)]
    pub instructions: Option<String>,
    /// Speaking speed, only for documented compatible adapters.
    #[arg(long)]
    pub speed: Option<f64>,
    /// Seconds of silence added between split sections at paragraph breaks (0-5).
    /// Only applies when the script is too long for one request.
    #[arg(long, default_value_t = 0.3)]
    pub paragraph_pause: f64,
    /// Project nodes record one job each, so they must fit one request.
    #[arg(skip)]
    pub single_request: bool,
    #[command(flatten)]
    pub common: crate::generation::Common,
}
#[derive(Debug, Args)]
pub struct MusicArgs {
    #[arg(long)]
    pub prompt: String,
    /// Request no vocals; listen to verify model compliance.
    #[arg(long)]
    pub instrumental: bool,
    #[arg(long)]
    pub reference: Option<PathBuf>,
    #[command(flatten)]
    pub common: crate::generation::Common,
}
fn text_check(text: &str, max: usize, what: &str) -> Result<()> {
    if text.trim().is_empty() || text.len() > max || text.contains('\0') {
        bail!("{what} must contain 1..{max} UTF-8 bytes without NUL");
    }
    Ok(())
}
pub fn speech(args: SpeechArgs) -> Result<Value> {
    let mut refs = Vec::new();
    let text = if let Some(path) = &args.file {
        let path = crate::media::canonical_file(path)?;
        let bytes =
            crate::generation::read_bounded_file(&path, crate::speech_split::MAX_SCRIPT_BYTES)?;
        let hash = crate::generation::bytes_sha256(&bytes);
        let text = String::from_utf8(bytes).context("narration script must be UTF-8")?;
        refs.push(json!({"path":path,"sha256":hash,"kind":"narration-script"}));
        text
    } else {
        args.text.context("provide --text or --file")?
    };
    text_check(
        &text,
        crate::speech_split::MAX_SCRIPT_BYTES,
        "narration text",
    )?;
    if !args.paragraph_pause.is_finite() || !(0.0..=5.0).contains(&args.paragraph_pause) {
        bail!("--paragraph-pause must be between 0 and 5 seconds");
    }
    if let Some(v) = &args.instructions {
        text_check(v, 4096, "delivery instructions")?;
    }
    let model = crate::models::resolve_model("speech-generation", args.common.model.as_deref())?;
    let caps = crate::models::find_model(
        "speech-generation",
        &model,
        args.common.cache_dir.as_deref(),
        args.common.offline,
    )?;
    if model.starts_with("google/") || model.starts_with("heygen/") {
        bail!(
            "this narration surface requires an MP3-capable speech model; selected provider documents PCM-only output; choose a listed MP3 model such as hexgrad/kokoro-82m"
        );
    }
    if let Some(voices) = caps["supported_voices"]
        .as_array()
        .filter(|v| !v.is_empty())
    {
        let voice=args.voice.as_ref().context("selected model requires --voice; inspect supported_voices in models list --task speech-generation")?;
        if !voices.iter().any(|v| v.as_str() == Some(voice)) {
            bail!("voice is not in selected model's supported_voices");
        }
    }
    if args
        .voice
        .as_ref()
        .is_some_and(|v| v.is_empty() || v.len() > 256 || v.chars().any(char::is_control))
    {
        bail!("voice ID must contain 1..256 printable UTF-8 bytes");
    }
    if let Some(speed) = args.speed {
        if !speed.is_finite() || !(0.5..=2.0).contains(&speed) {
            bail!("speech speed must be between 0.5 and 2.0");
        }
        if !model.starts_with("microsoft/mai-voice-") && model != "bytedance-seed/seed-audio-1-0" {
            bail!("speed control is not documented for this speech adapter; omit --speed");
        }
    }
    let budget = crate::speech_split::budget(&model, &caps);
    let split = crate::speech_split::needs_split(&text, budget.chars);
    if split && args.single_request {
        bail!(
            "speech node text exceeds one request for {model} ({} characters, from {}); split it into several speech nodes at paragraph breaks",
            budget.chars,
            budget.source
        );
    }
    let mut body = json!({"model":model,"input":text,"response_format":"mp3"});
    if let Some(v) = args.voice {
        body["voice"] = json!(v);
    }
    if let Some(v) = args.instructions {
        body["instructions"] = json!(v);
    }
    if let Some(v) = args.speed {
        body["speed"] = json!(v);
    }
    if split {
        return crate::speech_split::run(
            crate::speech_split::Plan {
                model,
                body,
                references: refs,
                budget,
                paragraph_pause: args.paragraph_pause,
                common: args.common,
            },
            &text,
        );
    }
    crate::generation::submit_audio("speech-generation", model, body, refs, args.common)
}
pub fn music(args: MusicArgs) -> Result<Value> {
    text_check(&args.prompt, 32 * 1024, "music prompt")?;
    let model = crate::models::resolve_model("music-generation", args.common.model.as_deref())?;
    let caps = crate::models::find_model(
        "music-generation",
        &model,
        args.common.cache_dir.as_deref(),
        args.common.offline,
    )?;
    let prompt = if args.instrumental {
        format!(
            "{}\nRequested arrangement: instrumental only, no vocals or speech.",
            args.prompt
        )
    } else {
        args.prompt
    };
    let mut content = vec![json!({"type":"text","text":prompt})];
    let mut refs = Vec::new();
    if let Some(path) = args.reference {
        if !caps["architecture"]["input_modalities"]
            .as_array()
            .is_some_and(|v| v.iter().any(|m| m == "image"))
        {
            bail!("selected music model does not advertise image input");
        }
        content.push(crate::generation::audio_image_reference(&path, &mut refs)?);
    }
    let body = json!({"model":model,"messages":[{"role":"user","content":content}],"modalities":["text","audio"],"audio":{"format":"mp3"},"stream":true});
    crate::generation::submit_audio("music-generation", model, body, refs, args.common)
}

pub(crate) fn parse_music_stream(reader: impl Read) -> Result<Value> {
    let mut reader = BufReader::new(reader.take(MAX_WIRE as u64 + 1));
    let mut line = Vec::new();
    let mut event = String::new();
    let mut total = 0usize;
    let mut audio = Vec::new();
    let mut text = String::new();
    let mut usage = Value::Null;
    let mut generation_id = String::new();
    let mut done = false;
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        total = total
            .checked_add(n)
            .context("music response size overflow")?;
        if total > MAX_WIRE {
            bail!("music response exceeds 64 MiB");
        }
        if n == 0 {
            break;
        }
        let value = std::str::from_utf8(&line)
            .context("music SSE must be UTF-8")?
            .trim_end_matches(['\r', '\n']);
        if value.is_empty() {
            if !event.is_empty() {
                if event == "[DONE]" {
                    done = true;
                    break;
                }
                let chunk: Value =
                    serde_json::from_str(&event).context("invalid music SSE JSON")?;
                if !chunk["error"].is_null() {
                    bail!("music provider returned an error event");
                }
                if let Some(id) = chunk["id"].as_str() {
                    if id.len() > 256 || id.chars().any(char::is_control) {
                        bail!("invalid music generation ID");
                    }
                    if !generation_id.is_empty() && generation_id != id {
                        bail!("music stream changed generation identity");
                    }
                    generation_id = id.into();
                }
                if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
                    if serde_json::to_vec(u)?.len() > 64 * 1024 {
                        bail!("music usage exceeds 64 KiB");
                    }
                    usage = u.clone();
                }
                if let Some(choices) = chunk["choices"].as_array() {
                    if choices.len() > 1 {
                        bail!("expected one music output choice");
                    }
                    for choice in choices {
                        if let Some(t) = choice["delta"]["content"].as_str() {
                            if text.len().saturating_add(t.len()) > 32 * 1024 {
                                bail!("music response text exceeds 32 KiB");
                            }
                            text.push_str(t);
                        }
                        if let Some(data) = choice["delta"]["audio"]["data"].as_str() {
                            if data.len() > MAX_AUDIO * 4 / 3 + 8 {
                                bail!("music chunk exceeds audio budget");
                            }
                            let bytes =
                                STANDARD.decode(data).context("music chunk is not base64")?;
                            if audio.len().saturating_add(bytes.len()) > MAX_AUDIO {
                                bail!("music output exceeds 32 MiB");
                            }
                            audio.extend(bytes);
                        }
                    }
                }
                event.clear();
            }
        } else if let Some(data) = value.strip_prefix("data:") {
            if !event.is_empty() {
                event.push('\n');
            }
            event.push_str(data.strip_prefix(' ').unwrap_or(data));
            if event.len() > MAX_WIRE {
                bail!("music event exceeds 64 MiB");
            }
        } else if !value.starts_with(':')
            && !value.starts_with("event:")
            && !value.starts_with("id:")
            && !value.starts_with("retry:")
        {
            bail!("invalid music SSE field");
        }
    }
    if !done || audio.is_empty() {
        bail!(
            "music stream ended without complete audio ({} audio bytes received, {}); do not automatically resubmit",
            audio.len(),
            if done {
                "terminator seen"
            } else {
                "no [DONE] terminator"
            }
        );
    }
    Ok(
        json!({"audio":{"data":STANDARD.encode(audio)},"text":text,"usage":usage,"generation_id":generation_id}),
    )
}
pub(crate) struct AudioData {
    pub bytes: Vec<u8>,
    pub media_type: &'static str,
    pub extension: &'static str,
}
pub(crate) fn audio_data(response: &Value) -> Result<AudioData> {
    let encoded = response["audio"]["data"]
        .as_str()
        .context("audio response lacks encoded data")?;
    if encoded.len() > MAX_AUDIO * 4 / 3 + 8 {
        bail!("audio response exceeds 32 MiB decoded");
    }
    let bytes = STANDARD
        .decode(encoded)
        .context("audio response is not base64")?;
    if bytes.is_empty() || bytes.len() > MAX_AUDIO {
        bail!("audio result must contain 1..32 MiB");
    }
    let (media_type, extension) = if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE")
    {
        ("audio/wav", "wav")
    } else if bytes.starts_with(b"ID3")
        || (bytes.len() > 2 && bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0)
    {
        ("audio/mpeg", "mp3")
    } else {
        bail!("audio result must be actual MP3 or WAV; raw PCM is not a playable container");
    };
    Ok(AudioData {
        bytes,
        media_type,
        extension,
    })
}
pub(crate) fn validate_audio(path: &Path, mime: &str) -> Result<()> {
    if !matches!(mime, "audio/mpeg" | "audio/wav") {
        bail!("unsupported generated audio MIME");
    }
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_AUDIO as u64 {
        bail!("generated audio must be a regular file up to 32 MiB");
    }
    let info = crate::media::probe_for_timing(path)
        .context("generated audio failed decode validation during timing probe")?;
    if !info.has_audio()
        || info.has_video()
        || !info.duration.is_finite()
        || !(0.0..=600.0).contains(&info.duration)
        || info.duration == 0.0
    {
        bail!("generated audio must have an audio stream, no video, and at most 600 seconds");
    }
    let format_ok = if mime == "audio/mpeg" {
        info.format_name.split(',').any(|v| v == "mp3")
    } else {
        info.format_name.split(',').any(|v| v == "wav")
    };
    if !format_ok {
        bail!("generated audio container disagrees with MIME");
    }
    let mut args = crate::process::args(&[
        "-v",
        "error",
        "-nostdin",
        "-xerror",
        "-err_detect",
        "explode",
        "-threads",
        "1",
        "-protocol_whitelist",
        "file,pipe",
        "-i",
    ]);
    args.push(path.as_os_str().to_owned());
    args.extend(crate::process::args(&[
        "-map",
        "0:a:0",
        "-vn",
        "-progress",
        "pipe:1",
        "-nostats",
        "-f",
        "null",
        "-",
    ]));
    let result = crate::process::run("ffmpeg", &args, Duration::from_secs(180), 1024 * 1024)
        .context("generated audio failed decode validation")?;
    if !String::from_utf8_lossy(&result.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("out_time_us="))
        .any(|v| v.parse::<u64>().is_ok_and(|n| n > 0))
    {
        bail!("generated audio failed decode validation: no samples decoded");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(value: Value) -> String {
        format!("data: {}\n\n", value)
    }
    #[test]
    fn streamed_audio_preserves_independently_encoded_chunks_and_usage() {
        let mut stream = String::from(": keepalive\n\n");
        stream += &event(
            json!({"id":"gen-test","choices":[{"delta":{"content":"Original bed","audio":{"data":STANDARD.encode(b"RI")}}}]}),
        );
        stream += &event(
            json!({"id":"gen-test","choices":[{"delta":{"audio":{"data":STANDARD.encode(b"FF1234WAVEpayload")}}}],"usage":{"cost":0.04}}),
        );
        stream += "data: [DONE]\n\n";
        let parsed = parse_music_stream(stream.as_bytes()).unwrap();
        assert_eq!(
            STANDARD
                .decode(parsed["audio"]["data"].as_str().unwrap())
                .unwrap(),
            b"RIFF1234WAVEpayload"
        );
        assert_eq!(parsed["usage"]["cost"], 0.04);
        assert_eq!(parsed["text"], "Original bed");
        assert_eq!(audio_data(&parsed).unwrap().media_type, "audio/wav");
    }
    #[test]
    fn partial_or_invalid_streams_never_become_results() {
        let audio = event(
            json!({"id":"gen-a","choices":[{"delta":{"audio":{"data":STANDARD.encode(b"ID3data")}}}]}),
        );
        assert!(
            parse_music_stream(audio.as_bytes())
                .unwrap_err()
                .to_string()
                .contains("complete audio")
        );
        let other = audio.clone() + &event(json!({"id":"gen-b","choices":[]})) + "data: [DONE]\n\n";
        assert!(
            parse_music_stream(other.as_bytes())
                .unwrap_err()
                .to_string()
                .contains("changed generation")
        );
        assert!(
            parse_music_stream(b"data: {\"error\":{\"message\":\"unavailable\"}}\n\n".as_slice())
                .is_err()
        );
        let invalid = event(json!({"choices":[{"delta":{"audio":{"data":"not base64!"}}}]}))
            + "data: [DONE]\n\n";
        assert!(parse_music_stream(invalid.as_bytes()).is_err());
        let text = event(json!({"choices":[{"delta":{"content":"x".repeat(32*1024+1)}}]}))
            + "data: [DONE]\n\n";
        assert!(
            parse_music_stream(text.as_bytes())
                .unwrap_err()
                .to_string()
                .contains("32 KiB")
        );
    }
    #[test]
    fn metadata_valid_invalid_samples_reject_real_decode() {
        fn wav(data: &[u8]) -> Vec<u8> {
            let mut b = Vec::new();
            b.extend(b"RIFF");
            b.extend((36 + data.len() as u32).to_le_bytes());
            b.extend(b"WAVEfmt ");
            b.extend(16u32.to_le_bytes());
            b.extend(1u16.to_le_bytes());
            b.extend(1u16.to_le_bytes());
            b.extend(44100u32.to_le_bytes());
            b.extend(88200u32.to_le_bytes());
            b.extend(2u16.to_le_bytes());
            b.extend(16u16.to_le_bytes());
            b.extend(b"data");
            b.extend((data.len() as u32).to_le_bytes());
            b.extend(data);
            b
        }
        let temp = tempfile::tempdir().unwrap();
        let valid = temp.path().join("valid.wav");
        let bad = temp.path().join("bad.wav");
        std::fs::write(&valid, wav(&[0u8; 882])).unwrap();
        std::fs::write(&bad, wav(&[1])).unwrap();
        validate_audio(&valid, "audio/wav").unwrap();
        let metadata = crate::media::probe(&bad).unwrap();
        assert!(metadata.has_audio());
        assert!(metadata.duration > 0.0);
        let error = validate_audio(&bad, "audio/wav").unwrap_err();
        assert!(
            format!("{error:#}").contains("decode validation"),
            "{error:#}"
        );
        assert!(
            error
                .chain()
                .skip(1)
                .any(|cause| cause.to_string().contains("ffmpeg")),
            "underlying decoder failure must be preserved: {error:#}"
        );
    }
}
