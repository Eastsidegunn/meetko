//! A cheap check, before a channel goes to whisper, of whether the channel is dead or steady. Whisper
//! given a channel with no speech at all (a muted mic, a constant noise floor) does not return
//! nothing: it writes subtitle credits and the like ("한글자막 by …", "…광고를 포함하고 있습니다")
//! onto it. A channel this check calls `NoSpeech` is not sent to whisper; it gets no words and its
//! provenance says why (`skipped: no_speech`).
//!
//! What is skipped, exactly: a channel whose 20 ms frame energies are all digital silence (at or below
//! -80 dBFS: muted devices, the zero-filled spans a capture writes where no audio arrived) or steady
//! around one level. Steady means both: (a) less than 100 ms in total rises 3 dB or more above the
//! floor (the 10th percentile of the frames above -80 dBFS; silent frames do not count, so zero-filled
//! spans cannot pull the floor down), and (b) the spread of those frames, 90th minus 10th percentile, is
//! under 2 dB. Steady hiss varies about 1 dB between frames by (b)'s measure; speech that adds even as
//! much energy as the noise itself (0 dB SNR) moves the frames it is in by 3 dB, and moves (b) once it
//! fills a tenth of the channel. Anything else goes to whisper: a cough, a click, typing, a fan that
//! changes speed, and a file that cannot be read. Known edge: heavily compressed speech or music held at
//! one level with no pauses could look steady, but it would also have to stay within 3 dB in every 20 ms.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};

use crate::error::{Error, Result};

pub const FRAME_MS: u32 = 20;
pub const FLOOR_PERCENTILE: usize = 10;
pub const ABOVE_FLOOR_DB: f64 = 3.0;
pub const MIN_DBFS: f64 = -80.0;
pub const MIN_ACTIVE_MS: u64 = 100;
/// 90th minus 10th percentile of the audible frames: at or above this a channel is not steady
pub const MAX_STEADY_SPREAD_DB: f64 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// not dead or steady: the engine decides
    Speech,
    /// dead (digital silence) or steady (a constant noise floor): skipped
    NoSpeech,
    /// not a WAV this check reads (the engine still gets it)
    Unread,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Speech => "speech",
            Verdict::NoSpeech => "no_speech",
            Verdict::Unread => "unread",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Check {
    pub verdict: Verdict,
    /// floor(frames * 1000 / rate), as the store and the capture manifest count it; None when unread
    pub duration_ms: Option<i64>,
    /// total length of the frames 3 dB or more above the floor
    pub active_ms: u64,
}

/// The settings of the check, as recorded in the producer's params (with the verdict).
pub fn params(verdict: Verdict) -> Value {
    json!({ "frame_ms": FRAME_MS, "floor_percentile": FLOOR_PERCENTILE, "above_floor_db": ABOVE_FLOOR_DB,
            "min_dbfs": MIN_DBFS, "min_active_ms": MIN_ACTIVE_MS, "max_steady_spread_db": MAX_STEADY_SPREAD_DB,
            "result": verdict.as_str() })
}

const UNREAD: Check = Check { verdict: Verdict::Unread, duration_ms: None, active_ms: 0 };

/// Read a mono WAV (any sample format hound reads) and judge it. What cannot be read is `Unread`; the
/// only error is `Cancelled`.
pub fn check(path: &Path, cancel: &AtomicBool) -> Result<Check> {
    let Ok(reader) = hound::WavReader::open(path) else { return Ok(UNREAD) };
    let spec = reader.spec();
    if spec.channels != 1 || spec.sample_rate == 0 {
        return Ok(UNREAD);
    }
    let frame = (spec.sample_rate as usize * FRAME_MS as usize / 1000).max(1);
    let mut energies: Vec<f64> = Vec::with_capacity(reader.duration() as usize / frame + 1);
    let (mut sum, mut n, mut total) = (0f64, 0usize, 0u64);
    let mut push = |x: f64, energies: &mut Vec<f64>| -> bool {
        sum += x * x;
        n += 1;
        total += 1;
        if n == frame {
            energies.push(sum / n as f64);
            sum = 0.0;
            n = 0;
            // about every 10 s of audio
            if energies.len().is_multiple_of(500) && cancel.load(Ordering::SeqCst) {
                return false;
            }
        }
        true
    };
    match spec.sample_format {
        hound::SampleFormat::Int => {
            let scale = 2f64.powi(spec.bits_per_sample as i32 - 1);
            for s in reader.into_samples::<i32>() {
                let Ok(s) = s else { return Ok(UNREAD) };
                if !push(s as f64 / scale, &mut energies) {
                    return Err(Error::Cancelled);
                }
            }
        }
        hound::SampleFormat::Float => {
            for s in reader.into_samples::<f32>() {
                let Ok(s) = s else { return Ok(UNREAD) };
                if !push(s as f64, &mut energies) {
                    return Err(Error::Cancelled);
                }
            }
        }
    }
    // a partial last frame counts too (a short word at the very end)
    if n > 0 {
        energies.push(sum / n as f64);
    }
    let duration_ms = (total * 1000 / spec.sample_rate as u64) as i64;
    let (verdict, active_ms) = judge(&energies);
    Ok(Check { verdict, duration_ms: Some(duration_ms), active_ms })
}

