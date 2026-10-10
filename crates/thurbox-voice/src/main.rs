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
    /// Show the config file, the cleanup backend, and what context is sent.
    Config,
    /// Print exactly what the cleanup provider would be given — the system
    /// prompt and the user message — without calling it.
    Prompt {
        /// The transcript to build the prompt around.
        #[arg(default_value = "(your dictation)")]
        text: String,
        /// Build the context for this thurbox session, as a dictation into it
        /// would.
        #[arg(long)]
        session: Option<String>,
    },
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
    /// Build the context for this thurbox session, as a dictation into it
    /// would — its metadata and screen, per `[context] sources`.
    #[arg(long)]
    session: Option<String>,
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
        let built = context::build(&config.context, self.session.as_deref());
        // As in a dictation: a broken instructions file is reported and the
        // built-in rules go out alone. `prompt` is where it is an error.
        let instructions = config.cleanup.user_instructions().unwrap_or_else(|e| {
            eprintln!("[cleanup] instructions skipped: {e:#}");
            None
        });
        Ok(Plan {
            backend: self.backend.unwrap_or(config.cleanup.backend),
            agent: self.agent.clone().or(built.agent),
            system: cleanup::system_prompt(instructions.as_deref()),
            context: Some(self.extend(built.text)?),
        })
    }

    /// The context a dictation through thurbox gets, plus `--context-file`.
    fn extend(&self, mut context: String) -> Result<String> {
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
    system: String,
    context: Option<String>,
}

