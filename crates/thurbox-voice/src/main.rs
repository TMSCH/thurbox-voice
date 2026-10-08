//! thurbox-voice: local dictation for thurbox.
//!
//! This is the phase-0 spike: a plain CLI to record or read a clip and
//! transcribe it with Parakeet or Whisper, logging timings so the two can be
//! compared on real prompts. The daemon and the thurbox pane come later.

mod audio;
mod engine;
mod models;
mod stats;

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use engine::{Engine, EngineId};

#[derive(Parser)]
#[command(version, about = "Local dictation for thurbox")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the known models and whether each is installed.
    Models,
    /// Download a model (pinned revision, SHA-256 checked).
    Pull {
        #[arg(value_enum)]
        engine: EngineId,
    },
    /// Record (or read a WAV) and transcribe it, printing the text and timings.
    Test {
        /// Engine to use; repeat to run the same clip through several.
        #[arg(long = "engine", value_enum, default_value = "parakeet")]
        engines: Vec<EngineId>,
        /// Transcribe this WAV instead of recording.
        #[arg(long, conflicts_with = "save")]
        file: Option<PathBuf>,
        /// Keep the recording as a WAV, to replay through another engine.
        #[arg(long)]
        save: Option<PathBuf>,
        /// Words to bias Whisper towards, e.g. "thurbox, kubectl, Spotpay".
        #[arg(long)]
        vocabulary: Option<String>,
    },
    /// Summarise compare.jsonl per engine.
    Stats,
}

/// `$THURBOX_VOICE_HOME`, else `$XDG_DATA_HOME/thurbox-voice`, else
/// `~/.local/share/thurbox-voice` — the same root thurbox uses, on every OS.
fn data_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("THURBOX_VOICE_HOME") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("thurbox-voice");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".local/share/thurbox-voice")
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // whisper.cpp and ggml print every load step to stderr. Route them to the
    // `log` facade instead, where no logger is installed, so the only output is
    // ours.
    whisper_rs::install_logging_hooks();
    let root = data_root();
    match cli.command {
        Command::Models => {
            let models_root = root.join("models");
            for model in models::MODELS {
                let state = if model.is_installed(&models_root) {
                    "installed"
                } else {
                    "-"
                };
                println!(
                    "{:<9} {:>7}  {:<9}  {}  [{}]",
                    model.engine.as_str(),
                    models::mb(model.size()),
                    state,
                    model.description,
                    model.licence
                );
            }
            Ok(())
        }
        Command::Pull { engine } => {
            let model = models::model(engine);
            eprintln!("pulling {} ({})", engine.as_str(), models::mb(model.size()));
            models::pull(model, &root.join("models"))?;
            eprintln!("✓ {} installed", engine.as_str());
            Ok(())
        }
        Command::Test {
            engines,
            file,
            save,
            vocabulary,
        } => test(&root, &engines, file, save, vocabulary.as_deref()),
        Command::Stats => stats::print(&root),
    }
}

fn test(
    root: &Path,
    engines: &[EngineId],
    file: Option<PathBuf>,
    save: Option<PathBuf>,
    vocabulary: Option<&str>,
) -> Result<()> {
    let models_root = root.join("models");
    // Fail before recording, not after someone has spoken for a minute.
    for &id in engines {
        if !models::model(id).is_installed(&models_root) {
            bail!(
                "{} is not installed — run `thurbox-voice pull {}`",
                id.as_str(),
                id.as_str()
            );
        }
    }

    let clip = match &file {
        Some(path) => audio::read_wav(path)?,
        None => audio::record_until_enter()?,
    };
    if audio::is_blocked(&clip.samples) {
        bail!(
            "the microphone delivered pure silence — on macOS, allow your terminal app in \
             System Settings › Privacy & Security › Microphone, then restart it"
        );
    }
    if let Some(path) = &save {
        audio::write_wav(path, &clip)?;
        eprintln!("saved {}", path.display());
    }
    let clip_name = file.or(save).map(|p| p.display().to_string());

    let resampled = audio::to_16k(&clip)?;
    let Some(speech) = audio::trim_silence(&resampled) else {
        eprintln!(
            "no speech detected in {:.1}s of audio — nothing to transcribe",
            clip.seconds()
        );
        return Ok(());
    };
    let audio_s = speech.len() as f64 / audio::RATE as f64;

    for &id in engines {
        let (mut engine, load) = Engine::load(id, &models_root)?;
        let started = Instant::now();
        let text = engine.transcribe(speech, vocabulary)?;
        let infer = started.elapsed();
        eprintln!(
            "[{}] audio {:.1}s · load {:.2}s · infer {:.2}s · {:.0}× realtime",
            id.as_str(),
            audio_s,
            load.as_secs_f64(),
            infer.as_secs_f64(),
            audio_s / infer.as_secs_f64().max(1e-6)
        );
        println!("{text}");
        stats::append(
            root,
            &stats::Entry {
                at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                engine: id.as_str().to_string(),
                audio_s,
                load_s: load.as_secs_f64(),
                infer_s: infer.as_secs_f64(),
                chars: text.chars().count(),
                clip: clip_name.clone(),
                text,
            },
        )?;
    }
    Ok(())
}
