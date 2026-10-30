//! Optional HTML/React composition projects. Original media are copied, never linked.
use super::number;
use crate::{
    media,
    timeline::{MediaKind, ValidatedEdit},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

pub(super) fn export(edit: &ValidatedEdit, target: &str, output: &Path) -> Result<Value> {
    ensure!(
        !output.exists(),
        "project output directory already exists; choose a new directory (existing projects are never overwritten)"
    );
    if target == "remotion" {
        ensure!(
            edit.fps.num.is_multiple_of(edit.fps.den),
            "this Remotion exporter requires integer fps; use FCPXML or FFmpeg for rational frame rates"
        );
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure!(parent.is_dir(), "project parent directory does not exist");
    let staging = tempfile::Builder::new()
        .prefix(".skywrite-project-")
        .tempdir_in(parent)?;
    let media_dir = if target == "remotion" {
        "public/media"
    } else {
        "assets"
    };
    fs::create_dir_all(staging.path().join(media_dir))?;
    let mut sources = HashMap::<PathBuf, String>::new();
    let mut total_bytes = 0u64;
    for source in edit.source_paths() {
        if sources.contains_key(source) {
            continue;
        }
        total_bytes = total_bytes
            .checked_add(fs::metadata(source)?.len())
            .context("asset size overflow")?;
        ensure!(
            total_bytes <= 20 * 1024 * 1024 * 1024,
            "composition export is limited to 20 GiB of copied media; use FCPXML or FFmpeg for larger sources"
        );
        let ext = source
            .extension()
            .and_then(|s| s.to_str())
            .filter(|s| s.len() <= 12 && s.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or("bin");
        let name = format!("asset-{:04}.{ext}", sources.len());
        fs::copy(source, staging.path().join(media_dir).join(&name))
            .with_context(|| format!("copying {}", source.display()))?;
        sources.insert(source.to_owned(), name);
    }
    if target == "hyperframes" {
        hyperframes(edit, staging.path(), &sources)?;
    } else {
        remotion(edit, staging.path(), &sources)?;
    }
    fs::write(
        staging.path().join("edit.validated.json"),
        serde_json::to_vec_pretty(edit)?,
    )?;
    // Exclusive reservation prevents clobbering a destination created while the
    // media were being copied. Only our fresh directory receives generated files.
    fs::create_dir(output).context("reserving new project directory")?;
    for entry in fs::read_dir(staging.path())? {
        let entry = entry?;
        fs::rename(entry.path(), output.join(entry.file_name()))?;
    }
    Ok(
        json!({"target":target,"output":output,"duration_seconds":edit.duration(),"copied_media_bytes":total_bytes,"warnings":edit.warnings,"next_command":if target=="hyperframes" {"hyperframes check <project-directory>"}else{"npm install && npm run render (inside project directory)"}}),
    )
}

fn hyperframes(edit: &ValidatedEdit, dir: &Path, sources: &HashMap<PathBuf, String>) -> Result<()> {
    // Hyperframes rewrites this HTML before rendering. Keep human names only in
    // edit.validated.json: even escaped presentation text need not cross that
    // executable-document boundary. Media references use generated asset IDs.
    let mut html = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"UTF-8\"><meta name=\"viewport\" content=\"width={}, height={}\"><title>Skywrite composition</title>\n<script src=\"https://cdn.jsdelivr.net/npm/gsap@3.14.2/dist/gsap.min.js\"></script>\n<style>*{{box-sizing:border-box;margin:0}}html,body{{width:{}px;height:{}px;overflow:hidden;background:#000}}.clip{{position:absolute;inset:0;width:100%;height:100%;object-fit:contain}}#root{{position:relative;width:100%;height:100%;background:#000}}</style></head><body>\n<div id=\"root\" data-composition-id=\"main\" data-start=\"0\" data-duration=\"{}\" data-width=\"{}\" data-height=\"{}\" data-fps=\"{}\">\n",
        edit.width,
        edit.height,
        edit.width,
        edit.height,
        number(edit.duration()),
        edit.width,
        edit.height,
        edit.fps.ffmpeg()
    );
    for (index, clip) in edit.clips.iter().enumerate() {
        if clip.kind == MediaKind::Gap {
            continue;
        }
        let source = clip.source.as_ref().context("validated source missing")?;
        let common = format!(
            "id=\"shot{index}\" class=\"clip\" data-start=\"{}\" data-duration=\"{}\" data-track-index=\"0\"",
            number(edit.fps.seconds(clip.offset_frames)),
            number(edit.fps.seconds(clip.duration_frames))
        );
        let src = format!("assets/{}", sources[source]);
        if clip.kind == MediaKind::Image {
            writeln!(html, "<img {common} src=\"{src}\" alt=\"Image\">")?;
        } else {
            writeln!(
                html,
                "<video {common} src=\"{src}\" data-media-start=\"{}\" data-playback-rate=\"{}\" data-volume=\"{}\" {} playsinline preload=\"auto\"></video>",
                number(clip.source_in),
                number(clip.speed),
                number(clip.volume),
                if clip.volume == 0.0
                    || !clip.media.as_ref().is_some_and(media::MediaInfo::has_audio)
                {
                    "muted"
                } else {
                    "data-has-audio=\"true\""
                }
            )?;
        }
    }
    for (index, clip) in edit.audio.iter().enumerate() {
        writeln!(
            html,
            "<audio id=\"audio{index}\" src=\"assets/{}\" data-start=\"{}\" data-duration=\"{}\" data-media-start=\"{}\" data-playback-rate=\"{}\" data-volume=\"{}\" data-track-index=\"{}\" preload=\"auto\"></audio>",
            sources[&clip.source],
            number(edit.fps.seconds(clip.offset_frames)),
            number(edit.fps.seconds(clip.duration_frames)),
            number(clip.source_in),
            number(clip.speed),
            number(clip.volume),
            index + 1
        )?;
    }
    writeln!(
        html,
        "</div><script>window.__timelines = window.__timelines || {{}}; const tl = gsap.timeline({{paused:true}}); window.__timelines.main = tl; tl.to({{}}, {{duration:{}}}); tl.seek(0);</script></body></html>",
        number(edit.duration())
    )?;
    fs::write(dir.join("index.html"), html)?;
    fs::write(
        dir.join("hyperframes.json"),
        serde_json::to_vec_pretty(
            &json!({"$schema":"https://hyperframes.heygen.com/schema/hyperframes.json","paths":{"assets":"assets"},"media":{"autoProxy":false}}),
        )?,
    )?;
    fs::write(
        dir.join("README.md"),
        "Generated by Skywrite. Uses your installed Hyperframes CLI.\n\nRun `hyperframes lint .`, `hyperframes check .`, and `hyperframes render . -o edit.mp4`.\n\nAssets are copies. Timing, source trims, playback rates, and linear gains come from the validated edit. The pinned GSAP script may require network access unless your Hyperframes installation provides its vendor cache.\n",
    )?;
    Ok(())
}

fn remotion(edit: &ValidatedEdit, dir: &Path, sources: &HashMap<PathBuf, String>) -> Result<()> {
    let fps = edit.fps.value();
    let mut shots = Vec::new();
    for clip in &edit.clips {
        shots.push(json!({"source":clip.source.as_ref().map(|s|format!("media/{}",sources[s])),"kind":clip.kind,"from":clip.offset_frames,"duration":clip.duration_frames,"trim":(clip.source_in*fps).round() as u64,"speed":clip.speed,"volume":clip.volume}));
    }
    let mut audio = Vec::new();
    for clip in &edit.audio {
        audio.push(json!({"source":format!("media/{}",sources[&clip.source]),"from":clip.offset_frames,"duration":clip.duration_frames,"trim":(clip.source_in*fps).round() as u64,"speed":clip.speed,"volume":clip.volume}));
    }
    let data = json!({"width":edit.width,"height":edit.height,"fps":fps,"duration":edit.duration_frames,"shots":shots,"audio":audio});
    fs::write(dir.join("edit.json"), serde_json::to_vec_pretty(&data)?)?;
    // Keep Remotion packages in lockstep on a published release. Its transitive
    // Zod pin needs the same-major security fix for GHSA-mrhx-c7xc-m3w5.
    fs::write(
        dir.join("package.json"),
        serde_json::to_vec_pretty(
            &json!({"name":"skywrite-edit","private":true,"version":"1.0.0","scripts":{"studio":"remotion studio index.jsx","render":"remotion render index.jsx Skywrite edit.mp4"},"dependencies":{"@remotion/cli":"4.0.539","remotion":"4.0.539","react":"19.3.1","react-dom":"19.3.1"},"overrides":{"zod":"4.6.7"}}),
        )?,
    )?;
    fs::write(
        dir.join("index.jsx"),
        r#"import React from 'react';
import {AbsoluteFill, Composition, Img, OffthreadVideo, Html5Audio, Sequence, registerRoot, staticFile} from 'remotion';
import edit from './edit.json';

const style = {width: '100%', height: '100%', objectFit: 'contain'};
const Film = () => <AbsoluteFill style={{backgroundColor: 'black'}}>
  {edit.shots.map((shot, index) => <Sequence key={index} from={shot.from} durationInFrames={shot.duration}>
    {shot.kind === 'gap' ? <AbsoluteFill style={{backgroundColor: 'black'}}/> : shot.kind === 'image' ?
      <Img src={staticFile(shot.source)} style={style}/> :
      <OffthreadVideo src={staticFile(shot.source)} trimBefore={shot.trim} playbackRate={shot.speed} volume={shot.volume} muted={shot.volume === 0} preservePitch style={style}/>}
  </Sequence>)}
  {edit.audio.map((sound, index) => <Sequence key={index} from={sound.from} durationInFrames={sound.duration}>
    <Html5Audio src={staticFile(sound.source)} trimBefore={sound.trim} playbackRate={sound.speed} volume={sound.volume} preservePitch/>
  </Sequence>)}
</AbsoluteFill>;
const Root = () => <Composition id="Skywrite" component={Film} width={edit.width} height={edit.height} fps={edit.fps} durationInFrames={edit.duration}/>;
registerRoot(Root);
"#,
    )?;
    fs::write(
        dir.join("README.md"),
        "Generated by Skywrite. Assets are copies. Install the pinned optional runtime with `npm install`, then run `npm run studio` or `npm run render`.\n\nRemotion's license applies to its runtime; see https://www.remotion.dev/license. Source trims are rounded to composition-frame boundaries. This exporter accepts integer FPS; use FCPXML or FFmpeg for NTSC rates.\n",
    )?;
    Ok(())
}
