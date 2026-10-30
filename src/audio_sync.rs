//! Audio-only coarse-to-fine synchronization. FFmpeg decodes; bounded FFT aligns.
use crate::sync::{
    Fit, Pair, SyncOptions, SyncResult, choose_fit, fit_models, fit_pairs, result_from_fit,
};
use anyhow::{Context, Result, ensure};
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;
const SAMPLE_RATE: usize = 2000;
const FEATURE_RATE: usize = 20;
const MAX_WINDOWS: usize = 24;
const MAX_FFT: usize = 1 << 20;
#[derive(Clone, Copy, Default)]
struct Complex {
    re: f64,
    im: f64,
}
impl Complex {
    fn mul(self, rhs: Self) -> Self {
        Self {
            re: self.re * rhs.re - self.im * rhs.im,
            im: self.re * rhs.im + self.im * rhs.re,
        }
    }
}
/// Iterative radix-2 transform; all callers bound sizes and use powers of two.
fn fft(values: &mut [Complex], inverse: bool) {
    let n = values.len();
    debug_assert!(n.is_power_of_two() && n <= MAX_FFT);
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            values.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let angle = (if inverse { 2.0 } else { -2.0 }) * std::f64::consts::PI / len as f64;
        let unit = Complex {
            re: angle.cos(),
            im: angle.sin(),
        };
        for start in (0..n).step_by(len) {
            let mut w = Complex { re: 1.0, im: 0.0 };
            for i in 0..len / 2 {
                let a = values[start + i];
                let b = values[start + i + len / 2].mul(w);
                values[start + i] = Complex {
                    re: a.re + b.re,
                    im: a.im + b.im,
                };
                values[start + i + len / 2] = Complex {
                    re: a.re - b.re,
                    im: a.im - b.im,
                };
                w = w.mul(unit);
            }
        }
        len *= 2;
    }
    if inverse {
        for value in values {
            value.re /= n as f64;
            value.im /= n as f64;
        }
    }
}
fn decode(path: &Path, options: &SyncOptions) -> Result<Vec<f64>> {
    let path = crate::media::canonical_file(path)?;
    let info = crate::media::probe(&path)?;
    ensure!(info.has_audio(), "{} has no audio stream", path.display());
    // Preserve audio delay relative to container origin, including nonzero/negative PTS.
    let args: Vec<OsString> = [
        "-nostdin",
        "-v",
        "error",
        "-threads",
        "1",
        "-filter_threads",
        "1",
        "-copyts",
        "-start_at_zero",
        "-protocol_whitelist",
        "file,pipe",
        "-i",
    ]
    .into_iter()
    .map(Into::into)
    .chain([path.into_os_string()])
    .chain([
        "-map".into(),
        "0:a:0".into(),
        "-vn".into(),
        "-af".into(),
        format!("aresample={SAMPLE_RATE}:async=0:first_pts=0").into(),
        "-ac".into(),
        "1".into(),
        "-t".into(),
        options.max_audio_seconds.to_string().into(),
        "-f".into(),
        "f32le".into(),
        "pipe:1".into(),
    ])
    .collect();
    let max_bytes = (options.max_audio_seconds * SAMPLE_RATE as f64 * 4.0).ceil() as usize + 65536;
    let output = crate::process::run("ffmpeg", &args, Duration::from_secs(180), max_bytes)
        .context("decoding audio for synchronization")?;
    ensure!(
        output.stdout.len() % 4 == 0,
        "FFmpeg returned malformed PCM audio"
    );
    let samples: Vec<f64> = output
        .stdout
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()) as f64)
        .collect();
    ensure!(
        samples.len() >= SAMPLE_RATE * 6,
        "audio synchronization needs at least 6 seconds of decoded audio"
    );
    ensure!(
        samples.iter().all(|x| x.is_finite()),
        "decoded audio contains non-finite samples"
    );
    let energy = samples.iter().map(|x| x * x).sum::<f64>() / samples.len() as f64;
    ensure!(
        energy > 1e-12,
        "audio is silent or too quiet to synchronize"
    );
    Ok(samples)
}
fn features(samples: &[f64]) -> Vec<f64> {
    let block = SAMPLE_RATE / FEATURE_RATE;
    let energy: Vec<f64> = samples
        .chunks_exact(block)
        .map(|s| ((s.iter().map(|x| x * x).sum::<f64>() / block as f64).sqrt() + 1e-6).ln())
        .collect();
    let mut prefix = vec![0.0; energy.len() + 1];
    for (i, x) in energy.iter().enumerate() {
        prefix[i + 1] = prefix[i] + x;
    }
    energy
        .iter()
        .enumerate()
        .map(|(i, x)| {
            let a = i.saturating_sub(FEATURE_RATE / 2);
            let b = (i + FEATURE_RATE / 2 + 1).min(energy.len());
            x - (prefix[b] - prefix[a]) / (b - a) as f64
        })
        .collect()
}
struct Correlator {
    reference: Vec<f64>,
    spectrum: Vec<Complex>,
    prefix: Vec<f64>,
    squares: Vec<f64>,
    fft_size: usize,
}
impl Correlator {
    fn new(reference: &[f64], max_query: usize) -> Result<Self> {
        let fft_size = (reference.len() + max_query).next_power_of_two();
        ensure!(
            fft_size <= MAX_FFT,
            "audio correlation exceeds FFT budget; reduce --max-audio-seconds"
        );
        let mut spectrum = vec![Complex::default(); fft_size];
        for (t, &v) in spectrum.iter_mut().zip(reference) {
            t.re = v;
        }
        fft(&mut spectrum, false);
        let mut prefix = vec![0.0; reference.len() + 1];
        let mut squares = prefix.clone();
        for (i, &x) in reference.iter().enumerate() {
            prefix[i + 1] = prefix[i] + x;
            squares[i + 1] = squares[i] + x * x;
        }
        Ok(Self {
            reference: reference.to_vec(),
            spectrum,
            prefix,
            squares,
            fft_size,
        })
    }
    fn peaks(
        &self,
        query: &[f64],
        count: usize,
        separation: usize,
        absolute: bool,
    ) -> Vec<(usize, f64)> {
        if query.is_empty() || query.len() > self.reference.len() {
            return Vec::new();
        }
        let mean = query.iter().sum::<f64>() / query.len() as f64;
        let energy = query.iter().map(|x| (x - mean).powi(2)).sum::<f64>();
        if energy <= 1e-12 {
            return Vec::new();
        }
        let mut spectrum = vec![Complex::default(); self.fft_size];
        for (i, &x) in query.iter().rev().enumerate() {
            spectrum[i].re = x - mean;
        }
        fft(&mut spectrum, false);
        for (v, &r) in spectrum.iter_mut().zip(&self.spectrum) {
            *v = v.mul(r);
        }
        fft(&mut spectrum, true);
        let mut scores = Vec::new();
        for start in 0..=self.reference.len() - query.len() {
            let sum = self.prefix[start + query.len()] - self.prefix[start];
            let variance = self.squares[start + query.len()]
                - self.squares[start]
                - sum * sum / query.len() as f64;
            if variance <= 1e-12 {
                continue;
            }
            let score = spectrum[start + query.len() - 1].re / (energy * variance).sqrt();
            scores.push((
                start,
                (if absolute { score.abs() } else { score }).clamp(-1.0, 1.0),
            ));
        }
        scores.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let mut selected: Vec<(usize, f64)> = Vec::new();
        for p in scores {
            if selected
                .iter()
                .all(|s| s.0.abs_diff(p.0) >= separation.max(1))
            {
                selected.push(p);
                if selected.len() == count {
                    break;
                }
            }
        }
        selected
    }
}
fn coarse(reference: &[f64], asset: &[f64], options: &SyncOptions) -> Result<(Vec<Fit>, usize)> {
    let rf = features(reference);
    let af = features(asset);
    let query_len = (FEATURE_RATE * 8)
        .min(af.len() * 45 / 100)
        .max(FEATURE_RATE * 2);
    ensure!(
        query_len < af.len() && query_len <= rf.len(),
        "recordings too short to synchronize"
    );
    let correlator = Correlator::new(&rf, query_len)?;
    let available = af.len() - query_len;
    let windows = MAX_WINDOWS.min(available + 1);
    let mut pairs = Vec::new();
    for i in 0..windows {
        let start = i * available / (windows - 1).max(1);
        let query = &af[start..start + query_len];
        for (position, score) in correlator.peaks(query, 3, query_len / 2, false) {
            if score < 0.35 {
                continue;
            }
            pairs.push(Pair {
                asset: (start as f64 + query_len as f64 / 2.0) / FEATURE_RATE as f64,
                reference: (position as f64 + query_len as f64 / 2.0) / FEATURE_RATE as f64,
                asset_index: i,
                reference_index: position,
                weight: score.powi(2),
            });
        }
    }
    let mut opts = options.clone();
    opts.tolerance_seconds = options.tolerance_seconds.max(0.15);
    let mut models = fit_models(&pairs, &opts)
        .context("no reliable shared audio; try transcripts or increase the analyzed interval")?;
    models.retain(|fit| fit.weight / fit.pairs.len() as f64 > 0.12);
    ensure!(
        !models.is_empty(),
        "shared audio matches are too weak to synchronize"
    );
    Ok((models, pairs.len()))
}
fn refine(
    reference: &[f64],
    asset: &[f64],
    coarse: &Fit,
    options: &SyncOptions,
) -> Result<Option<Fit>> {
    let mut pairs = Vec::new();
    for (i, pair) in coarse.pairs.iter().enumerate() {
        let center = pair.asset * SAMPLE_RATE as f64;
        let half = SAMPLE_RATE as f64;
        if center < half || center + half >= asset.len() as f64 {
            continue;
        }
        let query_len = (half * 2.0 * coarse.scale).round() as usize;
        let mut query = Vec::with_capacity(query_len);
        for j in 0..query_len {
            let position = center - half + j as f64 / coarse.scale;
            let at = position.floor() as usize;
            let fraction = position - at as f64;
            query.push(asset[at] * (1.0 - fraction) + asset[at + 1] * fraction);
        }
        let predicted = (coarse.offset + coarse.scale * pair.asset) * SAMPLE_RATE as f64;
        let search_start = (predicted - query_len as f64 / 2.0 - SAMPLE_RATE as f64 * 0.30)
            .floor()
            .max(0.0) as usize;
        let search_end = (predicted + query_len as f64 / 2.0 + SAMPLE_RATE as f64 * 0.30)
            .ceil()
            .min(reference.len() as f64) as usize;
        if search_start >= search_end || search_end - search_start < query.len() {
            continue;
        }
        let correlator = Correlator::new(&reference[search_start..search_end], query.len())?;
        if let Some((position, score)) = correlator.peaks(&query, 1, 1, true).first() {
            if *score < 0.15 {
                continue;
            }
            pairs.push(Pair {
                asset: pair.asset,
                reference: (search_start as f64 + *position as f64 + query_len as f64 / 2.0)
                    / SAMPLE_RATE as f64,
                asset_index: i,
                reference_index: search_start + position,
                weight: score.powi(2),
            });
        }
    }
    if pairs.len() < options.min_anchors {
        return Ok(None);
    }
    let mut opts = options.clone();
    opts.tolerance_seconds = options.tolerance_seconds.min(0.04);
    Ok(fit_pairs(&pairs, &opts).ok())
}
pub fn align_audio(reference: &Path, asset: &Path, options: &SyncOptions) -> Result<SyncResult> {
    options.validate()?;
    let rp = crate::media::canonical_file(reference)?;
    let ap = crate::media::canonical_file(asset)?;
    let rs = decode(&rp, options)?;
    let asset_samples = decode(&ap, options)?;
    // Search shorter inside longer; inverse maps are converted back before reporting.
    let swapped = asset_samples.len() > rs.len();
    let (r, a) = if swapped {
        (&asset_samples, &rs)
    } else {
        (&rs, &asset_samples)
    };
    let (models, count) = coarse(r, a, options)?;
    let mut refined = Vec::new();
    for model in &models {
        if let Some(mut fit) = refine(r, a, model, options)? {
            for _ in 0..3 {
                if let Some(next) = refine(r, a, &fit, options)? {
                    fit = next;
                } else {
                    break;
                }
            }
            refined.push(fit);
        }
    }
    refined.sort_by(|a, b| {
        b.weight
            .total_cmp(&a.weight)
            .then_with(|| b.pairs.len().cmp(&a.pairs.len()))
    });
    // Coarse models can converge onto the same waveform mapping.
    let mut unique: Vec<Fit> = Vec::new();
    for fit in refined {
        if unique.iter().all(|other| {
            (other.offset - fit.offset).abs() > 0.10 || (other.scale - fit.scale).abs() > 0.0005
        }) {
            unique.push(fit);
        }
    }
    let precise = !unique.is_empty();
    let mut fit = if precise {
        choose_fit(&unique)?
    } else {
        choose_fit(&models)?
    };
    if swapped {
        fit.offset = -fit.offset / fit.scale;
        fit.scale = fit.scale.recip();
        for p in &mut fit.pairs {
            std::mem::swap(&mut p.asset, &mut p.reference);
            std::mem::swap(&mut p.asset_index, &mut p.reference_index);
        }
        fit.pairs.sort_by(|a, b| a.asset.total_cmp(&b.asset));
    }
    ensure!(
        (fit.scale - 1.0).abs() <= options.max_scale_deviation + 1e-12,
        "estimated asset-to-reference scale exceeds --max-scale-deviation"
    );
    let mut result = result_from_fit(
        fit,
        count,
        options,
        &rp,
        &ap,
        if precise {
            "audio_waveform"
        } else {
            "audio_envelope"
        },
        if precise {
            1.0 / SAMPLE_RATE as f64
        } else {
            1.0 / FEATURE_RATE as f64
        },
    )?;
    if !precise {
        result.quality = "limited".into();
        result.warnings.push("only energy-envelope alignment succeeded; waveform validation did not establish sub-frame accuracy".into());
    }
    let ri = crate::media::probe(&rp)?;
    let ai = crate::media::probe(&ap)?;
    if ri.duration > options.max_audio_seconds || ai.duration > options.max_audio_seconds {
        result.warnings.push(format!("analyzed only the first {} seconds of each recording; mapping is evidenced only over the reported matched interval",options.max_audio_seconds));
    }
    result.warnings.push("audio alignment includes microphone propagation delay; internal camera audio/video latency is not measured".into());
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fft_convolution_matches_direct_dot_products() {
        let r: Vec<_> = (0..73)
            .map(|i| ((i * i + 13 * i + 1) as f64).sin())
            .collect();
        let q: Vec<_> = (0..17).map(|i| ((i * 7 + 3) as f64).cos()).collect();
        let c = Correlator::new(&r, q.len()).unwrap();
        let observed = c.peaks(&q, r.len(), 1, false);
        let mean = q.iter().sum::<f64>() / q.len() as f64;
        assert_eq!(observed.len(), r.len() - q.len() + 1);
        for (start, score) in observed {
            let window = &r[start..start + q.len()];
            let wm = window.iter().sum::<f64>() / q.len() as f64;
            let numerator = window
                .iter()
                .zip(&q)
                .map(|(a, b)| (a - wm) * (b - mean))
                .sum::<f64>();
            let denominator = (window.iter().map(|a| (a - wm).powi(2)).sum::<f64>()
                * q.iter().map(|b| (b - mean).powi(2)).sum::<f64>())
            .sqrt();
            assert!(
                (score - numerator / denominator).abs() < 1e-9,
                "{start}: {score}"
            );
        }
    }
    #[test]
    fn constant_audio_has_no_correlation_peak() {
        let c = Correlator::new(&[1.0; 100], 20).unwrap();
        assert!(c.peaks(&[1.0; 20], 3, 1, false).is_empty());
    }
}
#[cfg(test)]
mod integration_tests {
    use super::*;
    use std::io::Write;
    fn hash(mut x: u64) -> f64 {
        x = x.wrapping_add(0x9e3779b97f4a7c15);
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
        x ^= x >> 31;
        ((x >> 32) as u32) as f64 / u32::MAX as f64
    }
    fn signal(t: f64) -> f64 {
        let cell = (t * 2.7).floor();
        let phase = t * 2.7 - cell;
        let amplitude = 0.03 + 0.30 * hash(cell as u64 + 117);
        let on = 0.03 + hash(cell as u64 + 313) * 0.2;
        let off = 0.55 + hash(cell as u64 + 719) * 0.4;
        let gate = if phase < on {
            phase / on
        } else if phase > off {
            (1.0 - phase) / (1.0 - off)
        } else {
            1.0
        };
        amplitude
            * gate
            * ((std::f64::consts::TAU * (117.0 * t + 0.3 * t * t)).sin()
                + 0.4 * (std::f64::consts::TAU * (247.0 * t + 0.13 * t * t)).sin())
    }
    fn wav(path: &Path, seconds: f64, rate: u32, offset: f64, scale: f64, silent: bool) {
        let count = (seconds * rate as f64) as u32;
        let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        f.write_all(b"RIFF").unwrap();
        f.write_all(&(36 + count * 2).to_le_bytes()).unwrap();
        f.write_all(b"WAVEfmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(&rate.to_le_bytes()).unwrap();
        f.write_all(&(rate * 2).to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&(count * 2).to_le_bytes()).unwrap();
        for i in 0..count {
            let sample = if silent {
                0
            } else {
                (signal(offset + scale * i as f64 / rate as f64) * i16::MAX as f64) as i16
            };
            f.write_all(&sample.to_le_bytes()).unwrap();
        }
    }
    #[test]
    fn audio_sync_ffmpeg_offset_and_one_percent_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let reference = tmp.path().join("main.wav");
        let asset = tmp.path().join("camera.wav");
        wav(&reference, 80.0, 8000, 0.0, 1.0, false);
        wav(&asset, 60.0, 8000, 7.123, 1.01, false);
        let result = align_audio(&reference, &asset, &SyncOptions::default()).unwrap();
        eprintln!(
            "audio result offset {} scale {} p95 {}",
            result.offset_seconds, result.scale, result.p95_error_seconds
        );
        assert!((result.offset_seconds - 7.123).abs() < 0.01, "{result:?}");
        assert!((result.scale - 1.01).abs() < 0.0002, "{result:?}");
        assert_eq!(result.method, "audio_waveform");
        let inverse = align_audio(&asset, &reference, &SyncOptions::default()).unwrap();
        assert!((inverse.offset_seconds + 7.123 / 1.01).abs() < 0.01);
        assert!((inverse.scale - 1.0 / 1.01).abs() < 0.0002);
    }
    #[test]
    fn audio_sync_different_sample_rates_are_not_clock_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let reference = tmp.path().join("main.wav");
        let asset = tmp.path().join("camera.wav");
        wav(&reference, 65.0, 48000, 0.0, 1.0, false);
        wav(&asset, 50.0, 44100, 4.375, 1.0001, false);
        let result = align_audio(&reference, &asset, &SyncOptions::default()).unwrap();
        assert!((result.offset_seconds - 4.375).abs() < 0.01, "{result:?}");
        assert!((result.scale - 1.0001).abs() < 0.00005, "{result:?}");
    }
    #[test]
    fn audio_sync_rejects_silence_and_unrelated_recordings() {
        let tmp = tempfile::tempdir().unwrap();
        let reference = tmp.path().join("main.wav");
        let asset = tmp.path().join("camera.wav");
        wav(&reference, 40.0, 8000, 0.0, 1.0, false);
        wav(&asset, 30.0, 8000, 0.0, 1.0, true);
        assert!(align_audio(&reference, &asset, &SyncOptions::default()).is_err());
        wav(&asset, 30.0, 8000, 137.0, 1.0, false);
        assert!(align_audio(&reference, &asset, &SyncOptions::default()).is_err());
    }
    #[test]
    fn audio_sync_preserves_delayed_audio_and_nonzero_container_pts() {
        let tmp = tempfile::tempdir().unwrap();
        let reference = tmp.path().join("main.wav");
        let asset = tmp.path().join("camera.wav");
        let delayed = tmp.path().join("delayed.mkv");
        wav(&reference, 80.0, 8000, 0.0, 1.0, false);
        wav(&asset, 60.0, 8000, 7.123, 1.001, false);
        let mut args: Vec<OsString> = [
            "-nostdin",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=64x64:r=10:d=65",
            "-itsoffset",
            "1.25",
            "-i",
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        args.push(asset.into_os_string());
        args.extend(
            [
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-c:v",
                "mpeg4",
                "-c:a",
                "pcm_s16le",
                "-output_ts_offset",
                "4.0",
            ]
            .into_iter()
            .map(Into::into),
        );
        args.push(delayed.clone().into_os_string());
        crate::process::run("ffmpeg", &args, Duration::from_secs(30), 1024 * 1024).unwrap();
        let info = crate::media::probe(&delayed).unwrap();
        assert!(info.start_time > 3.9, "{info:?}");
        let result = align_audio(&reference, &delayed, &SyncOptions::default()).unwrap();
        let expected = 7.123 - 1.25 * 1.001;
        assert!(
            (result.offset_seconds - expected).abs() < 0.02,
            "expected {expected}, {result:?}"
        );
        assert!((result.scale - 1.001).abs() < 0.0002, "{result:?}");
    }
}
