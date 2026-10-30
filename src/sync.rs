//! Robust bounded transcript alignment. Mapping is always asset -> reference.
use crate::transcript::Transcript;
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MAX_WORDS: usize = 100_000;
const MAX_CANDIDATES: usize = 40_000;
const MAX_OCCURRENCES: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyncOptions {
    pub max_scale_deviation: f64,
    pub tolerance_seconds: f64,
    pub min_anchors: usize,
    pub min_span_seconds: f64,
    /// Analyze at most this many seconds from each file in audio-only mode.
    pub max_audio_seconds: f64,
}
impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            max_scale_deviation: 0.03,
            tolerance_seconds: 0.15,
            min_anchors: 8,
            min_span_seconds: 10.0,
            max_audio_seconds: 3600.0,
        }
    }
}
impl SyncOptions {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.max_scale_deviation.is_finite()
                && (0.0..=0.10).contains(&self.max_scale_deviation),
            "max_scale_deviation must be finite and between 0 and 0.10"
        );
        ensure!(
            self.tolerance_seconds.is_finite() && (0.001..=2.0).contains(&self.tolerance_seconds),
            "tolerance_seconds must be finite and between 0.001 and 2"
        );
        ensure!(
            (3..=1000).contains(&self.min_anchors),
            "min_anchors must be between 3 and 1000"
        );
        ensure!(
            self.min_span_seconds.is_finite() && (1.0..=600.0).contains(&self.min_span_seconds),
            "min_span_seconds must be finite and between 1 and 600"
        );
        ensure!(
            self.max_audio_seconds.is_finite()
                && (10.0..=14400.0).contains(&self.max_audio_seconds),
            "max_audio_seconds must be finite and between 10 and 14400"
        );
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncAnchor {
    pub asset_time: f64,
    pub reference_time: f64,
    pub error_seconds: f64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResult {
    pub version: u32,
    pub method: String,
    pub reference: PathBuf,
    pub asset: PathBuf,
    pub mapping: String,
    pub offset_seconds: f64,
    /// Duration multiplier; playback-rate multiplier is its reciprocal.
    pub scale: f64,
    pub playback_rate: f64,
    pub drift_ppm: f64,
    pub drift_estimated: bool,
    /// Evidence grade, not a calibrated probability.
    pub quality: String,
    pub candidate_matches: usize,
    pub inliers: usize,
    pub inlier_ratio: f64,
    pub anchor_span_seconds: f64,
    pub asset_interval: [f64; 2],
    pub reference_interval: [f64; 2],
    pub median_error_seconds: f64,
    pub p95_error_seconds: f64,
    /// Residual-based diagnostic, not a statistical confidence interval.
    pub scale_uncertainty_ppm: Option<f64>,
    pub timing_resolution_seconds: f64,
    pub warnings: Vec<String>,
    pub anchors: Vec<SyncAnchor>,
}
#[derive(Debug, Clone)]
pub(crate) struct Pair {
    pub asset: f64,
    pub reference: f64,
    pub asset_index: usize,
    pub reference_index: usize,
    pub weight: f64,
}
#[derive(Debug, Clone)]
pub(crate) struct Fit {
    pub offset: f64,
    pub scale: f64,
    pub pairs: Vec<Pair>,
    pub weight: f64,
}
fn normalize(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}
fn eligible_single(word: &str) -> bool {
    word.chars().count() >= 4
        && ![
            "that", "this", "with", "from", "have", "there", "they", "their", "them", "were",
            "what", "when", "where", "will", "would", "could", "should", "about", "just", "like",
            "your", "some", "then", "than", "into", "been", "also", "very", "because", "know",
        ]
        .contains(&word)
}
fn validate_transcript(transcript: &Transcript) -> Result<()> {
    ensure!(
        transcript.version == 1,
        "unsupported transcript version {}; expected 1",
        transcript.version
    );
    ensure!(
        transcript.duration.is_finite() && transcript.duration > 0.0,
        "transcript duration must be finite and positive"
    );
    ensure!(
        transcript.words.len() <= MAX_WORDS,
        "transcript exceeds the {MAX_WORDS}-word sync limit"
    );
    ensure!(
        transcript
            .words
            .iter()
            .map(|word| word.text.len())
            .sum::<usize>()
            <= 8 * 1024 * 1024,
        "transcript word text exceeds the 8 MiB sync limit"
    );
    let mut previous = 0.0;
    for word in &transcript.words {
        ensure!(
            word.start.is_finite()
                && word.end.is_finite()
                && word.start >= previous
                && word.end >= word.start
                && word.end <= transcript.duration + 0.5,
            "word timestamps must be finite, chronological and inside the recording"
        );
        ensure!(
            word.text.len() <= 4096,
            "word text exceeds the 4096-byte sync limit"
        );
        if let Some(confidence) = word.confidence {
            ensure!(
                confidence.is_finite() && (0.0..=1.0).contains(&confidence),
                "word confidence must be between 0 and 1"
            );
        }
        previous = word.start;
    }
    Ok(())
}
fn key(tokens: &[String], index: usize, count: usize) -> Option<String> {
    if index + count > tokens.len()
        || tokens[index..index + count].iter().any(String::is_empty)
        || (count == 1 && !eligible_single(&tokens[index]))
    {
        return None;
    }
    Some(tokens[index..index + count].join("\u{1f}"))
}
fn candidates(reference: &Transcript, asset: &Transcript) -> Result<Vec<Pair>> {
    let rt: Vec<_> = reference.words.iter().map(|w| normalize(&w.text)).collect();
    let at: Vec<_> = asset.words.iter().map(|w| normalize(&w.text)).collect();
    let mut index: HashMap<String, Vec<usize>> = HashMap::new();
    for i in 0..rt.len() {
        for count in [3, 2, 1] {
            if let Some(k) = key(&rt, i, count) {
                let occurrences = index.entry(k).or_default();
                if occurrences.len() <= MAX_OCCURRENCES {
                    occurrences.push(i);
                }
            }
        }
    }
    let mut found: Vec<Pair> = Vec::new();
    let mut stride = 1;
    for (i, word) in asset.words.iter().enumerate() {
        if i % stride != 0 || word.confidence.is_some_and(|c| c < 0.25) {
            continue;
        }
        let mut local: HashMap<usize, Pair> = HashMap::new();
        for count in [3, 2, 1] {
            if let Some(k) = key(&at, i, count)
                && let Some(occurrences) = index.get(&k)
                && occurrences.len() <= MAX_OCCURRENCES
            {
                for &j in occurrences {
                    if reference.words[j].confidence.is_some_and(|c| c < 0.25) {
                        continue;
                    }
                    let weight = count as f64 / occurrences.len() as f64;
                    let pair = Pair {
                        asset: word.start,
                        reference: reference.words[j].start,
                        asset_index: i,
                        reference_index: j,
                        weight,
                    };
                    local
                        .entry(j)
                        .and_modify(|p| p.weight = p.weight.max(weight))
                        .or_insert(pair);
                }
            }
        }
        // Thin by source-word index *before* growing storage: at most 40k
        // candidates plus 24 local alternatives, regardless of repetition.
        while found.len() + local.len() > MAX_CANDIDATES {
            stride *= 2;
            found.retain(|pair| pair.asset_index % stride == 0);
        }
        if i % stride == 0 {
            found.extend(local.into_values());
        }
    }
    found.sort_by_key(|p| (p.asset_index, p.reference_index));
    ensure!(
        !found.is_empty(),
        "no shared distinctive words or phrases; recordings may not overlap"
    );
    Ok(found)
}
fn inliers(pairs: &[Pair], offset: f64, scale: f64, tolerance: f64) -> Vec<Pair> {
    let mut selected = Vec::new();
    let mut last_reference = None;
    let mut at = 0;
    while at < pairs.len() {
        let index = pairs[at].asset_index;
        let mut best: Option<(&Pair, f64)> = None;
        while at < pairs.len() && pairs[at].asset_index == index {
            let p = &pairs[at];
            let error = (p.reference - offset - scale * p.asset).abs();
            if error <= tolerance
                && last_reference.is_none_or(|previous| p.reference_index > previous)
                && best.is_none_or(|(_, previous_error)| error < previous_error)
            {
                best = Some((p, error));
            }
            at += 1;
        }
        if let Some((p, _)) = best {
            last_reference = Some(p.reference_index);
            selected.push(p.clone());
        }
    }
    selected
}
pub(crate) fn least_squares(pairs: &[Pair]) -> Option<(f64, f64)> {
    let weight: f64 = pairs.iter().map(|p| p.weight).sum();
    if weight <= 0.0 {
        return None;
    }
    let x = pairs.iter().map(|p| p.weight * p.asset).sum::<f64>() / weight;
    let y = pairs.iter().map(|p| p.weight * p.reference).sum::<f64>() / weight;
    let denominator = pairs
        .iter()
        .map(|p| p.weight * (p.asset - x).powi(2))
        .sum::<f64>();
    if denominator <= 1e-12 {
        return None;
    }
    let slope = pairs
        .iter()
        .map(|p| p.weight * (p.asset - x) * (p.reference - y))
        .sum::<f64>()
        / denominator;
    Some((y - slope * x, slope))
}
fn equivalent(a: &Fit, b: &Fit, min: f64, max: f64, tolerance: f64) -> bool {
    [min, max]
        .into_iter()
        .all(|t| ((a.offset + a.scale * t) - (b.offset + b.scale * t)).abs() < tolerance * 4.0)
}
fn consider(pairs: &[Pair], offset: f64, scale: f64, options: &SyncOptions, best: &mut Vec<Fit>) {
    if !offset.is_finite()
        || !scale.is_finite()
        || (scale - 1.0).abs() > options.max_scale_deviation + 1e-12
    {
        return;
    }
    let mut accepted = inliers(pairs, offset, scale, options.tolerance_seconds);
    if accepted.len() < options.min_anchors {
        return;
    }
    let mut offset = offset;
    let mut scale = scale;
    for _ in 0..2 {
        if let Some((next_offset, next_scale)) = least_squares(&accepted)
            && (next_scale - 1.0).abs() <= options.max_scale_deviation + 1e-12
        {
            offset = next_offset;
            scale = next_scale;
            accepted = inliers(pairs, offset, scale, options.tolerance_seconds);
        }
    }
    if accepted.len() < options.min_anchors {
        return;
    }
    let candidate = Fit {
        offset,
        scale,
        weight: accepted
            .iter()
            .map(|p| {
                let error =
                    (p.reference - offset - scale * p.asset).abs() / options.tolerance_seconds;
                p.weight * (1.0 - error * error).max(0.0)
            })
            .sum(),
        pairs: accepted,
    };
    let min = candidate.pairs.first().unwrap().asset;
    let max = candidate.pairs.last().unwrap().asset;
    let ordering = |a: &Fit, b: &Fit| {
        b.weight
            .total_cmp(&a.weight)
            .then_with(|| b.pairs.len().cmp(&a.pairs.len()))
    };
    if let Some(i) = best
        .iter()
        .position(|p| equivalent(p, &candidate, min, max, options.tolerance_seconds))
    {
        if ordering(&candidate, &best[i]).is_lt() {
            best[i] = candidate;
        }
    } else {
        best.push(candidate);
    }
    best.sort_by(ordering);
    best.truncate(6);
}
pub(crate) fn fit_models(pairs: &[Pair], options: &SyncOptions) -> Result<Vec<Fit>> {
    options.validate()?;
    ensure!(
        pairs.len() >= options.min_anchors,
        "too few matching anchors"
    );
    let mut pairs = pairs.to_vec();
    pairs.sort_by_key(|p| (p.asset_index, p.reference_index));
    let mut models = Vec::new();
    let mut offsets: HashMap<i64, (usize, f64)> = HashMap::new();
    for p in &pairs {
        let delta = p.reference - p.asset;
        let e = offsets.entry((delta / 0.5).round() as i64).or_default();
        e.0 += 1;
        e.1 += delta;
    }
    let mut offsets: Vec<_> = offsets.into_values().collect();
    offsets.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.total_cmp(&b.1)));
    for (count, total) in offsets.into_iter().take(24) {
        consider(&pairs, total / count as f64, 1.0, options, &mut models);
    }
    let mut random: u64 = 0x4d595df4d0f33173;
    let mut next = || {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        random as usize % pairs.len()
    };
    for trial in 0..1024 {
        // A nearly complete one-to-one consensus needs no further random search.
        // Duplicate candidate passages lower this ratio and retain the full budget.
        if trial >= 64
            && models
                .first()
                .is_some_and(|fit| fit.pairs.len() as f64 >= pairs.len() as f64 * 0.90)
        {
            break;
        }
        let a = &pairs[next()];
        let b = &pairs[next()];
        if (a.asset - b.asset).abs() < 2.0 {
            continue;
        }
        let scale = (a.reference - b.reference) / (a.asset - b.asset);
        consider(
            &pairs,
            a.reference - scale * a.asset,
            scale,
            options,
            &mut models,
        );
    }
    ensure!(
        !models.is_empty(),
        "no consistent offset/scale; recordings may not overlap or may contain discontinuities"
    );
    // Polish only the short hypothesis list, not every RANSAC trial. Median/MAD
    // trimming prevents a minority of biased word boundaries or waveform cycles
    // from pulling an otherwise well-supported least-squares slope.
    for fit in &mut models {
        for _ in 0..3 {
            let mut residuals: Vec<_> = fit
                .pairs
                .iter()
                .map(|p| p.reference - fit.offset - fit.scale * p.asset)
                .collect();
            residuals.sort_by(f64::total_cmp);
            let median = residuals[residuals.len() / 2];
            let mut deviations: Vec<_> = residuals.iter().map(|r| (r - median).abs()).collect();
            deviations.sort_by(f64::total_cmp);
            let tolerance = (deviations[deviations.len() / 2] * 4.5)
                .max(options.tolerance_seconds * 0.08)
                .min(options.tolerance_seconds);
            let kept: Vec<_> = fit
                .pairs
                .iter()
                .filter(|p| {
                    (p.reference - fit.offset - fit.scale * p.asset - median).abs() <= tolerance
                })
                .cloned()
                .collect();
            if kept.len() < options.min_anchors {
                break;
            }
            if let Some((offset, scale)) = least_squares(&kept)
                && (scale - 1.0).abs() <= options.max_scale_deviation + 1e-12
            {
                fit.offset = offset;
                fit.scale = scale;
                fit.pairs = kept;
            }
        }
        fit.weight = fit
            .pairs
            .iter()
            .map(|p| {
                let error = (p.reference - fit.offset - fit.scale * p.asset).abs()
                    / options.tolerance_seconds;
                p.weight * (1.0 - error * error).max(0.0)
            })
            .sum();
    }
    models.sort_by(|a, b| {
        b.weight
            .total_cmp(&a.weight)
            .then_with(|| b.pairs.len().cmp(&a.pairs.len()))
    });
    let mut unique: Vec<Fit> = Vec::new();
    for model in models {
        let min = model.pairs.first().unwrap().asset;
        let max = model.pairs.last().unwrap().asset;
        if unique
            .iter()
            .all(|p| !equivalent(p, &model, min, max, options.tolerance_seconds))
        {
            unique.push(model);
        }
    }
    Ok(unique)
}
pub(crate) fn choose_fit(models: &[Fit]) -> Result<Fit> {
    let Some(best) = models.first() else {
        bail!(
            "no consistent offset/scale; recordings may not overlap or may contain discontinuities"
        );
    };
    if let Some(second) = models.get(1)
        && second.pairs.len() as f64 >= best.pairs.len() as f64 * 0.85
        && second.weight >= best.weight * 0.70
    {
        bail!("ambiguous synchronization: multiple passages support different time mappings");
    }
    Ok(best.clone())
}
pub(crate) fn fit_pairs(pairs: &[Pair], options: &SyncOptions) -> Result<Fit> {
    choose_fit(&fit_models(pairs, options)?)
}
pub(crate) fn result_from_fit(
    fit: Fit,
    candidate_count: usize,
    options: &SyncOptions,
    reference: &Path,
    asset: &Path,
    method: &str,
    resolution: f64,
) -> Result<SyncResult> {
    ensure!(!fit.pairs.is_empty(), "no accepted sync anchors");
    let first = fit.pairs.first().unwrap();
    let last = fit.pairs.last().unwrap();
    let span = last.asset - first.asset;
    let drift_estimated =
        span >= options.min_span_seconds && fit.pairs.len() >= options.min_anchors;
    let mut warnings = Vec::new();
    let (offset, scale) = if drift_estimated {
        (fit.offset, fit.scale)
    } else {
        warnings.push("overlap too short to estimate drift; offset-only mapping is valid near the matched interval".into());
        let mut offsets: Vec<_> = fit.pairs.iter().map(|p| p.reference - p.asset).collect();
        offsets.sort_by(f64::total_cmp);
        (offsets[offsets.len() / 2], 1.0)
    };
    let mut residuals: Vec<_> = fit
        .pairs
        .iter()
        .map(|p| (p.reference - offset - scale * p.asset).abs())
        .collect();
    residuals.sort_by(f64::total_cmp);
    let median = residuals[residuals.len() / 2];
    let p95 = residuals[((residuals.len() - 1) as f64 * 0.95).ceil() as usize];
    let uncertainty =
        drift_estimated.then_some(p95.max(resolution) * 2.0 / span.max(1.0) * 1_000_000.0);
    let quality =
        if drift_estimated && fit.pairs.len() >= 16 && p95 < options.tolerance_seconds * 0.75 {
            "strong"
        } else {
            "limited"
        };
    let step = fit.pairs.len().div_ceil(64);
    let anchors = fit
        .pairs
        .iter()
        .step_by(step)
        .map(|p| SyncAnchor {
            asset_time: p.asset,
            reference_time: p.reference,
            error_seconds: p.reference - offset - scale * p.asset,
        })
        .collect();
    Ok(SyncResult {
        version: 1,
        method: method.into(),
        reference: reference.into(),
        asset: asset.into(),
        mapping: "reference_time = offset_seconds + scale * asset_time".into(),
        offset_seconds: offset,
        scale,
        playback_rate: scale.recip(),
        drift_ppm: (scale - 1.0) * 1_000_000.0,
        drift_estimated,
        quality: quality.into(),
        candidate_matches: candidate_count,
        inliers: fit.pairs.len(),
        inlier_ratio: fit.pairs.len() as f64 / candidate_count.max(1) as f64,
        anchor_span_seconds: span,
        asset_interval: [first.asset, last.asset],
        reference_interval: [first.reference, last.reference],
        median_error_seconds: median,
        p95_error_seconds: p95,
        scale_uncertainty_ppm: uncertainty,
        timing_resolution_seconds: resolution,
        warnings,
        anchors,
    })
}
pub fn align_transcripts(
    reference: &Transcript,
    asset: &Transcript,
    options: &SyncOptions,
) -> Result<SyncResult> {
    options.validate()?;
    validate_transcript(reference)?;
    validate_transcript(asset)?;
    let pairs = candidates(reference, asset)?;
    let fit = fit_pairs(&pairs, options)?;
    let fraction =
        fit.pairs.len() as f64 / reference.words.len().min(asset.words.len()).max(1) as f64;
    ensure!(
        fraction >= 0.08,
        "too little shared speech for a reliable sync ({:.1}% of the shorter transcript)",
        fraction * 100.0
    );
    let mut result = result_from_fit(
        fit,
        pairs.len(),
        options,
        &reference.source,
        &asset.source,
        "transcript",
        0.02,
    )?;
    result.warnings.push("word timestamps are ASR estimates; validate against audio before frame-accurate conforming".into());
    Ok(result)
}
pub fn align_audio(reference: &Path, asset: &Path, options: &SyncOptions) -> Result<SyncResult> {
    crate::audio_sync::align_audio(reference, asset, options)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Word;
    fn transcript(offset: f64, scale: f64, count: usize) -> Transcript {
        let words: Vec<_> = (0..count)
            .map(|i| Word {
                text: format!("distinctive{i}"),
                start: offset + scale * i as f64 * 0.7,
                end: offset + scale * (i as f64 * 0.7 + 0.3),
                confidence: Some(0.95),
            })
            .collect();
        Transcript {
            version: 1,
            source: PathBuf::from("test.wav"),
            duration: offset + scale * (count as f64 * 0.7 + 1.0),
            text: words
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            words,
            model: None,
            usage: None,
        }
    }
    #[test]
    fn transcript_sync_estimates_offset_drift_and_reciprocal_playback() {
        let reference = transcript(12.345, 1.01, 200);
        let asset = transcript(0.0, 1.0, 200);
        let result = align_transcripts(&reference, &asset, &SyncOptions::default()).unwrap();
        assert!((result.offset_seconds - 12.345).abs() < 1e-6);
        assert!((result.scale - 1.01).abs() < 1e-9);
        assert!((result.playback_rate - 1.0 / 1.01).abs() < 1e-9);
        assert!(result.drift_estimated);
    }
    #[test]
    fn transcript_sync_ignores_asr_edits_and_timestamp_outliers() {
        let reference = transcript(5.25, 1.0001, 1000);
        let mut asset = transcript(0.0, 1.0, 1000);
        for i in (0..asset.words.len()).step_by(11) {
            asset.words[i].text = "incorrect transcription".into();
        }
        for i in (3..asset.words.len()).step_by(17) {
            asset.words[i].start += 0.25;
            asset.words[i].end += 0.25;
        }
        asset.words = asset
            .words
            .into_iter()
            .enumerate()
            .filter(|(i, _)| i % 13 != 0)
            .map(|(_, w)| w)
            .collect();
        let mut with_insertions = Vec::new();
        for (i, mut word) in asset.words.into_iter().enumerate() {
            let jitter = 0.01 * (i as f64).sin();
            word.start += jitter;
            word.end += jitter;
            let insertion = Word {
                text: "hallucinated-insertion".into(),
                start: word.end + 0.02,
                end: word.end + 0.04,
                confidence: Some(0.9),
            };
            with_insertions.push(word);
            if i % 29 == 0 {
                with_insertions.push(insertion);
            }
        }
        asset.words = with_insertions;
        let result = align_transcripts(&reference, &asset, &SyncOptions::default()).unwrap();
        assert!((result.offset_seconds - 5.25).abs() < 0.01, "{result:?}");
        assert!((result.scale - 1.0001).abs() < 0.00001);
        assert!(result.inliers > 700);
    }
    #[test]
    fn transcript_sync_negative_offset_partial_overlap() {
        let mut reference = transcript(0.0, 1.0, 200);
        let mut asset = transcript(20.0, 1.0, 200);
        reference.words.drain(0..30);
        asset.words.truncate(170);
        let result = align_transcripts(&reference, &asset, &SyncOptions::default()).unwrap();
        assert!((result.offset_seconds + 20.0).abs() < 1e-6);
        assert!(result.inliers >= 140);
    }
    #[test]
    fn transcript_sync_rejects_two_equally_plausible_takes() {
        let mut reference = transcript(0.0, 1.0, 100);
        reference.words.extend(transcript(100.0, 1.0, 100).words);
        reference.duration = 180.0;
        let asset = transcript(0.0, 1.0, 100);
        let error = align_transcripts(&reference, &asset, &SyncOptions::default()).unwrap_err();
        assert!(error.to_string().contains("ambiguous"), "{error}");
    }
    #[test]
    fn transcript_sync_rejects_empty_unrelated_repetitive() {
        let reference = transcript(0.0, 1.0, 100);
        assert!(
            align_transcripts(
                &reference,
                &transcript(0.0, 1.0, 0),
                &SyncOptions::default()
            )
            .is_err()
        );
        let mut unrelated = transcript(0.0, 1.0, 100);
        for w in &mut unrelated.words {
            w.text.push_str("other");
        }
        assert!(align_transcripts(&reference, &unrelated, &SyncOptions::default()).is_err());
        for w in &mut unrelated.words {
            w.text = "the".into();
        }
        let mut repeated = reference;
        for w in &mut repeated.words {
            w.text = "the".into();
        }
        assert!(align_transcripts(&repeated, &unrelated, &SyncOptions::default()).is_err());
    }
    #[test]
    fn transcript_sync_reports_short_overlap_drift_unmeasurable() {
        let result = align_transcripts(
            &transcript(2.0, 1.0, 10),
            &transcript(0.0, 1.0, 10),
            &SyncOptions::default(),
        )
        .unwrap();
        assert!(!result.drift_estimated);
        assert!(result.scale_uncertainty_ppm.is_none());
        assert_eq!(result.scale, 1.0);
    }
    #[test]
    fn transcript_sync_rejects_invalid_timestamps_options() {
        let reference = transcript(0.0, 1.0, 100);
        let mut asset = transcript(0.0, 1.0, 100);
        asset.words[3].start = f64::NAN;
        assert!(align_transcripts(&reference, &asset, &SyncOptions::default()).is_err());
        assert!(
            SyncOptions {
                max_scale_deviation: f64::INFINITY,
                ..SyncOptions::default()
            }
            .validate()
            .is_err()
        );
    }
    #[test]
    fn transcript_sync_realistic_hour_is_bounded() {
        let reference = transcript(31.0, 0.9999, 6000);
        let asset = transcript(0.0, 1.0, 6000);
        let started = std::time::Instant::now();
        let result = align_transcripts(&reference, &asset, &SyncOptions::default()).unwrap();
        assert!((result.scale - 0.9999).abs() < 1e-8);
        assert!(result.candidate_matches <= MAX_CANDIDATES);
        eprintln!("6000-word sync {:?}", started.elapsed());
    }
    #[test]
    fn transcript_sync_caps_candidate_storage_and_preserves_temporal_span() {
        let asset = transcript(0.0, 1.0, 10_000);
        let mut reference = transcript(0.0, 1.0, 10_000);
        for copy in 1..6 {
            reference
                .words
                .extend(transcript(copy as f64 * 7010.0, 1.0, 10_000).words);
        }
        reference.duration = 42_100.0;
        let pairs = candidates(&reference, &asset).unwrap();
        assert!(pairs.len() <= MAX_CANDIDATES);
        assert!(pairs.first().unwrap().asset < 1.0);
        assert!(pairs.last().unwrap().asset > asset.duration * 0.99);
    }
    #[test]
    fn transcript_sync_caps_total_word_text_before_indexing() {
        let reference = transcript(0.0, 1.0, 10);
        let mut asset = transcript(0.0, 1.0, 2050);
        for word in &mut asset.words {
            word.text = "x".repeat(4096);
        }
        let error = align_transcripts(&reference, &asset, &SyncOptions::default()).unwrap_err();
        assert!(error.to_string().contains("8 MiB"));
    }
    #[test]
    fn transcript_sync_rejects_discontinuous_or_out_of_bounds_clock() {
        let asset = transcript(0.0, 1.0, 200);
        let mut reference = transcript(5.0, 1.0, 200);
        for word in reference.words.iter_mut().skip(100) {
            word.start += 4.0;
            word.end += 4.0;
        }
        reference.duration += 4.0;
        assert!(align_transcripts(&reference, &asset, &SyncOptions::default()).is_err());
        assert!(
            align_transcripts(&transcript(0.0, 1.2, 200), &asset, &SyncOptions::default()).is_err()
        );
    }
    #[test]
    fn normalization_keeps_non_ascii_letters() {
        assert_eq!(normalize("HÉLLO!"), "héllo");
        assert_eq!(normalize("世界。"), "世界");
    }
}
