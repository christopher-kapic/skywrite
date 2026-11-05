//! Long narration: split a script at natural breaks into one durable speech job
//! per section, then join the decoded sections into a single published file.
//!
//! Each section is an ordinary generation job keyed by its own request, so
//! rerunning the same command replays finished sections and pays only for the
//! missing ones. Intermediate PCM lives in a temporary directory; only the
//! requested output is written to the user's folders.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

/// Whole-script limit for split narration.
pub(crate) const MAX_SCRIPT_BYTES: usize = 64 * 1024;
/// Per-request ceiling Skywrite applies regardless of model.
pub(crate) const MAX_REQUEST_BYTES: usize = 8192;
/// Section budget when neither Skywrite nor the catalog knows a model's limit.
/// Speech is billed by input length, so small sections cost nothing extra.
const DEFAULT_CHARS: usize = 2000;
/// Floor for sections (and for re-splitting after a length rejection).
const MIN_CHARS: usize = 200;
const MAX_JOINED_SECONDS: f64 = 3600.0;
const RATE: usize = 48_000;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Budget {
    pub chars: usize,
    pub source: &'static str,
}

/// Per-request text budget for a speech model, most specific source first.
pub(crate) fn budget(model: &str, caps: &Value) -> Budget {
    if model == "bytedance-seed/seed-audio-1-0" {
        return Budget {
            chars: 3000,
            source: "skywrite known model limit",
        };
    }
    if let Some(tokens) = caps["context_length"].as_u64().filter(|t| *t > 0) {
        // Two characters per token is conservative for TTS text tokenizers.
        let chars = usize::try_from(tokens.saturating_mul(2)).unwrap_or(usize::MAX);
        return Budget {
            chars: chars.clamp(MIN_CHARS, MAX_REQUEST_BYTES),
            source: "catalog context_length",
        };
    }
    Budget {
        chars: DEFAULT_CHARS,
        source: "conservative default (catalog lists no limit)",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Break {
    Start,
    Paragraph,
    Sentence,
    Word,
}
impl Break {
    fn name(self) -> &'static str {
        match self {
            Break::Start => "start",
            Break::Paragraph => "paragraph",
            Break::Sentence => "sentence",
            Break::Word => "word",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Section {
    pub text: String,
    /// The kind of break between the previous section and this one.
    pub before: Break,
}

fn fits(text: &str, chars: usize) -> bool {
    text.len() <= MAX_REQUEST_BYTES && text.chars().count() <= chars
}

/// Whether the script needs more than one request.
pub(crate) fn needs_split(text: &str, chars: usize) -> bool {
    !fits(text, chars)
}

fn paragraphs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                out.push(current.join("\n").trim().to_owned());
                current.clear();
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        out.push(current.join("\n").trim().to_owned());
    }
    out.retain(|p| !p.is_empty());
    out
}

/// Raw slices ending after sentence punctuation (and any closing quotes).
fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (_, c) = chars[i];
        let cjk = matches!(c, '。' | '！' | '？');
        if cjk || matches!(c, '.' | '!' | '?' | '…') {
            let mut j = i + 1;
            while j < chars.len() && matches!(chars[j].1, '"' | '\'' | '”' | '’' | ')' | ']') {
                j += 1;
            }
            let at_space = j < chars.len() && chars[j].1.is_whitespace();
            if (cjk || at_space) && j < chars.len() {
                let end = chars[j].0;
                out.push(&text[start..end]);
                start = end;
                i = j;
                continue;
            }
        }
        i += 1;
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Raw slices that each end after a run of whitespace.
fn words(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_space = false;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            in_space = true;
        } else if in_space {
            out.push(&text[start..i]);
            start = i;
            in_space = false;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn hard(text: &str, chars: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut count = 0;
    for c in text.chars() {
        if count + 1 > chars || current.len() + c.len_utf8() > MAX_REQUEST_BYTES {
            out.push(std::mem::take(&mut current));
            count = 0;
        }
        current.push(c);
        count += 1;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Greedily packs raw pieces, splitting any oversized piece with `finer`.
fn pack(
    pieces: &[&str],
    chars: usize,
    kind: Break,
    finer: &dyn Fn(&str) -> Vec<(String, Break)>,
) -> Vec<(String, Break)> {
    let mut out: Vec<(String, Break)> = Vec::new();
    let mut current = String::new();
    for piece in pieces {
        let candidate = format!("{current}{piece}");
        if fits(candidate.trim(), chars) {
            current = candidate;
            continue;
        }
        if !current.trim().is_empty() {
            out.push((current.trim().to_owned(), kind));
        }
        current = String::new();
        if fits(piece.trim(), chars) {
            current = (*piece).to_owned();
        } else {
            out.extend(finer(piece.trim()));
        }
    }
    if !current.trim().is_empty() {
        out.push((current.trim().to_owned(), kind));
    }
    out
}

fn split_paragraph(paragraph: &str, chars: usize) -> Vec<(String, Break)> {
    let word_level = |sentence: &str| -> Vec<(String, Break)> {
        let hard_level = |word: &str| -> Vec<(String, Break)> {
            hard(word, chars)
                .into_iter()
                .map(|w| (w, Break::Word))
                .collect()
        };
        pack(&words(sentence), chars, Break::Word, &hard_level)
    };
    pack(&sentences(paragraph), chars, Break::Sentence, &word_level)
}

/// Splits at paragraph breaks, packing whole paragraphs together while they fit;
/// oversized paragraphs fall back to sentences, then words.
pub(crate) fn split(text: &str, chars: usize) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    let mut current = String::new();
    let mut pending = Break::Start;
    let push = |out: &mut Vec<Section>, text: String, before: Break| {
        let before = if out.is_empty() { Break::Start } else { before };
        out.push(Section { text, before });
    };
    for paragraph in paragraphs(text) {
        let candidate = if current.is_empty() {
            paragraph.clone()
        } else {
            format!("{current}\n\n{paragraph}")
        };
        if fits(&candidate, chars) {
            current = candidate;
            continue;
        }
        if !current.is_empty() {
            push(&mut out, std::mem::take(&mut current), pending);
            pending = Break::Paragraph;
        }
        if fits(&paragraph, chars) {
            current = paragraph;
        } else {
            for (index, (piece, kind)) in split_paragraph(&paragraph, chars).into_iter().enumerate()
            {
                push(&mut out, piece, if index == 0 { pending } else { kind });
                pending = Break::Paragraph;
            }
        }
    }
    if !current.is_empty() {
        push(&mut out, current, pending);
    }
    out
}

/// A clear provider rejection for over-long input: the request was refused
/// before generation, so a smaller request is a different, safe submission.
fn length_rejection(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("submission rejected")
        && ["http 400", "http 413", "http 422"]
            .iter()
            .any(|s| lower.contains(s))
        && [
            "too long",
            "length",
            "characters",
            "maximum",
            "exceed",
            "max_",
            "limit",
        ]
        .iter()
        .any(|s| lower.contains(s))
}

pub(crate) struct Plan {
    pub model: String,
    pub body: Value,
    pub references: Vec<Value>,
    pub budget: Budget,
    pub paragraph_pause: f64,
    pub common: crate::generation::Common,
}

fn section_common(common: &crate::generation::Common, model: &str) -> crate::generation::Common {
    crate::generation::Common {
        model: Some(model.to_owned()),
        output: None,
        overwrite: false,
        cache_dir: common.cache_dir.clone(),
        offline: common.offline,
        dry_run: common.dry_run,
        take: common.take.clone(),
    }
}

fn first_words(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().take(8).collect();
    let mut out = words.join(" ");
    if text.split_whitespace().count() > 8 {
        out.push('…');
    }
    out
}

fn break_from(name: &str) -> Result<Break> {
    Ok(match name {
        "start" => Break::Start,
        "paragraph" => Break::Paragraph,
        "sentence" => Break::Sentence,
        "word" => Break::Word,
        _ => bail!("invalid saved section break {name}"),
    })
}

/// The combined job identity: every section request plus how they are joined.
fn identity(plan: &Plan, sections: &[Section]) -> Result<String> {
    let mut template = plan.body.clone();
    template["input"] = Value::Null;
    let sections: Vec<Value> = sections
        .iter()
        .map(|s| json!({"text":s.text,"before":s.before.name()}))
        .collect();
    let joined =
        json!({"template":template,"sections":sections,"paragraph_pause":plan.paragraph_pause});
    Ok(crate::openrouter::cache_key(&[
        b"skywrite-split-speech-v1",
        &serde_json::to_vec(&joined)?,
        plan.common.take.as_deref().unwrap_or("").as_bytes(),
    ]))
}

/// Splits a script and generates (or previews) it as one combined job.
pub(crate) fn run(plan: Plan, text: &str) -> Result<Value> {
    let sections = split(text, plan.budget.chars);
    execute(plan, sections)
}

/// Continues a combined job from its saved plan, never re-splitting the text.
pub(crate) fn resume(
    id: &str,
    saved: Value,
    output: Option<std::path::PathBuf>,
    overwrite: bool,
) -> Result<Value> {
    let sections = saved["sections"]
        .as_array()
        .context("saved narration plan lacks sections")?
        .iter()
        .map(|s| {
            Ok(Section {
                text: s["text"]
                    .as_str()
                    .context("saved section lacks text")?
                    .to_owned(),
                before: break_from(s["before"].as_str().unwrap_or(""))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let plan = Plan {
        model: saved["model"]
            .as_str()
            .context("saved plan lacks model")?
            .to_owned(),
        body: saved["body"].clone(),
        references: saved["references"].as_array().cloned().unwrap_or_default(),
        budget: Budget {
            chars: saved["budget"]["chars"]
                .as_u64()
                .context("saved plan lacks budget")? as usize,
            source: "saved plan",
        },
        paragraph_pause: saved["paragraph_pause"]
            .as_f64()
            .context("saved plan lacks pause")?,
        common: crate::generation::Common {
            model: None,
            output,
            overwrite,
            cache_dir: None,
            offline: true,
            dry_run: false,
            take: saved["take"].as_str().map(str::to_owned),
        },
    };
    if identity(&plan, &sections)? != id {
        bail!("saved narration plan does not match job {id}");
    }
    execute(plan, sections)
}

fn execute(plan: Plan, planned: Vec<Section>) -> Result<Value> {
    let id = identity(&plan, &planned)?;
    let Plan {
        model,
        body,
        references,
        budget,
        paragraph_pause,
        common,
    } = plan;
    let split_info = json!({
        "sections":planned.len(),
        "budget_chars":budget.chars,
        "budget_source":budget.source,
        "paragraph_pause_seconds":paragraph_pause,
    });
    if common.dry_run {
        let mut previews = Vec::new();
        for (i, section) in planned.iter().enumerate() {
            let mut request = body.clone();
            request["input"] = json!(section.text);
            let preview = crate::generation::without_planned_identity(|| {
                crate::generation::submit_audio(
                    "speech-generation",
                    model.clone(),
                    request,
                    references.clone(),
                    section_common(&common, &model),
                )
            })?;
            previews.push(json!({"index":i,"job_id":preview["id"],"chars":section.text.chars().count(),"break_before":section.before.name(),"first_words":first_words(&section.text)}));
        }
        return Ok(json!({
            "dry_run":true,"id":id,"task":"speech-generation","model":model,"paid_request":false,
            "references":references,"requests":planned.len(),"split":split_info,"sections":previews,
            "note":"Long narration is one combined job made of section requests. Speech is usually billed by input length, so the total is close to one long request. Pricing comes from the selected model catalog/endpoint; this preview is not a quote."
        }));
    }
    crate::generation::check_planned_identity(&id)?;
    if let Some(output) = common.output.as_deref() {
        crate::generation::validate_audio_output(output, &references, common.overwrite)?;
    }
    let saved = json!({
        "version":1,"model":model,"body":body,"references":references,
        "budget":{"chars":budget.chars,"source":budget.source},
        "paragraph_pause":paragraph_pause,"take":common.take,
        "sections":planned.iter().map(|s| json!({"text":s.text,"before":s.before.name()})).collect::<Vec<_>>(),
    });
    let mut template = body.clone();
    template["input"] = json!(format!("<{} narration sections>", planned.len()));
    let mut parent =
        crate::generation::SplitJob::open(&id, &model, template, references.clone(), &saved)?;
    if parent.status() != "completed" {
        join_sections(
            &mut parent,
            &model,
            &body,
            &references,
            &common,
            planned,
            paragraph_pause,
            split_info,
        )?;
    }
    if let Some(output) = common.output.as_deref() {
        parent.publish(output, common.overwrite)?;
    }
    let mut out = parent.public()?;
    if let Some(output) = common.output.as_deref() {
        out["output"] = json!(output);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn join_sections(
    parent: &mut crate::generation::SplitJob,
    model: &str,
    body: &Value,
    references: &[Value],
    common: &crate::generation::Common,
    mut sections: Vec<Section>,
    paragraph_pause: f64,
    mut split_info: Value,
) -> Result<()> {
    let mut results: Vec<Value> = Vec::new();
    let mut index = 0;
    while index < sections.len() {
        let mut request = body.clone();
        request["input"] = json!(sections[index].text);
        let outcome = crate::generation::without_planned_identity(|| {
            crate::generation::submit_audio(
                "speech-generation",
                model.to_owned(),
                request,
                references.to_vec(),
                section_common(common, model),
            )
        });
        let rejection = match &outcome {
            Err(error) => Some(format!("{error:#}")),
            Ok(value) if value["status"] == "rejected" => {
                Some(value["error"].as_str().unwrap_or("").to_owned())
            }
            Ok(_) => None,
        };
        if let Some(message) = &rejection
            && length_rejection(message)
        {
            let section = &sections[index];
            let chars = section.text.chars().count() / 2;
            if chars < MIN_CHARS {
                let error = format!(
                    "section {} of {} was rejected as too long even at {} characters: {message}",
                    index + 1,
                    sections.len(),
                    section.text.chars().count()
                );
                parent.block("rejected", error.clone())?;
                bail!(error);
            }
            let mut smaller = split_paragraph(&section.text, chars)
                .into_iter()
                .map(|(text, before)| Section { text, before })
                .collect::<Vec<_>>();
            smaller[0].before = section.before;
            sections.splice(index..=index, smaller);
            continue;
        }
        let (status, detail) = match &outcome {
            Ok(value) if value["status"] == "completed" => {
                results.push(value.clone());
                index += 1;
                continue;
            }
            Ok(value) => (
                value["status"].as_str().unwrap_or("uncertain").to_owned(),
                value["error"]
                    .as_str()
                    .unwrap_or("no detail recorded")
                    .to_owned(),
            ),
            Err(error) => {
                let text = format!("{error:#}");
                let status = if text.contains("Submission rejected") {
                    "rejected"
                } else {
                    "uncertain"
                };
                (status.to_owned(), text)
            }
        };
        let error = format!(
            "narration section {} of {} is {status}: {detail}. Finished sections are kept; once it is resolved, rerun the same command or `skywrite jobs resume JOB_ID` and only missing sections are requested",
            index + 1,
            sections.len()
        );
        let status = if matches!(status.as_str(), "rejected" | "failed") {
            status
        } else {
            "uncertain".into()
        };
        parent.block(&status, error.clone())?;
        bail!(error);
    }
    let temp = tempfile::tempdir()?;
    let pause_samples = (paragraph_pause * RATE as f64).round() as usize;
    let mut joined: Vec<u8> = Vec::new();
    let mut timing = Vec::new();
    for (i, (section, result)) in sections.iter().zip(&results).enumerate() {
        if section.before == Break::Paragraph {
            joined.resize(joined.len() + pause_samples * 2, 0);
        }
        let start = joined.len() / 2;
        let source = result["artifact_path"]
            .as_str()
            .context("completed section lacks an artifact")?;
        let pcm = decode(Path::new(source), &temp.path().join(format!("{i}.raw")))?;
        joined.extend_from_slice(&pcm);
        let end = joined.len() / 2;
        if end as f64 / RATE as f64 > MAX_JOINED_SECONDS {
            bail!("joined narration exceeds {MAX_JOINED_SECONDS} seconds");
        }
        timing.push(json!({
            "index":i,"job_id":result["id"],
            "start_seconds":start as f64 / RATE as f64,"end_seconds":end as f64 / RATE as f64,
            "break_before":section.before.name(),
            "text_sha256":crate::generation::bytes_sha256(section.text.as_bytes()),
            "first_words":first_words(&section.text),
        }));
    }
    let raw = temp.path().join("joined.raw");
    std::fs::write(&raw, &joined)?;
    let mp3 = temp.path().join("joined.mp3");
    let mut args = crate::process::args(&[
        "-v", "error", "-nostdin", "-y", "-f", "s16le", "-ar", "48000", "-ac", "1", "-i",
    ]);
    args.push(raw.into_os_string());
    // LAME's gapless header lets FFmpeg decode back to exact section offsets.
    args.extend(crate::process::args(&[
        "-c:a",
        "libmp3lame",
        "-b:a",
        "160k",
    ]));
    args.push(mp3.clone().into_os_string());
    crate::process::run("ffmpeg", &args, Duration::from_secs(600), 1024 * 1024)
        .context("joining narration sections")?;
    let costs: Vec<Option<f64>> = results
        .iter()
        .map(|r| r["usage"]["cost"].as_f64())
        .collect();
    let usage = if costs.iter().all(Option::is_some) {
        json!({"cost":costs.iter().flatten().sum::<f64>(),"source":"sum of section generation metadata"})
    } else {
        Value::Null
    };
    split_info["sections"] = json!(timing);
    split_info["requests"] = json!(sections.len());
    split_info["duration_seconds"] = json!(joined.len() as f64 / 2.0 / RATE as f64);
    parent.complete(&mp3, usage, split_info)
}

/// Decodes one section to 48 kHz mono PCM16 for sample-exact joining.
fn decode(source: &Path, raw: &Path) -> Result<Vec<u8>> {
    let mut args = crate::process::args(&[
        "-v",
        "error",
        "-nostdin",
        "-xerror",
        "-y",
        "-protocol_whitelist",
        "file,pipe",
        "-i",
    ]);
    args.push(source.as_os_str().to_owned());
    args.extend(crate::process::args(&[
        "-map", "0:a:0", "-vn", "-ac", "1", "-ar", "48000", "-f", "s16le",
    ]));
    args.push(raw.as_os_str().to_owned());
    crate::process::run("ffmpeg", &args, Duration::from_secs(180), 1024 * 1024)
        .context("decoding narration section")?;
    let bytes = std::fs::read(raw)?;
    if bytes.is_empty() {
        bail!("narration section decoded to no samples");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn short_scripts_are_not_split() {
        assert!(!needs_split("Hello there.\n\nSecond paragraph.", 2000));
    }
    #[test]
    fn paragraphs_pack_greedily_and_mark_breaks() {
        let p = "a".repeat(90);
        let text = format!("{p}\n\n{p}\n\n{p}");
        let sections = split(&text, 200);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].text, format!("{p}\n\n{p}"));
        assert_eq!(sections[0].before, Break::Start);
        assert_eq!(sections[1].before, Break::Paragraph);
    }
    #[test]
    fn long_paragraphs_fall_back_to_sentences_then_words() {
        let sentence = format!("{}. ", "word ".repeat(30).trim());
        let paragraph = sentence.repeat(10);
        let sections = split(&paragraph, 200);
        assert!(sections.len() > 1);
        assert!(sections.iter().all(|s| s.text.chars().count() <= 200));
        assert!(sections.iter().skip(1).all(|s| s.before == Break::Sentence));
        assert!(sections.iter().all(|s| s.text.ends_with('.')));
        let rejoined: Vec<&str> = sections
            .iter()
            .flat_map(|s| s.text.split_whitespace())
            .collect();
        let original: Vec<&str> = paragraph.split_whitespace().collect();
        assert_eq!(rejoined, original);
        let run_on = "word ".repeat(100);
        let words = split(&run_on, 200);
        assert!(words.iter().skip(1).all(|s| s.before == Break::Word));
        assert!(words.iter().all(|s| s.text.chars().count() <= 200));
    }
    #[test]
    fn multibyte_text_respects_request_bytes_and_cjk_sentences() {
        let text = "これは文です。".repeat(800);
        let sections = split(&text, 8192);
        assert!(sections.len() > 1);
        assert!(sections.iter().all(|s| s.text.len() <= MAX_REQUEST_BYTES));
        assert!(sections.iter().all(|s| s.text.ends_with('。')));
    }
    #[test]
    fn budgets_prefer_known_limits_then_catalog() {
        let none = json!({"context_length":0});
        assert_eq!(budget("bytedance-seed/seed-audio-1-0", &none).chars, 3000);
        assert_eq!(
            budget("hexgrad/kokoro-82m", &json!({"context_length":4096})).chars,
            8192
        );
        assert_eq!(budget("x/y", &json!({"context_length":1000})).chars, 2000);
        assert_eq!(budget("x/y", &none).chars, DEFAULT_CHARS);
    }
    #[test]
    fn only_clear_length_rejections_trigger_resplitting() {
        assert!(length_rejection(
            "Submission rejected: OpenRouter HTTP 400: text exceeds maximum length of 2000 characters"
        ));
        assert!(!length_rejection(
            "Submission rejected: OpenRouter HTTP 401: invalid key"
        ));
        assert!(!length_rejection(
            "Submission outcome uncertain: connection reset (length unknown)"
        ));
    }

    /// Loopback provider: answers each POST from `responses`, records request inputs.
    fn provider(
        responses: Vec<Vec<u8>>,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        std::thread::JoinHandle<()>,
    ) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = seen.clone();
        let handle = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            for response in responses {
                let started = std::time::Instant::now();
                let (mut socket, _) = loop {
                    match listener.accept() {
                        Ok(v) => break v,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                started.elapsed() < Duration::from_secs(10),
                                "fixture missing expected request"
                            );
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                let end = loop {
                    let n = socket.read(&mut buffer).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let header = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                let length: usize = header
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < end + length {
                    let n = socket.read(&mut buffer).unwrap();
                    bytes.extend_from_slice(&buffer[..n]);
                }
                let body: Value = serde_json::from_slice(&bytes[end..end + length]).unwrap();
                observed
                    .lock()
                    .unwrap()
                    .push(body["input"].as_str().unwrap().to_owned());
                socket.write_all(&response).unwrap();
            }
        });
        (base, seen, handle)
    }
    fn http(status: &str, kind: &str, body: &[u8]) -> Vec<u8> {
        [
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes(),
            body.to_vec(),
        ]
        .concat()
    }
    fn plan(output: &Path, overwrite: bool) -> Plan {
        Plan {
            model: "test/speech".into(),
            body: json!({"model":"test/speech","input":"","voice":"v","response_format":"mp3"}),
            references: Vec::new(),
            budget: Budget {
                chars: 1100,
                source: "test",
            },
            paragraph_pause: 0.5,
            common: crate::generation::Common {
                model: None,
                output: Some(output.to_owned()),
                overwrite,
                cache_dir: None,
                offline: true,
                dry_run: false,
                take: None,
            },
        }
    }
    #[test]
    fn long_narration_joins_sections_resplits_rejections_and_replays_free() {
        if std::env::var_os("SKYWRITE_SPLIT_CHILD").is_none() {
            let home = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "speech_split::tests::long_narration_joins_sections_resplits_rejections_and_replays_free",
                    "--nocapture",
                ])
                .env("SKYWRITE_SPLIT_CHILD", "1")
                .env("SKYWRITE_HOME", home.path())
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let wav = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        let mut args = crate::process::args(&[
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=0.25",
            "-acodec",
            "pcm_s16le",
        ]);
        args.push(wav.path().as_os_str().to_owned());
        crate::process::run("ffmpeg", &args, Duration::from_secs(10), 1024 * 1024).unwrap();
        let audio = http("200 OK", "audio/wav", &std::fs::read(wav.path()).unwrap());
        let rejection = http(
            "400 Bad Request",
            "application/json",
            br#"{"error":{"message":"text exceeds maximum length"}}"#,
        );
        let short = format!("{}.", "one two three ".repeat(10).trim());
        let long = (0..13)
            .map(|i| format!("Sentence {i} {}.", "four five six ".repeat(5).trim()))
            .collect::<Vec<_>>()
            .join(" ");
        let closing = format!("{}.", "seven eight nine ".repeat(10).trim());
        let text = format!("{short}\n\n{long}\n\n{closing}");
        let pieces = split_paragraph(&long, long.chars().count() / 2).len();
        assert!(pieces >= 2);
        let mut responses = vec![audio.clone(), rejection];
        responses.extend(std::iter::repeat_n(audio.clone(), pieces + 1));
        let (base, seen, handle) = provider(responses);
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("narration.mp3");
        let result =
            crate::generation::with_test_transport(&base, || run(plan(&output, false), &text))
                .unwrap();
        handle.join().unwrap();
        assert_eq!(result["status"], "completed", "{result}");
        assert!(output.is_file());
        let sections = result["split"]["sections"].as_array().unwrap();
        assert_eq!(sections.len(), pieces + 2, "{result}");
        let at = |i: usize, key: &str| sections[i][key].as_f64().unwrap();
        // Paragraph seams get the pause; sentence seams inside a re-split paragraph do not.
        assert_eq!(sections[1]["break_before"], "paragraph");
        assert!((at(1, "start_seconds") - at(0, "end_seconds") - 0.5).abs() < 1e-9);
        assert_eq!(sections[2]["break_before"], "sentence");
        assert!((at(2, "start_seconds") - at(1, "end_seconds")).abs() < 1e-9);
        let last = sections.len() - 1;
        assert_eq!(sections[last]["break_before"], "paragraph");
        // The gapless MP3 artifact decodes back to the recorded total length.
        let decoded = decode(
            Path::new(result["artifact_path"].as_str().unwrap()),
            &dir.path().join("check.raw"),
        )
        .unwrap();
        assert_eq!(
            decoded.len() as f64 / 2.0 / RATE as f64,
            at(last, "end_seconds")
        );
        let posted = seen.lock().unwrap().clone();
        assert_eq!(posted.len(), pieces + 3);
        assert_eq!(
            posted[1], long,
            "the rejected request carried the whole paragraph"
        );
        // lavfi sine is 1/8 amplitude (about -18 dBFS): measured, no warning.
        let peak = result["artifact"]["levels"]["peak_dbfs"].as_f64().unwrap();
        assert!((peak + 18.06).abs() < 0.6, "{peak}");
        assert!(result["warnings"].is_null());
        let id = result["id"].as_str().unwrap().to_owned();
        // A rerun replays the combined job: no provider at all.
        let again = crate::generation::with_test_transport("http://127.0.0.1:9", || {
            run(plan(&output, true), &text)
        })
        .unwrap();
        assert_eq!(again["id"], result["id"]);
        assert_eq!(again["split"], result["split"]);
        // Without --overwrite the existing output is refused before any request.
        assert!(
            crate::generation::with_test_transport("http://127.0.0.1:9", || run(
                plan(&output, false),
                &text
            ))
            .is_err()
        );
        // jobs status/resume treat the combined job like any other.
        let status = crate::generation::execute_jobs(crate::generation::JobsCommand::Status {
            id: id.clone(),
            refresh: false,
        })
        .unwrap();
        assert_eq!(status["status"], "completed");
        let wav = dir.path().join("again.wav");
        crate::generation::with_test_transport("http://127.0.0.1:9", || {
            crate::generation::execute_jobs(crate::generation::JobsCommand::Resume {
                id: id.clone(),
                output: Some(wav.clone()),
                overwrite: false,
                wait: 0,
                remote_id: None,
            })
        })
        .unwrap();
        assert!(wav.is_file());
    }
}