fn db(mean_square: f64) -> f64 {
    10.0 * mean_square.max(1e-20).log10()
}

/// The `p`th percentile of sorted values.
fn pct(sorted: &[f64], p: usize) -> f64 {
    sorted[(sorted.len() - 1) * p / 100]
}

/// Dead or steady (`NoSpeech`), else `Speech`; and the length of the frames above the floor.
fn judge(energies: &[f64]) -> (Verdict, u64) {
    // silent frames say nothing about the floor: zero-filled spans would pull it to -200 dB
    let mut audible: Vec<f64> = energies.iter().map(|&e| db(e)).filter(|&d| d > MIN_DBFS).collect();
    if audible.is_empty() {
        return (Verdict::NoSpeech, 0);
    }
    audible.sort_by(|a, b| a.total_cmp(b));
    let floor = pct(&audible, FLOOR_PERCENTILE);
    let spread = pct(&audible, 100 - FLOOR_PERCENTILE) - floor;
    let active = audible.iter().filter(|&&d| d >= floor + ABOVE_FLOOR_DB).count() as u64 * FRAME_MS as u64;
    let steady = active < MIN_ACTIVE_MS && spread < MAX_STEADY_SPREAD_DB;
    (if steady { Verdict::NoSpeech } else { Verdict::Speech }, active)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 16000;

    fn write(path: &Path, samples: &[f32]) {
        let spec = hound::WavSpec { channels: 1, sample_rate: RATE, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for s in samples {
            w.write_sample((s * 32767.0).round().clamp(-32768.0, 32767.0) as i16).unwrap();
        }
        w.finalize().unwrap();
    }

    /// Deterministic Gaussian-ish noise (sum of uniforms) with standard deviation `sigma`.
    fn noise(n: usize, sigma: f32, seed: &mut u64) -> Vec<f32> {
        (0..n)
            .map(|_| {
                let mut acc = 0f32;
                for _ in 0..12 {
                    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    acc += (*seed >> 40) as f32 / (1u64 << 24) as f32;
                }
                (acc - 6.0) * sigma
            })
            .collect()
    }

    /// A voiced syllable train: harmonics of 140 Hz under a 4 Hz syllable envelope, peak `amp`.
    fn voice(n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                let env = (std::f32::consts::PI * 4.0 * t).sin().abs();
                let tone: f32 = (1..=8).map(|k| (2.0 * std::f32::consts::PI * 140.0 * k as f32 * t).sin() / k as f32).sum();
                amp * env * tone / 2.0
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    fn add(x: &mut [f32], at: usize, y: &[f32]) {
        for (i, s) in y.iter().enumerate() {
            x[at + i] += s;
        }
    }

    fn run(samples: &[f32]) -> Check {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("ch.wav");
        write(&p, samples);
        check(&p, &AtomicBool::new(false)).unwrap()
    }

    const S: usize = RATE as usize;

    #[test]
    fn a_constant_noise_floor_and_digital_silence_have_no_speech() {
        let mut seed = 7;
        // the comparison's muted mic: Gaussian noise at about -66 dBFS, ten minutes
        let c = run(&noise(S * 600, 0.0005, &mut seed));
        assert_eq!(c.verdict, Verdict::NoSpeech, "{c:?}");
        assert_eq!(c.duration_ms, Some(600_000));
        assert_eq!(run(&vec![0.0; S * 30]).verdict, Verdict::NoSpeech);
        // loud steady noise is still no speech
        assert_eq!(run(&noise(S * 30, 0.05, &mut seed)).verdict, Verdict::NoSpeech);
        // an empty file
        assert_eq!(run(&[]).verdict, Verdict::NoSpeech);
    }

    #[test]
    fn zero_filled_spans_do_not_pull_the_floor_down() {
        let mut seed = 3;
        // a capture's zero-filled gap (15 %, 50 %) before a steady hiss: still dead or steady
        for zeros in [15, 50] {
            let mut x = vec![0.0; S * 60 * zeros / 100];
            x.extend(noise(S * 60 - x.len(), 0.0005, &mut seed));
            assert_eq!(run(&x).verdict, Verdict::NoSpeech, "{zeros}% zeros + hiss");
            // the same with a second of quiet speech in the hiss
            add(&mut x, S * 50, &voice(S, 0.003));
            assert_eq!(run(&x).verdict, Verdict::Speech, "{zeros}% zeros + speech");
        }
        // zero-filled gaps in the middle, several of them
        let mut x = noise(S * 60, 0.0005, &mut seed);
        for k in 0..6 {
            x[S * (5 + 9 * k)..S * (7 + 9 * k)].fill(0.0);
        }
        assert_eq!(run(&x).verdict, Verdict::NoSpeech);
    }

    #[test]
    fn quiet_speech_on_the_floor_is_kept() {
        let mut seed = 11;
        // one second of very quiet voice (peak about -50 dBFS) in five minutes of a -66 dBFS floor
        let mut x = noise(S * 300, 0.0005, &mut seed);
        add(&mut x, S * 150, &voice(S, 0.003));
        let c = run(&x);
        assert_eq!(c.verdict, Verdict::Speech, "{c:?}");
        // quiet speech over digital silence
        let mut y = vec![0.0; S * 60];
        add(&mut y, S * 10, &voice(S / 2, 0.0005));
        assert_eq!(run(&y).verdict, Verdict::Speech);
        // a meeting that is speech nearly all the time
        assert_eq!(run(&voice(S * 20, 0.3)).verdict, Verdict::Speech);
    }

    #[test]
    fn speech_at_or_below_the_noise_is_sent_to_whisper() {
        let mut seed = 5;
        let hiss = noise(S * 60, 0.01, &mut seed);
        let n = rms(&hiss);
        for snr_db in [0.0f32, -3.0] {
            // 30 s of talk (with its syllable gaps) whose RMS is `snr_db` against broadband noise
            let v = voice(S * 30, 1.0);
            let gain = n * 10f32.powf(snr_db / 20.0) / rms(&v);
            let v: Vec<f32> = v.iter().map(|s| s * gain).collect();
            let mut x = hiss.clone();
            add(&mut x, S * 15, &v);
            assert_eq!(run(&x).verdict, Verdict::Speech, "speech at {snr_db} dB SNR");
        }
    }

    #[test]
    fn the_boundaries_of_steady() {
        let mut seed = 9;
        let base = noise(S * 60, 0.0005, &mut seed);
        let tone = |ms: usize, amp: f32| (0..S * ms / 1000).map(|i| (i as f32 * 0.17).sin() * amp).collect::<Vec<_>>();
        // a 40 ms click is not enough to call it speech; 200 ms of a tone 10 dB over the hiss is
        let mut x = base.clone();
        add(&mut x, S * 30, &tone(40, 0.0022));
        assert_eq!(run(&x).verdict, Verdict::NoSpeech, "a lone click");
        let mut x = base.clone();
        add(&mut x, S * 30, &tone(200, 0.0022));
        assert_eq!(run(&x).verdict, Verdict::Speech, "200 ms above the floor");
        // a noise floor that changes level (a fan speeding up) is not steady: whisper decides
        let mut x = base.clone();
        for s in &mut x[S * 30..] {
            *s *= 1.6;
        }
        assert_eq!(run(&x).verdict, Verdict::Speech, "two noise levels");
    }

    #[test]
    fn what_cannot_be_read_goes_to_the_engine_and_a_cancel_stops_the_check() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("x.wav");
        std::fs::write(&p, b"not a wav").unwrap();
        assert_eq!(check(&p, &AtomicBool::new(false)).unwrap(), UNREAD);
        assert_eq!(check(&tmp.path().join("missing.wav"), &AtomicBool::new(false)).unwrap().verdict, Verdict::Unread);
        write(&p, &vec![0.0; S * 30]);
        assert!(matches!(check(&p, &AtomicBool::new(true)), Err(Error::Cancelled)));
    }
}
