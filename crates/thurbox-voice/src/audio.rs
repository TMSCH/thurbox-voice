//! Audio in, 16 kHz mono f32 out: capture, WAV files, resampling and the
//! silence gate.

use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat};
use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Fft, FixedSync, Resampler};

/// What every engine expects.
pub const RATE: u32 = 16_000;

/// A clip at its native rate, already mixed down to mono.
pub struct Clip {
    pub samples: Vec<f32>,
    pub rate: u32,
}

impl Clip {
    pub fn seconds(&self) -> f64 {
        self.samples.len() as f64 / self.rate as f64
    }
}

/// A recording in progress. The cpal stream is not `Send` on every platform,
/// so it lives on a thread of its own and this handle only talks to it.
pub struct Recorder {
    stop: std::sync::mpsc::Sender<()>,
    thread: std::thread::JoinHandle<()>,
    buffer: Arc<Mutex<Vec<f32>>>,
    rate: u32,
    pub device: String,
    pub started: Instant,
}

impl Recorder {
    /// Open the default input device and start filling the buffer. Returns
    /// once the stream is running, or with the reason it could not start.
    pub fn start() -> Result<Self> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(u32, String)>>();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let buffer: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::clone(&buffer);
        let thread = std::thread::spawn(move || {
            let stream = match open(&shared) {
                Ok((stream, rate, name)) => {
                    let _ = ready_tx.send(Ok((rate, name)));
                    stream
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            // Held until told to stop, or until the handle is dropped.
            let _ = stop_rx.recv();
            drop(stream);
        });
        let (rate, device) = ready_rx
            .recv()
            .context("the audio thread exited before starting")??;
        Ok(Self {
            stop: stop_tx,
            thread,
            buffer,
            rate,
            device,
            started: Instant::now(),
        })
    }

    /// Stop recording and hand back everything captured.
    pub fn stop(self) -> Clip {
        let _ = self.stop.send(());
        let _ = self.thread.join();
        let samples = std::mem::take(&mut *self.buffer.lock().unwrap());
        Clip {
            samples,
            rate: self.rate,
        }
    }
}

fn open(buffer: &Arc<Mutex<Vec<f32>>>) -> Result<(cpal::Stream, u32, String)> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .context("no input device — is a microphone connected?")?;
    let config = device
        .default_input_config()
        .context("the input device reports no configuration")?;
    let channels = config.channels() as usize;
    let rate = config.sample_rate();
    let name = device
        .description()
        .map(|d| d.to_string())
        .unwrap_or_else(|_| "default input".to_string());
    buffer.lock().unwrap().reserve(rate as usize * 60);
    let err_fn = |err| eprintln!("audio stream error: {err}");
    let stream = match config.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, config.into(), channels, buffer, err_fn)?,
        SampleFormat::I16 => build::<i16>(&device, config.into(), channels, buffer, err_fn)?,
        SampleFormat::I32 => build::<i32>(&device, config.into(), channels, buffer, err_fn)?,
        SampleFormat::U16 => build::<u16>(&device, config.into(), channels, buffer, err_fn)?,
        other => bail!("unsupported input sample format {other:?}"),
    };
    stream.play().context("start the input stream")?;
    Ok((stream, rate, name))
}

/// Record from the default input device until the user presses Enter.
pub fn record_until_enter() -> Result<Clip> {
    let recorder = Recorder::start()?;
    eprintln!(
        "● recording from {} ({} Hz) — press Enter to stop",
        recorder.device, recorder.rate
    );
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    eprintln!(
        "■ stopped after {:.1}s",
        recorder.started.elapsed().as_secs_f64()
    );
    io::stderr().flush().ok();
    Ok(recorder.stop())
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    buffer: &Arc<Mutex<Vec<f32>>>,
    err_fn: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: FromSample<T>,
{
    let buffer = Arc::clone(buffer);
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &_| {
            let mut out = buffer.lock().unwrap();
            for frame in data.chunks(channels) {
                let sum: f32 = frame.iter().map(|s| s.to_sample::<f32>()).sum();
                out.push(sum / channels as f32);
            }
        },
        err_fn,
        None,
    )?;
    Ok(stream)
}

