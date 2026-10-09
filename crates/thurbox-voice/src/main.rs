//! thurbox-voice: local dictation for thurbox.
//!
//! Two halves. `start`/`stop`/`cancel`/`status` drive a background daemon that
//! records, transcribes, has an LLM fix misheard words, and pastes the result
//! into a thurbox session — what the thurbox pane calls. `test`, `cleanup` and
//! `stats` are the same pipeline in the foreground, for measuring engines and
//! cleanup backends on real prompts.

mod audio;
mod cleanup;
mod config;
mod context;
mod daemon;
mod engine;
mod models;
mod stats;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};

use config::Backend;
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
        #[command(flatten)]
        cleanup: CleanupArgs,
    },
    /// Run only the cleanup pass over some text — iterate on it without audio.
    Cleanup {
        /// The transcript to clean up.
        text: String,
        #[command(flatten)]
        cleanup: CleanupArgs,
    },
    /// Show where the config file is and which cleanup backend would be used.
    Config,
    /// Summarise compare.jsonl per engine and per cleanup backend.
    Stats,
    /// Start recording (launching the daemon if needed). Returns at once.
    Start {
        /// Session to paste the text into when recording stops.
        #[arg(long)]
        session: Option<String>,
        /// Override `[voice] engine` for this dictation.
        #[arg(long, value_enum)]
        engine: Option<EngineId>,
    },
    /// Stop recording, transcribe, clean up, and paste into the session.
    Stop,
    /// Stop recording and throw the audio away.
    Cancel,
    /// What the daemon is doing.
    Status,
    /// Stop the daemon, freeing the model's memory.
    Quit,
    /// Run the daemon in the foreground (what `start` launches).
    #[command(hide = true)]
    Daemon,
}

#[derive(Args)]
struct CleanupArgs {
    /// Skip the cleanup pass, whatever the config says.
    #[arg(long)]
    raw: bool,
    /// Override `[cleanup] backend`.
    #[arg(long, value_enum)]
    backend: Option<Backend>,
    /// The coding agent the text is for (claude, codex, …) — what `auto`
    /// follows, and which agent CLI the `agent` backend runs.
    #[arg(long)]
    agent: Option<String>,
    /// A file of context for the cleanup model: repo and branch names, the
    /// agent's last screen, a glossary. Names in it win over near-misses.
    #[arg(long)]
    context_file: Option<PathBuf>,
}

impl CleanupArgs {
    /// `None` when cleanup is off for this run.
    fn plan(&self, config: &config::Config) -> Result<Option<Plan>> {
        if self.raw || !config.cleanup.enabled {
            return Ok(None);
        }
        self.plan_always(config).map(Some)
    }

    /// The plan whether or not cleanup is on — what `cleanup` runs, since
    /// asking for it by name is asking for it.
    fn plan_always(&self, config: &config::Config) -> Result<Plan> {
        Ok(Plan {
            backend: self.backend.unwrap_or(config.cleanup.backend),
            agent: self.agent.clone(),
            context: Some(self.context(config)?),
        })
    }

    /// The same context a dictation through thurbox gets — the built-in
    /// thurbox vocabulary and the user's glossary — plus `--context-file`.
    fn context(&self, config: &config::Config) -> Result<String> {
        let mut context = context::build(&config.context.glossary, None).text;
        if let Some(path) = &self.context_file {
            let extra = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
            context.push('\n');
            context.push_str(&extra);
        }
        Ok(context)
    }
}

struct Plan {
    backend: Backend,
    agent: Option<String>,
    context: Option<String>,
}

