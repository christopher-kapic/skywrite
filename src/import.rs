//! Bring an NLE timeline back into a version 2 edit.
//!
//! Final Cut Pro 7 XML (xmeml, what Premiere exports with File > Export >
//! Final Cut Pro XML) is the supported path. Premiere `.prproj` files are read
//! experimentally: they are undocumented, versioned, gzip-compressed object
//! graphs, so results are labelled and anything unrecognised is reported.
use anyhow::{Context, Result, bail, ensure};
use quick_xml::{XmlVersion, events::Event};
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
};

const MAX_INPUT: u64 = 256 * 1024 * 1024;
const MAX_NODES: usize = 4_000_000;
/// Premiere project time units per second.
const PRPROJ_TICKS: f64 = 254_016_000_000.0;

#[derive(Debug, Default)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
    pub text: String,
}
impl Node {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }
    fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }
    fn path(&self, path: &[&str]) -> Option<&Node> {
        path.iter().try_fold(self, |node, name| node.child(name))
    }
    fn text_of(&self, path: &[&str]) -> Option<&str> {
        self.path(path).map(|n| n.text.trim())
    }
    fn number(&self, path: &[&str]) -> Option<f64> {
        self.text_of(path)?.parse().ok()
    }
    fn descendants<'a>(&'a self, out: &mut Vec<&'a Node>) {
        for child in &self.children {
            out.push(child);
            child.descendants(out);
        }
    }
}