/// Read any PCM WAV, mixed down to mono.
pub fn read_wav(path: &Path) -> Result<Clip> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("open {}", path.display()))?;
    let spec = reader.spec();
    let channels = spec.channels as usize;
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|s| s as f32 / scale))
                .collect::<Result<_, _>>()?
        }
    };
    let samples = interleaved
        .chunks(channels)
        .map(|f| f.iter().sum::<f32>() / channels as f32)
        .collect();
    Ok(Clip {
        samples,
        rate: spec.sample_rate,
    })
}

/// Write 16-bit mono PCM, the format every other tool reads.
pub fn write_wav(path: &Path, clip: &Clip) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: clip.rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .with_context(|| format!("create {}", path.display()))?;
    for &s in &clip.samples {
        writer.write_sample(i16::from_sample(s.clamp(-1.0, 1.0)))?;
    }
    writer.finalize()?;
    Ok(())
}

/// Resample to [`RATE`]. A no-op when the clip is already there.
pub fn to_16k(clip: &Clip) -> Result<Vec<f32>> {
    if clip.rate == RATE || clip.samples.is_empty() {
        return Ok(clip.samples.clone());
    }
    let mut resampler =
        Fft::<f32>::new(clip.rate as usize, RATE as usize, 1024, 1, FixedSync::Input)
            .context("build resampler")?;
    let input = [clip.samples.clone()];
    let adapter = SequentialSliceOfVecs::new(&input, 1, clip.samples.len())
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let out = resampler
        .process_all(&adapter, clip.samples.len(), None)
        .context("resample")?;
    Ok(out.take_data())
}

/// True when the device delivered exact zeros, which is what macOS hands an
/// app whose terminal has been denied the microphone — not quiet, *nothing*.
pub fn is_blocked(samples: &[f32]) -> bool {
    !samples.is_empty() && samples.iter().all(|&s| s == 0.0)
}

/// 30 ms frames at 16 kHz.
const FRAME: usize = 480;
/// RMS below this is silence. About -46 dBFS: well under speech at a laptop
/// mic, well over its noise floor.
const GATE: f32 = 0.005;
/// Kept either side of the speech, so a soft first or last syllable survives.
const PAD: usize = 8 * FRAME;

/// Trim leading and trailing silence. `None` when there is no speech at all,
/// which is the case Whisper fills with "Thank you." — better to say nothing.
pub fn trim_silence(samples: &[f32]) -> Option<&[f32]> {
    let loud = |chunk: &[f32]| {
        let rms = (chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32).sqrt();
        rms > GATE
    };
    let frames: Vec<&[f32]> = samples.chunks(FRAME).collect();
    let first = frames.iter().position(|f| loud(f))?;
    let last = frames.iter().rposition(|f| loud(f))?;
    let start = (first * FRAME).saturating_sub(PAD);
    let end = ((last + 1) * FRAME + PAD).min(samples.len());
    Some(&samples[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(len: usize, amp: f32) -> Vec<f32> {
        (0..len)
            .map(|i| amp * (i as f32 * 440.0 * 2.0 * std::f32::consts::PI / RATE as f32).sin())
            .collect()
    }

    #[test]
    fn silence_is_no_speech() {
        assert!(trim_silence(&vec![0.0005; RATE as usize]).is_none());
    }

    #[test]
    fn speech_is_kept_with_padding() {
        let mut clip = vec![0.0; RATE as usize];
        clip.extend(tone(RATE as usize / 2, 0.3));
        clip.extend(vec![0.0; RATE as usize]);
        let kept = trim_silence(&clip).unwrap();
        assert!(kept.len() >= RATE as usize / 2);
        assert!(kept.len() <= RATE as usize / 2 + 2 * PAD + 2 * FRAME);
    }

    #[test]
    fn exact_zeros_mean_blocked() {
        assert!(is_blocked(&[0.0; 100]));
        assert!(!is_blocked(&[0.0, 0.0001]));
        assert!(!is_blocked(&[]));
    }

    #[test]
    fn resampling_48k_keeps_duration() {
        let clip = Clip {
            samples: tone(48_000, 0.3),
            rate: 48_000,
        };
        let out = to_16k(&clip).unwrap();
        assert!((out.len() as i64 - RATE as i64).abs() < 50, "{}", out.len());
    }
}
