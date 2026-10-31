//! Optional HTML/React composition projects. Original media are copied, never linked.
use super::number;
use crate::{
    captions, media,
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
    for (index, track) in edit.captions.iter().enumerate() {
        for format in ["srt", "vtt"] {
            fs::write(
                staging.path().join(format!("captions-{index}.{format}")),
                captions::encode(&track.cues, format)?,
            )?;
        }
    }
    fs::write(
        staging.path().join("chapters.ffmetadata"),
        captions::chapter_metadata(&edit.chapters, edit.duration()),
    )?;
    fs::write(
        staging.path().join("captions-chapters.json"),
        serde_json::to_vec_pretty(&json!({"captions":edit.captions,"chapters":edit.chapters}))?,
    )?;
    fs::write(
        staging.path().join("edit.validated.json"),
        serde_json::to_vec_pretty(edit)?,
    )?;
    publish_project(staging.path(), output)?;
    let mut warnings = edit.warnings.clone();
    if !edit.captions.is_empty() || !edit.chapters.is_empty() {
        warnings.push("Captions and chapters are preserved in sidecars. After rendering, run skywrite captions mux VIDEO --edit ORIGINAL_EDIT --output FINISHED.mp4 to apply soft tracks, explicit burn-in, and chapters.".into());
    }
    Ok(
        json!({"target":target,"output":output,"duration_seconds":edit.duration(),"copied_media_bytes":total_bytes,"warnings":warnings,"next_command":if target=="hyperframes" {"hyperframes check <project-directory>"}else{"npm install && npm run render (inside project directory)"}}),
    )
}

/// Publish the completed tree with exclusive directory renaming where available.
/// Other platforms retain exclusive reservation with rollback on failed moves.
fn publish_project(staging: &Path, output: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let source = CString::new(staging.as_os_str().as_bytes())?;
        let destination = CString::new(output.as_os_str().as_bytes())?;
        // NOREPLACE also protects an empty directory created during compilation.
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP)
        ) {
            return Err(error).context("publishing complete project directory");
        }
    }
    publish_reserved(staging, output)
}

fn publish_reserved(staging: &Path, output: &Path) -> Result<()> {
    fs::create_dir(output).context("reserving new project directory")?;
    let mut moved = Vec::new();
    let result = (|| -> Result<()> {
        for entry in fs::read_dir(staging)? {
            let entry = entry?;
            let destination = output.join(entry.file_name());
            fs::rename(entry.path(), &destination)?;
            moved.push(destination);
        }
        Ok(())
    })();
    if let Err(error) = result {
        // Restore only our entries and remove only an empty reservation. Never
        // recursively remove content another caller might have added.
        let mut failures = Vec::new();
        for destination in moved.into_iter().rev() {
            let source = staging.join(destination.file_name().expect("generated entry name"));
            if let Err(rollback) = fs::rename(&destination, &source) {
                failures.push(format!("{}: {rollback}", destination.display()));
            }
        }
        if let Err(rollback) = fs::remove_dir(output) {
            failures.push(format!("{}: {rollback}", output.display()));
        }
        if !failures.is_empty() {
            return Err(error).context(format!(
                "project publication rollback needs recovery: {}",
                failures.join("; ")
            ));
        }
        return Err(error).context("publishing project directory; reservation rolled back");
    }
    Ok(())
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
        "Generated by Skywrite. Uses your installed Hyperframes CLI.\n\nRun `hyperframes lint .`, `hyperframes check .`, and `hyperframes render . -o edit.mp4`.\n\nAssets are copies. Timing, source trims, playback rates, and linear gains come from the validated edit. Captions and chapters are preserved in sidecars; apply them after rendering with `skywrite captions mux VIDEO --edit ORIGINAL_EDIT --output FINISHED.mp4`. The pinned GSAP script may require network access unless your Hyperframes installation provides its vendor cache.\n",
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
        "Generated by Skywrite. Assets are copies. Install the pinned optional runtime with `npm install`, then run `npm run studio` or `npm run render`.\n\nCaptions and chapters are preserved in sidecars; apply them after rendering with `skywrite captions mux VIDEO --edit ORIGINAL_EDIT --output FINISHED.mp4`.\n\nRemotion's license applies to its runtime; see https://www.remotion.dev/license. Source trims are rounded to composition-frame boundaries. This exporter accepts integer FPS; use FCPXML or FFmpeg for NTSC rates.\n",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_publication_preserves_existing_directories_and_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        for publish in [publish_project, publish_reserved] {
            let staging = tempfile::tempdir_in(dir.path()).unwrap();
            fs::write(staging.path().join("complete.json"), b"complete").unwrap();
            let output = dir.path().join("project");
            fs::create_dir(&output).unwrap();
            fs::write(output.join("owned.txt"), b"previous").unwrap();
            assert!(publish(staging.path(), &output).is_err());
            assert_eq!(fs::read(output.join("owned.txt")).unwrap(), b"previous");
            assert!(staging.path().join("complete.json").is_file());
            fs::remove_file(output.join("owned.txt")).unwrap();
            fs::remove_dir(&output).unwrap();
            publish(staging.path(), &output).unwrap();
            assert_eq!(fs::read(output.join("complete.json")).unwrap(), b"complete");
            fs::remove_dir_all(output).unwrap();
        }
        let output = dir.path().join("failed");
        assert!(publish_reserved(&dir.path().join("missing-staging"), &output).is_err());
        assert!(!output.exists(), "failed reservation must be removed");
    }
}