impl Plan {
    fn run(&self, config: &config::Config, raw: &str) -> Result<cleanup::Cleaned> {
        let cleaned = cleanup::run(
            &config.cleanup,
            self.backend,
            self.agent.as_deref(),
            self.context.as_deref(),
            raw,
        )?;
        eprintln!("[cleanup] {} · {:.2}s", cleaned.backend, cleaned.secs);
        if let Some(why) = &cleaned.rejected {
            eprintln!("[cleanup] ignored ({why}) — keeping the raw transcript. Model said:");
            eprintln!("{}", cleaned.model_output);
        }
        Ok(cleaned)
    }
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
    let config = config::load()?;
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
            cleanup,
        } => {
            let plan = cleanup.plan(&config)?;
            test(
                &root,
                &config,
                plan.as_ref(),
                &engines,
                file,
                save,
                vocabulary.as_deref(),
            )
        }
        Command::Cleanup { text, cleanup } => {
            let plan = cleanup.plan_always(&config)?;
            let cleaned = plan.run(&config, &text)?;
            println!("{}", cleaned.text);
            Ok(())
        }
        Command::Config => {
            let path = config::path();
            let state = if path.exists() {
                ""
            } else {
                " (absent — defaults in use)"
            };
            println!("config: {}{state}", path.display());
            println!("data:   {}", root.display());
            println!("cleanup enabled: {}", config.cleanup.enabled);
            println!("cleanup backend: {:?}", config.cleanup.backend);
            Ok(())
        }
        Command::Stats => stats::print(&root),
        Command::Start { session, engine } => {
            let mut ask = serde_json::json!({ "cmd": "start" });
            if let Some(session) = session {
                ask["session"] = session.into();
            }
            if let Some(engine) = engine {
                ask["engine"] = engine.as_str().into();
            }
            let reply = client(&root, &ask, true, Duration::from_secs(10))?;
            println!("recording with {}", reply["engine"].as_str().unwrap_or("?"));
            Ok(())
        }
        Command::Stop => {
            let ask = serde_json::json!({ "cmd": "stop" });
            let reply = client(&root, &ask, false, Duration::from_secs(120))?;
            if reply["empty"] == true || reply["text"].as_str() == Some("") {
                println!("no speech heard");
            } else if reply["pasted"] == true {
                println!("dictated {} chars", reply["chars"]);
            } else if let Some(why) = reply["paste_error"].as_str() {
                bail!(
                    "transcribed but not pasted ({why}): {}",
                    reply["text"].as_str().unwrap_or("")
                );
            } else {
                println!("{}", reply["text"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        Command::Cancel => {
            client(
                &root,
                &serde_json::json!({ "cmd": "cancel" }),
                false,
                Duration::from_secs(5),
            )?;
            println!("cancelled");
            Ok(())
        }
        Command::Status => {
            // Not running is an answer here, not an error.
            let reply = daemon::request(
                &root,
                &serde_json::json!({ "cmd": "status" }),
                false,
                Duration::from_secs(5),
            )?;
            println!("{reply}");
            Ok(())
        }
        Command::Quit => {
            // The daemon exits mid-request, so no answer is the expected one.
            let _ = daemon::request(
                &root,
                &serde_json::json!({ "cmd": "quit" }),
                false,
                Duration::from_secs(2),
            );
            println!("stopped");
            Ok(())
        }
        Command::Daemon => daemon::serve(&root, config),
    }
}

/// One request to the daemon; a refusal becomes an error, so the exit status
/// and stderr carry it to whoever ran us.
fn client(
    root: &Path,
    ask: &serde_json::Value,
    launch: bool,
    wait: Duration,
) -> Result<serde_json::Value> {
    let reply = daemon::request(root, ask, launch, wait)?;
    if reply["ok"] != true {
        bail!(
            "{}",
            reply["error"].as_str().unwrap_or("the daemon refused")
        );
    }
    if !launch && reply["running"] == false {
        bail!("not recording — the daemon is not running");
    }
    Ok(reply)
}

fn test(
    root: &Path,
    config: &config::Config,
    plan: Option<&Plan>,
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
        let (text, raw, cleanup) = match plan {
            None => {
                println!("{text}");
                (text, None, None)
            }
            Some(plan) => {
                println!("raw:     {text}");
                // A failed cleanup is reported, not fatal: the raw text is
                // still a usable dictation.
                match plan.run(config, &text) {
                    Ok(cleaned) => {
                        println!("cleaned: {}", cleaned.text);
                        let log = stats::CleanupLog {
                            backend: cleaned.backend,
                            secs: cleaned.secs,
                            rejected: cleaned.rejected,
                        };
                        (cleaned.text, Some(text), Some(log))
                    }
                    Err(e) => {
                        eprintln!("[cleanup] failed: {e:#}");
                        (text, None, None)
                    }
                }
            }
        };
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
                raw,
                cleanup,
            },
        )?;
    }
    Ok(())
}