/// A small DOM on quick-xml events; entities and character references resolved.
pub fn parse_xml(text: &str) -> Result<Node> {
    let mut reader = quick_xml::Reader::from_str(text);
    let mut stack = vec![Node::default()];
    let mut count = 0usize;
    let element = |start: &quick_xml::events::BytesStart<'_>| -> Result<Node> {
        let name = String::from_utf8_lossy(start.local_name().as_ref()).into_owned();
        let mut attrs = Vec::new();
        for attr in start.attributes() {
            let attr = attr?;
            attrs.push((
                String::from_utf8_lossy(attr.key.local_name().as_ref()).into_owned(),
                attr.normalized_value(XmlVersion::Implicit1_0)?.into_owned(),
            ));
        }
        Ok(Node {
            name,
            attrs,
            ..Node::default()
        })
    };
    loop {
        count += 1;
        ensure!(count <= MAX_NODES, "XML document is too large");
        match reader.read_event().context("invalid XML")? {
            Event::Start(start) => stack.push(element(&start)?),
            Event::Empty(start) => {
                let node = element(&start)?;
                stack.last_mut().context("XML nesting")?.children.push(node);
            }
            Event::End(_) => {
                let node = stack.pop().context("XML nesting")?;
                stack
                    .last_mut()
                    .context("unbalanced XML")?
                    .children
                    .push(node);
            }
            Event::Text(text) => {
                let content = text.xml_content(XmlVersion::Implicit1_0)?;
                stack
                    .last_mut()
                    .context("XML nesting")?
                    .text
                    .push_str(&content);
            }
            Event::CData(data) => {
                let content = String::from_utf8_lossy(&data.into_inner()).into_owned();
                stack
                    .last_mut()
                    .context("XML nesting")?
                    .text
                    .push_str(&content);
            }
            Event::GeneralRef(reference) => {
                let resolved = if reference.is_char_ref() {
                    reference.resolve_char_ref()?.map(String::from)
                } else {
                    match reference.decode()?.as_ref() {
                        "amp" => Some("&".into()),
                        "lt" => Some("<".into()),
                        "gt" => Some(">".into()),
                        "quot" => Some("\"".into()),
                        "apos" => Some("'".into()),
                        _ => None,
                    }
                };
                if let Some(value) = resolved {
                    stack
                        .last_mut()
                        .context("XML nesting")?
                        .text
                        .push_str(&value);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    ensure!(stack.len() == 1, "unbalanced XML");
    let mut root = stack.pop().expect("root");
    ensure!(
        !root.children.is_empty(),
        "XML document has no root element"
    );
    Ok(root.children.remove(0))
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "import input must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_INPUT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_INPUT,
        "import input exceeds 256 MiB"
    );
    Ok(bytes)
}

/// Decode an xmeml or Premiere project into an edit document and notes.
pub fn import(path: &Path) -> Result<(Value, Vec<String>)> {
    let bytes = read_bounded(path)?;
    if bytes.starts_with(&[0x1f, 0x8b]) || path.extension().is_some_and(|e| e == "prproj") {
        let mut text = String::new();
        flate2::read::GzDecoder::new(bytes.as_slice())
            .take(MAX_INPUT * 4)
            .read_to_string(&mut text)
            .context("a .prproj must be gzip-compressed XML")?;
        return prproj(&parse_xml(&text)?);
    }
    let text = String::from_utf8(bytes).context("XML must be UTF-8")?;
    let root = parse_xml(&text)?;
    match root.name.as_str() {
        "xmeml" => xmeml(&root),
        "PremiereData" => prproj(&root),
        other => bail!(
            "unsupported document <{other}>; import Final Cut Pro 7 XML (xmeml) or a Premiere .prproj"
        ),
    }
}

fn fps_of(rate: Option<&Node>) -> Result<(u32, u32)> {
    let rate = rate.context("missing <rate>")?;
    let timebase = rate.number(&["timebase"]).context("missing <timebase>")? as u32;
    ensure!(
        (1..=240).contains(&timebase),
        "unsupported timebase {timebase}"
    );
    let ntsc = rate
        .text_of(&["ntsc"])
        .is_some_and(|t| t.eq_ignore_ascii_case("TRUE"));
    Ok(if ntsc {
        (timebase * 1000, 1001)
    } else {
        (timebase, 1)
    })
}

fn file_path(url: &str) -> Result<PathBuf> {
    let normalized = url.replacen("file://localhost/", "file:///", 1);
    if let Ok(parsed) = url::Url::parse(&normalized)
        && let Ok(path) = parsed.to_file_path()
    {
        return Ok(path);
    }
    // Some writers emit plain paths.
    let path = PathBuf::from(url);
    ensure!(path.is_absolute(), "unsupported media URL {url}");
    Ok(path)
}

struct Effects {
    speed: f64,
    opacity: Option<(f64, Vec<(f64, f64)>)>,
    level: Option<(f64, Vec<(f64, f64)>)>,
    other: Vec<String>,
}

fn effects(item: &Node) -> Effects {
    let mut result = Effects {
        speed: 1.0,
        opacity: None,
        level: None,
        other: Vec::new(),
    };
    for effect in item.children("filter").filter_map(|f| f.child("effect")) {
        let id = effect.text_of(&["effectid"]).unwrap_or("").to_owned();
        let parameter = |name: &str| {
            effect
                .children("parameter")
                .find(|p| p.text_of(&["parameterid"]) == Some(name))
        };
        let keyframes = |p: &Node| -> Vec<(f64, f64)> {
            p.children("keyframe")
                .filter_map(|k| Some((k.number(&["when"])?, k.number(&["value"])?)))
                .collect()
        };
        match id.as_str() {
            "timeremap" => {
                if let Some(speed) = parameter("speed").and_then(|p| p.number(&["value"])) {
                    result.speed = speed / 100.0;
                }
            }
            "opacity" => {
                if let Some(p) = parameter("opacity") {
                    result.opacity =
                        Some((p.number(&["value"]).unwrap_or(100.0) / 100.0, keyframes(p)));
                }
            }
            "audiolevels" => {
                if let Some(p) = parameter("level") {
                    result.level = Some((p.number(&["value"]).unwrap_or(1.0), keyframes(p)));
                }
            }
            "" => {}
            other => result.other.push(other.to_owned()),
        }
    }
    result
}

fn xmeml(root: &Node) -> Result<(Value, Vec<String>)> {
    let mut notes = Vec::new();
    let sequence = root
        .child("sequence")
        .or_else(|| {
            let mut all = Vec::new();
            root.descendants(&mut all);
            all.into_iter().find(|n| n.name == "sequence")
        })
        .context("no <sequence> in the XML")?;
    let (num, den) = fps_of(sequence.child("rate"))?;
    let fps = num as f64 / den as f64;
    let media = sequence.child("media").context("sequence has no <media>")?;
    let format = media.path(&["video", "format", "samplecharacteristics"]);
    let width = format.and_then(|f| f.number(&["width"])).unwrap_or(1920.0) as u32;
    let height = format.and_then(|f| f.number(&["height"])).unwrap_or(1080.0) as u32;

    // File definitions may appear once and be referenced by id later.
    let mut all = Vec::new();
    root.descendants(&mut all);
    let mut files = HashMap::<String, PathBuf>::new();
    for file in all.iter().filter(|n| n.name == "file") {
        if let (Some(id), Some(url)) = (file.attr("id"), file.text_of(&["pathurl"])) {
            files.insert(id.to_owned(), file_path(url)?);
        }
    }
    let item_file = |item: &Node| -> Option<PathBuf> {
        let file = item.child("file")?;
        files.get(file.attr("id")?).cloned()
    };

    // Audio items linked to a video item become that clip's own sound.
    let video_ids: Vec<String> = media
        .path(&["video"])
        .into_iter()
        .flat_map(|v| v.children("track"))
        .flat_map(|t| t.children("clipitem"))
        .filter_map(|c| c.attr("id").map(str::to_owned))
        .collect();
    let mut linked_sound = HashMap::<String, &Node>::new();
    let mut audio_items = Vec::new();
    for (track_index, track) in media
        .path(&["audio"])
        .into_iter()
        .flat_map(|a| a.children("track"))
        .enumerate()
    {
        for item in track.children("clipitem") {
            let linked_video = item
                .children("link")
                .filter_map(|l| l.text_of(&["linkclipref"]))
                .find(|id| video_ids.iter().any(|v| v == id));
            match linked_video {
                Some(video) => {
                    linked_sound.entry(video.to_owned()).or_insert(item);
                }
                None => audio_items.push((track_index, item)),
            }
        }
    }

    let seconds = |frames: f64| frames / fps;
    let mut skipped = 0usize;
    let mut video_tracks = Vec::new();
    for track in media
        .path(&["video"])
        .into_iter()
        .flat_map(|v| v.children("track"))
    {
        let mut clips = Vec::new();
        for item in &track.children {
            if item.name == "generatoritem" || item.name == "transitionitem" {
                skipped += 1;
                continue;
            }
            if item.name != "clipitem" {
                continue;
            }
            if item
                .text_of(&["enabled"])
                .is_some_and(|e| e.eq_ignore_ascii_case("FALSE"))
            {
                continue;
            }
            let Some(source) = item_file(item) else {
                skipped += 1;
                continue;
            };
            let (Some(start), Some(end), Some(in_frame)) = (
                item.number(&["start"]),
                item.number(&["end"]),
                item.number(&["in"]),
            ) else {
                skipped += 1;
                continue;
            };
            let (start, end) = if start < 0.0 || end < 0.0 {
                // -1 marks an edge owned by a transition; fall back to in/out.
                notes.push(format!(
                    "{}: a transition edge was approximated",
                    source.display()
                ));
                let out = item.number(&["out"]).unwrap_or(in_frame);
                if start >= 0.0 {
                    (start, start + out - in_frame)
                } else {
                    (end - (out - in_frame), end)
                }
            } else {
                (start, end)
            };
            let fx = effects(item);
            for other in &fx.other {
                notes.push(format!(
                    "{}: effect {other} is not represented",
                    source.display()
                ));
            }
            let mut clip = json!({
                "start": seconds(start),
                "source": source,
                "source_in": seconds(in_frame) * fx.speed,
                "duration": seconds(end - start),
            });
            if (fx.speed - 1.0).abs() > 1e-9 {
                clip["speed"] = json!(fx.speed);
            }
            if let Some((opacity, keyframes)) = &fx.opacity {
                apply_opacity(&mut clip, *opacity, keyframes, in_frame, end - start, fps);
            }
            let id = item.attr("id").unwrap_or_default();
            let mix = match linked_sound.get(id) {
                Some(sound) => mix_of(
                    &effects(sound),
                    sound.number(&["in"]).unwrap_or(in_frame),
                    end - start,
                    fps,
                ),
                None => Some(json!({"volume": 0})),
            };
            if let Some(mix) = mix {
                clip["audio"] = mix;
            }
            clips.push(clip);
        }
        video_tracks.push(json!({"clips": clips}));
    }

    let mut audio_tracks: Vec<Vec<Value>> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut track_map = HashMap::<usize, usize>::new();
    for (track_index, item) in audio_items {
        if item
            .text_of(&["enabled"])
            .is_some_and(|e| e.eq_ignore_ascii_case("FALSE"))
        {
            continue;
        }
        let Some(source) = item_file(item) else {
            skipped += 1;
            continue;
        };
        let (Some(start), Some(end), Some(in_frame)) = (
            item.number(&["start"]),
            item.number(&["end"]),
            item.number(&["in"]),
        ) else {
            skipped += 1;
            continue;
        };
        // Stereo pairs arrive as two linked mono items with identical timing.
        if !seen.insert((source.clone(), start as i64, in_frame as i64)) {
            continue;
        }
        let fx = effects(item);
        let mut clip = json!({
            "start": seconds(start),
            "source": source,
            "source_in": seconds(in_frame) * fx.speed,
            "duration": seconds(end - start),
        });
        if (fx.speed - 1.0).abs() > 1e-9 {
            clip["speed"] = json!(fx.speed);
        }
        if let Some(Value::Object(mix)) = mix_of(&fx, in_frame, end - start, fps) {
            for (key, value) in mix {
                clip[key] = value;
            }
        }
        let index = *track_map.entry(track_index).or_insert_with(|| {
            audio_tracks.push(Vec::new());
            audio_tracks.len() - 1
        });
        audio_tracks[index].push(clip);
    }
    if skipped > 0 {
        notes.push(format!(
            "{skipped} items (generators, transitions, or clips without media) were not imported"
        ));
    }
    let chapters: Vec<Value> = sequence
        .children("marker")
        .filter_map(|m| {
            Some(json!({"start": seconds(m.number(&["in"])?), "title": m.text_of(&["name"]).filter(|n| !n.is_empty()).unwrap_or("Marker")}))
        })
        .collect();
    let mut edit = json!({
        "version": crate::timeline::EDIT_VERSION,
        "name": sequence.text_of(&["name"]).filter(|n| !n.is_empty()).unwrap_or("Imported sequence"),
        "width": width,
        "height": height,
        "fps": {"num": num, "den": den},
        "video": video_tracks,
        "audio": audio_tracks.into_iter().map(|clips| json!({"clips": clips})).collect::<Vec<_>>(),
    });
    if let Some(duration) = sequence.number(&["duration"]).filter(|d| *d > 0.0) {
        edit["duration"] = json!(seconds(duration));
    }
    if !chapters.is_empty() {
        edit["chapters"] = json!(chapters);
    }
    Ok((edit, notes))
}

fn apply_opacity(
    clip: &mut Value,
    opacity: f64,
    keyframes: &[(f64, f64)],
    in_frame: f64,
    length: f64,
    fps: f64,
) {
    if keyframes.is_empty() {
        if opacity < 1.0 {
            clip["opacity"] = json!(opacity.clamp(0.0, 1.0));
        }
        return;
    }
    // Recognise the fade shapes Skywrite writes: 0 -> level at the head,
    // level -> 0 at the tail. Other shapes keep their peak opacity.
    let points: Vec<(f64, f64)> = keyframes
        .iter()
        .map(|(when, value)| ((when - in_frame) / fps, value / 100.0))
        .collect();
    let peak = points
        .iter()
        .map(|p| p.1)
        .fold(0.0, f64::max)
        .clamp(0.0, 1.0);
    if peak < 1.0 {
        clip["opacity"] = json!(peak);
    }
    if let [first, second, ..] = points.as_slice()
        && first.1 <= 0.001
        && first.0 <= 0.001
        && second.1 > 0.0
    {
        clip["fade_in"] = json!(second.0);
    }
    if let [.., before, last] = points.as_slice()
        && last.1 <= 0.001
        && (last.0 - length / fps).abs() <= 1.0 / fps + 1e-9
        && before.1 > 0.0
    {
        clip["fade_out"] = json!(last.0 - before.0);
    }
}

fn mix_of(fx: &Effects, in_frame: f64, length: f64, fps: f64) -> Option<Value> {
    let (level, keyframes) = fx.level.clone()?;
    let mut mix = Map::new();
    if keyframes.is_empty() {
        if (level - 1.0).abs() > 1e-9 {
            mix.insert("volume".into(), json!(level.clamp(0.0, 4.0)));
        }
    } else {
        let mut points: Vec<Value> = Vec::new();
        let mut last = f64::NEG_INFINITY;
        for (when, value) in keyframes {
            let time = ((when - in_frame) / fps).clamp(0.0, length / fps);
            if time > last {
                points.push(json!({"time": time, "volume": value.clamp(0.0, 4.0)}));
                last = time;
            }
        }
        mix.insert("volume_keyframes".into(), json!(points));
    }
    (!mix.is_empty()).then_some(Value::Object(mix))
}

/// Experimental read-only Premiere project import. Objects are linked by
/// ObjectID/ObjectRef and ObjectUID/ObjectURef; times are ticks.
fn prproj(root: &Node) -> Result<(Value, Vec<String>)> {
    let mut notes = vec!["Experimental: .prproj is undocumented and changes between Premiere versions. Only clip placement, in points and media paths are read; effects, speed and levels are not. Prefer File > Export > Final Cut Pro XML, and report mismatches with skywrite nit.".to_owned()];
    let mut all = Vec::new();
    root.descendants(&mut all);
    let mut by_id = HashMap::<String, &Node>::new();
    for node in &all {
        for key in ["ObjectID", "ObjectUID"] {
            if let Some(id) = node.attr(key) {
                by_id.insert(id.to_owned(), node);
            }
        }
    }
    let resolve = |node: &Node| -> Option<&Node> {
        node.attr("ObjectRef")
            .or_else(|| node.attr("ObjectURef"))
            .and_then(|id| by_id.get(id).copied())
    };
    // Breadth-first search for a named element, following object references.
    let find = |start: &Node, target: &str, limit: usize| -> Option<String> {
        let mut queue = std::collections::VecDeque::from([(start, 0usize)]);
        let mut visited = 0usize;
        while let Some((node, depth)) = queue.pop_front() {
            visited += 1;
            if visited > 20_000 {
                return None;
            }
            if node.name == target && !node.text.trim().is_empty() {
                return Some(node.text.trim().to_owned());
            }
            if depth >= limit {
                continue;
            }
            for child in &node.children {
                queue.push_back((child, depth + 1));
                if let Some(target) = resolve(child) {
                    queue.push_back((target, depth + 1));
                }
            }
        }
        None
    };
    let mut video_tracks = Vec::new();
    let mut audio_tracks = Vec::new();
    for track in all.iter().filter(|n| {
        (n.name == "VideoClipTrack" || n.name == "AudioClipTrack") && n.attr("ObjectID").is_some()
    }) {
        let video = track.name == "VideoClipTrack";
        let mut items = Vec::new();
        track.descendants(&mut items);
        let mut clips = Vec::new();
        for reference in items.iter().filter(|n| n.name == "TrackItem") {
            let Some(item) = resolve(reference) else {
                continue;
            };
            let ticks = |name: &str| find(item, name, 3).and_then(|t| t.parse::<f64>().ok());
            let start = ticks("Start").unwrap_or(0.0);
            let Some(end) = ticks("End") else { continue };
            let in_point = find(item, "InPoint", 6)
                .and_then(|t| t.parse::<f64>().ok())
                .unwrap_or(0.0);
            let Some(path) = find(item, "ActualMediaFilePath", 12) else {
                continue;
            };
            let mut clip = json!({"start": start / PRPROJ_TICKS, "source": path,
                "source_in": in_point / PRPROJ_TICKS, "duration": (end - start) / PRPROJ_TICKS});
            if video {
                clip["audio"] = json!({"volume": 0});
            }
            clips.push(clip);
        }
        if clips.is_empty() {
            continue;
        }
        if video {
            video_tracks.push(json!({"clips": clips}));
        } else {
            audio_tracks.push(json!({"clips": clips}));
        }
    }
    ensure!(
        !video_tracks.is_empty() || !audio_tracks.is_empty(),
        "no clip tracks were recognised in this project; export Final Cut Pro XML from Premiere instead"
    );
    notes.push("Linked camera sound is imported on audio tracks; video clips are muted to avoid doubling it. Frame rate and size default to 30 fps 1920x1080: check them.".into());
    Ok((
        json!({"version": crate::timeline::EDIT_VERSION, "name": "Imported Premiere project", "width": 1920, "height": 1080,
            "fps": {"num": 30, "den": 1}, "video": video_tracks, "audio": audio_tracks}),
        notes,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn xml_entities_and_paths() {
        let node = parse_xml("<a x=\"1 &amp; 2\"><b>caf&#233; &lt;ok&gt;</b><c/></a>").unwrap();
        assert_eq!(node.attr("x"), Some("1 & 2"));
        assert_eq!(node.text_of(&["b"]), Some("café <ok>"));
        assert!(node.child("c").is_some());
        assert_eq!(
            file_path("file://localhost/tmp/a%20b.mp4").unwrap(),
            PathBuf::from("/tmp/a b.mp4")
        );
        assert!(parse_xml("<a><b></a>").is_err());
    }
}