impl Plan {
    fn run(&self, config: &config::Config, raw: &str) -> Result<cleanup::Cleaned> {
        let cleaned = cleanup::run(
            &config.cleanup,
            self.backend,
            self.agent.as_deref(),
            &self.system,
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
            print_config(&root, &config);
            Ok(())
        }
        Command::Prompt { text, session } => {
            let instructions = config.cleanup.user_instructions()?;
            let built = context::build(&config.context, session.as_deref());
            println!("── system prompt ──");
            println!("{}", cleanup::system_prompt(instructions.as_deref()));
            println!("── user message ──");
            println!("{}", cleanup::user_message(Some(&built.text), &text));
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
            let engine = reply["engine"].as_str().unwrap_or("?");
            match EngineId::parse(engine) {
                Some(id) => println!("recording with {engine} ({})", id.label()),
                None => println!("recording with {engine}"),
            }
            Ok(())
        }
        Command::Stop => {
            let ask = serde_json::json!({ "cmd": "stop" });
            let reply = client(&root, &ask, false, Duration::from_secs(120))?;
            println!("{}", stop_line(&reply)?);
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

/// `config`: where things are, and everything a dictation sends and to whom.
fn print_config(root: &Path, config: &config::Config) {
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
    let exists = |file: &str| {
        let path = config::expand(file);
        let state = if path.is_file() { "found" } else { "missing" };
        format!("{} ({state})", path.display())
    };
    match (
        &config.cleanup.instructions,
        &config.cleanup.instructions_file,
    ) {
        (None, None) => println!("cleanup instructions: built-in rules only"),
        (inline, file) => {
            if inline.is_some() {
                println!("cleanup instructions: [cleanup] instructions");
            }
            if let Some(file) = file {
                println!("cleanup instructions file: {}", exists(file));
            }
        }
    }
    let sources: Vec<&str> = config.context.sources.iter().map(|s| s.as_str()).collect();
    println!("context sources: {}", sources.join(", "));
    println!(
        "context budget: {} chars, screen {} lines",
        config.context.max_chars, config.context.screen_lines
    );
    if config.context.memory_files.is_empty() {
        println!("memory files: none");
    }
    for file in &config.context.memory_files {
        println!("memory file: {}", exists(file));
    }
    println!(
        "sent to the cleanup backend: the transcript, the rules above and the context \
         sources listed — `thurbox-voice prompt --session <id>` prints it exactly"
    );
}

/// The one line `stop` prints, which is what the strip shows: what was done,
/// with which speech model, and what became of the cleanup.
fn stop_line(reply: &serde_json::Value) -> Result<String> {
    let label = reply["engine"]
        .as_str()
        .and_then(EngineId::parse)
        .map(|id| format!(" · {}", id.label()))
        .unwrap_or_default();
    if reply["empty"] == true || reply["text"].as_str() == Some("") {
        return Ok(format!("no speech heard{label}"));
    }
    let cleanup = &reply["cleanup"];
    let why = brief(cleanup["why"].as_str().unwrap_or(""));
    let cleanup = match cleanup["state"].as_str() {
        Some("cleaned") => format!(
            " · cleanup {}",
            backend_name(cleanup["backend"].as_str().unwrap_or("?"))
        ),
        Some("refused") => format!(" · raw (cleanup refused: {why})"),
        Some("failed") => format!(" · raw (cleanup failed: {why})"),
        Some("off") => " · raw (cleanup off)".to_string(),
        _ => String::new(),
    };
    if reply["pasted"] == true {
        Ok(format!("dictated {} chars{label}{cleanup}", reply["chars"]))
    } else if let Some(why) = reply["paste_error"].as_str() {
        bail!(
            "transcribed but not pasted ({why}): {}",
            reply["text"].as_str().unwrap_or("")
        );
    } else {
        Ok(reply["text"].as_str().unwrap_or("").to_string())
    }
}

/// The backend as the one-line strip names it. An agent command is cut to its
/// program's name: its argv can be long, and can carry a key of its own.
fn backend_name(label: &str) -> String {
    match label.strip_prefix("agent:") {
        Some(argv) => {
            let program = argv.split_whitespace().next().unwrap_or("?");
            format!("agent:{}", program.rsplit('/').next().unwrap_or(program))
        }
        None => label.to_string(),
    }
}

/// A reason short enough for the strip: its first line, cut at a width that
/// leaves the session name in view. `daemon.log` keeps the whole of it.
fn brief(why: &str) -> String {
    const WIDTH: usize = 80;
    let line = why.lines().next().unwrap_or("").trim();
    if line.chars().count() <= WIDTH {
        return line.to_string();
    }
    let cut: String = line.chars().take(WIDTH).collect();
    format!("{}…", cut.trim_end())
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
                            model_output: cleaned
                                .rejected
                                .is_some()
                                .then(|| cleaned.model_output.clone()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stop_names_the_speech_model_and_the_cleanup() {
        let reply = json!({
            "ok": true, "text": "hi", "chars": 2, "pasted": true, "engine": "parakeet",
            "cleanup": { "state": "cleaned", "backend": "anthropic:claude-haiku-5-5" },
        });
        assert_eq!(
            stop_line(&reply).unwrap(),
            "dictated 2 chars · Parakeet TDT 0.6B v3 · cleanup anthropic:claude-haiku-5-5"
        );
        let reply = json!({
            "ok": true, "text": "hi", "chars": 2, "pasted": true, "engine": "whisper",
            "cleanup": { "state": "refused", "why": "length changed 2.00×" },
        });
        assert_eq!(
            stop_line(&reply).unwrap(),
            "dictated 2 chars · Whisper large-v3-turbo · raw (cleanup refused: length changed 2.00×)"
        );
        let reply = json!({ "ok": true, "text": "hi", "chars": 2, "pasted": true,
                            "engine": "whisper", "cleanup": { "state": "off" } });
        assert!(stop_line(&reply).unwrap().ends_with("· raw (cleanup off)"));
        let reply = json!({ "ok": true, "empty": true, "text": "", "engine": "parakeet" });
        assert_eq!(
            stop_line(&reply).unwrap(),
            "no speech heard · Parakeet TDT 0.6B v3"
        );
    }

    #[test]
    fn the_strip_gets_a_short_cleanup_label_and_never_an_argv() {
        let reply = json!({
            "ok": true, "text": "hi", "chars": 2, "pasted": true, "engine": "parakeet",
            "cleanup": { "state": "cleaned",
                         "backend": "agent:/usr/bin/llm --key sk-secret --model x" },
        });
        assert_eq!(
            stop_line(&reply).unwrap(),
            "dictated 2 chars · Parakeet TDT 0.6B v3 · cleanup agent:llm"
        );
        let long = format!("claude exited with 1: {}\nsecond line", "x".repeat(200));
        let reply = json!({
            "ok": true, "text": "hi", "chars": 2, "pasted": true, "engine": "parakeet",
            "cleanup": { "state": "failed", "why": long },
        });
        let line = stop_line(&reply).unwrap();
        assert!(
            !line.contains('\n') && !line.contains("second line"),
            "{line}"
        );
        assert!(line.ends_with("…)"), "a cut reason still closes: {line}");
        assert!(line.chars().count() < 160, "{line}");
    }

    #[test]
    fn a_reply_from_an_older_daemon_still_reads() {
        let reply = json!({ "ok": true, "text": "hi", "chars": 2, "pasted": true });
        assert_eq!(stop_line(&reply).unwrap(), "dictated 2 chars");
        let reply = json!({ "ok": true, "text": "hi", "pasted": false, "paste_error": "gone" });
        assert!(stop_line(&reply).is_err());
    }
}
