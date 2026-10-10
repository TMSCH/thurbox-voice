//! The background half: holds the microphone and the model between a `start`
//! and a `stop`, so thurbox only ever fires short commands at it.
//!
//! `start`/`stop`/`cancel`/`status` are clients. Each sends one JSON line over a
//! Unix socket and reads one back. The first `start` launches the daemon; it
//! exits by itself after `[voice] unload_after_secs` with nothing to do, which
//! is how the model's memory is given back.
//!
//! On `stop` the daemon does the whole rest of the job itself — transcribe,
//! clean up, paste into the session through `thurbox-cli session send
//! --no-enter --force` — so the pane that asked only needs the summary.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context as _, Result};
use serde_json::{json, Value};

use crate::audio::{self, Recorder};
use crate::cleanup;
use crate::config::Config;
use crate::context;
use crate::engine::{Engine, EngineId};
use crate::models;
use crate::stats;

pub fn socket_path(root: &Path) -> PathBuf {
    root.join("daemon.sock")
}

// ── client ──────────────────────────────────────────────────────────────────

/// Send one request and return the daemon's answer. `launch` starts the
/// daemon when none is listening; every other verb reports that it is not
/// running instead.
pub fn request(root: &Path, request: &Value, launch: bool, wait: Duration) -> Result<Value> {
    let socket = socket_path(root);
    let mut stream = match UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(_) if launch => spawn(root)?,
        Err(_) => return Ok(json!({ "ok": true, "state": "idle", "running": false })),
    };
    stream.set_read_timeout(Some(wait))?;
    writeln!(stream, "{request}")?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .context("no answer from the thurbox-voice daemon")?;
    serde_json::from_str(&line).context("unreadable answer from the daemon")
}

/// Launch `thurbox-voice daemon` detached, and wait for its socket.
fn spawn(root: &Path) -> Result<UnixStream> {
    std::fs::create_dir_all(root)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("daemon.log"))?;
    let exe = std::env::current_exe().context("locate thurbox-voice")?;
    let mut command = Command::new(exe);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    // Its own process group, so the shell `run` started this client in can
    // exit, or be killed by its timeout, without taking the daemon with it.
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.spawn().context("start the thurbox-voice daemon")?;

    let socket = socket_path(root);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(stream) = UnixStream::connect(&socket) {
            return Ok(stream);
        }
        if Instant::now() > deadline {
            bail!(
                "the daemon did not come up — see {}",
                root.join("daemon.log").display()
            );
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

// ── server ──────────────────────────────────────────────────────────────────

struct Recording {
    recorder: Recorder,
    session: Option<String>,
    engine: EngineId,
}

/// The loaded model, shared with the thread that loads it in the background
/// so a `start` never waits for it.
type Warm = Arc<Mutex<Option<(EngineId, Engine)>>>;

struct Daemon {
    root: PathBuf,
    config: Config,
    recording: Option<Recording>,
    warm: Warm,
    loading: Option<std::thread::JoinHandle<()>>,
    /// What the last `stop` is doing, for `status`.
    busy: Option<&'static str>,
}

pub fn serve(root: &Path, config: Config) -> Result<()> {
    std::fs::create_dir_all(root)?;
    let socket = socket_path(root);
    // A socket file outliving its daemon (a crash, a reboot) refuses a bind.
    // Only remove it when nothing answers there, or two daemons would race.
    if socket.exists() {
        if UnixStream::connect(&socket).is_ok() {
            bail!("a thurbox-voice daemon is already running");
        }
        std::fs::remove_file(&socket).ok();
    }
    let listener = UnixListener::bind(&socket).context("bind the daemon socket")?;
    listener.set_nonblocking(true)?;
    eprintln!("[daemon] listening on {}", socket.display());

    let idle_limit = Duration::from_secs(config.voice.unload_after_secs.max(30));
    let mut daemon = Daemon {
        root: root.to_path_buf(),
        config,
        recording: None,
        warm: Arc::new(Mutex::new(None)),
        loading: None,
        busy: None,
    };
    let mut last = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(e) = daemon.answer(stream) {
                    eprintln!("[daemon] {e:#}");
                }
                last = Instant::now();
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Nobody can reach a daemon whose socket was deleted — its data
                // directory removed, say — so a recording it holds could never
                // be stopped. Exit rather than record forever.
                if !socket.exists() {
                    eprintln!("[daemon] the socket is gone, exiting");
                    break;
                }
                if daemon.recording.is_none() && last.elapsed() > idle_limit {
                    eprintln!("[daemon] idle for {}s, exiting", idle_limit.as_secs());
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e).context("accept"),
        }
    }
    daemon.unload();
    std::fs::remove_file(&socket).ok();
    Ok(())
}

impl Daemon {
    fn answer(&mut self, stream: UnixStream) -> Result<()> {
        stream.set_nonblocking(false)?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let reply = match self.handle(&request) {
            Ok(reply) => reply,
            Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
        };
        let mut stream = stream;
        writeln!(stream, "{reply}")?;
        Ok(())
    }

    fn handle(&mut self, request: &Value) -> Result<Value> {
        match request["cmd"].as_str().unwrap_or_default() {
            "start" => self.start(request),
            "stop" => self.stop(),
            "cancel" => {
                let was = self.recording.take().map(|r| r.recorder.stop()).is_some();
                Ok(json!({ "ok": true, "cancelled": was }))
            }
            "status" => Ok(self.status()),
            "quit" => {
                self.recording = None;
                self.unload();
                // The accept loop notices the missing socket on its next turn.
                std::fs::remove_file(socket_path(&self.root)).ok();
                std::process::exit(0);
            }
            other => bail!("unknown request {other:?}"),
        }
    }

    /// Free the model before the process exits. whisper.cpp's Metal backend
    /// asserts (and aborts) when the process is torn down with a model still
    /// holding GPU buffers.
    fn unload(&mut self) {
        if let Some(loading) = self.loading.take() {
            let _ = loading.join();
        }
        if let Ok(mut warm) = self.warm.lock() {
            *warm = None;
        }
    }

    fn status(&self) -> Value {
        let state = if self.recording.is_some() {
            "recording"
        } else {
            self.busy.unwrap_or("idle")
        };
        let elapsed = self
            .recording
            .as_ref()
            .map(|r| r.recorder.started.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        let warm = self
            .warm
            .lock()
            .ok()
            .and_then(|w| w.as_ref().map(|(id, _)| id.as_str()));
        json!({
            "ok": true,
            "running": true,
            "state": state,
            "elapsed": elapsed,
            "session": self.recording.as_ref().and_then(|r| r.session.clone()),
            "engine": self.recording.as_ref().map(|r| r.engine.as_str()),
            "device": self.recording.as_ref().map(|r| r.recorder.device.clone()),
            "warm": warm,
        })
    }

    fn start(&mut self, request: &Value) -> Result<Value> {
        if self.recording.is_some() {
            bail!("already recording — stop or cancel first");
        }
        let engine = match request["engine"].as_str() {
            Some("whisper") => EngineId::Whisper,
            Some("parakeet") => EngineId::Parakeet,
            Some(other) => bail!("unknown engine {other:?}"),
            None => self.config.voice.engine,
        };
        let models_root = self.root.join("models");
        if !models::model(engine).is_installed(&models_root) {
            bail!(
                "{} is not installed — run `thurbox-voice pull {}`",
                engine.as_str(),
                engine.as_str()
            );
        }
        let recorder = Recorder::start()?;
        self.preload(engine, models_root);
        let session = request["session"].as_str().map(str::to_string);
        eprintln!(
            "[daemon] recording from {} for {:?} with {}",
            recorder.device,
            session,
            engine.as_str()
        );
        let device = recorder.device.clone();
        self.recording = Some(Recording {
            recorder,
            session,
            engine,
        });
        Ok(json!({ "ok": true, "state": "recording", "engine": engine.as_str(), "device": device }))
    }

    /// Load `engine` on a thread while the user is still talking, unless it is
    /// already the warm one.
    fn preload(&mut self, engine: EngineId, models_root: PathBuf) {
        let ready = matches!(&*self.warm.lock().unwrap(), Some((id, _)) if *id == engine);
        if ready {
            return;
        }
        if let Some(previous) = self.loading.take() {
            let _ = previous.join();
        }
        let warm = Arc::clone(&self.warm);
        self.loading = Some(std::thread::spawn(move || {
            // Drop the old model first: two at once is ~1.3 GB.
            *warm.lock().unwrap() = None;
            match Engine::load(engine, &models_root) {
                Ok((loaded, took)) => {
                    eprintln!(
                        "[daemon] {} loaded in {:.2}s",
                        engine.as_str(),
                        took.as_secs_f64()
                    );
                    *warm.lock().unwrap() = Some((engine, loaded));
                }
                Err(e) => eprintln!("[daemon] load {}: {e:#}", engine.as_str()),
            }
        }));
    }

    fn stop(&mut self) -> Result<Value> {
        let Some(Recording {
            recorder,
            session,
            engine,
        }) = self.recording.take()
        else {
            bail!("not recording");
        };
        let clip = recorder.stop();
        if audio::is_blocked(&clip.samples) {
            bail!(
                "the microphone delivered pure silence — allow your terminal app in System \
                 Settings › Privacy & Security › Microphone, then restart it"
            );
        }
        let resampled = audio::to_16k(&clip)?;
        let Some(speech) = audio::trim_silence(&resampled) else {
            return Ok(json!({
                "ok": true,
                "empty": true,
                "text": "",
                "chars": 0,
                "engine": engine.as_str(),
            }));
        };
        let audio_s = speech.len() as f64 / audio::RATE as f64;

        self.busy = Some("transcribing");
        let result = self.finish(session.as_deref(), engine, speech, audio_s);
        self.busy = None;
        result
    }

    fn finish(
        &mut self,
        session: Option<&str>,
        engine_id: EngineId,
        speech: &[f32],
        audio_s: f64,
    ) -> Result<Value> {
        let load_wait = Instant::now();
        if let Some(loading) = self.loading.take() {
            let _ = loading.join();
        }
        let load_s = load_wait.elapsed().as_secs_f64();
        let ctx = context::build(&self.config.context, session);

        let started = Instant::now();
        let raw = {
            let mut warm = self.warm.lock().unwrap();
            let (id, engine) = warm
                .as_mut()
                .filter(|(id, _)| *id == engine_id)
                .context("the model failed to load — see daemon.log")?;
            let vocabulary = (*id == EngineId::Whisper).then_some(ctx.vocabulary.as_str());
            engine.transcribe(speech, vocabulary)?
        };
        let infer_s = started.elapsed().as_secs_f64();

        let mut text = raw.clone();
        let mut cleanup_log = None;
        // What became of the cleanup, for the one line the strip shows.
        let mut outcome = json!({ "state": "off" });
        if self.config.cleanup.enabled && !raw.is_empty() {
            // A broken instructions file costs the user's extra rules, never
            // the dictation.
            let instructions = self.config.cleanup.user_instructions().unwrap_or_else(|e| {
                eprintln!("[daemon] cleanup instructions skipped: {e:#}");
                None
            });
            match cleanup::run(
                &self.config.cleanup,
                self.config.cleanup.backend,
                ctx.agent.as_deref(),
                &cleanup::system_prompt(instructions.as_deref()),
                Some(&ctx.text),
                &raw,
            ) {
                Ok(cleaned) => {
                    eprintln!(
                        "[daemon] cleanup {} {:.2}s{}",
                        cleaned.backend,
                        cleaned.secs,
                        cleaned
                            .rejected
                            .as_deref()
                            .map(|r| format!(" (rejected: {r})"))
                            .unwrap_or_default()
                    );
                    outcome = match &cleaned.rejected {
                        Some(why) => {
                            json!({ "state": "refused", "backend": cleaned.backend, "why": why })
                        }
                        None => json!({ "state": "cleaned", "backend": cleaned.backend }),
                    };
                    text = cleaned.text;
                    cleanup_log = Some(stats::CleanupLog {
                        backend: cleaned.backend,
                        secs: cleaned.secs,
                        model_output: cleaned
                            .rejected
                            .is_some()
                            .then(|| cleaned.model_output.clone()),
                        rejected: cleaned.rejected,
                    });
                }
                // The raw text is still a usable dictation.
                Err(e) => {
                    eprintln!("[daemon] cleanup failed: {e:#}");
                    outcome = json!({ "state": "failed", "why": format!("{e:#}") });
                }
            }
        }

        let pasted = match (session, text.is_empty()) {
            (Some(session), false) => Some(paste(session, &text)),
            _ => None,
        };

        stats::append(
            &self.root,
            &stats::Entry {
                at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                engine: engine_id.as_str().to_string(),
                audio_s,
                load_s,
                infer_s,
                chars: text.chars().count(),
                clip: None,
                text: text.clone(),
                raw: cleanup_log.as_ref().map(|_| raw.clone()),
                cleanup: cleanup_log,
            },
        )
        .ok();

        let mut reply = json!({
            "ok": true,
            "text": text,
            "chars": text.chars().count(),
            "audio_s": audio_s,
            "infer_s": infer_s,
            "engine": engine_id.as_str(),
            "cleanup": outcome,
        });
        match pasted {
            Some(Ok(())) => reply["pasted"] = json!(true),
            Some(Err(e)) => {
                reply["pasted"] = json!(false);
                reply["paste_error"] = json!(format!("{e:#}"));
            }
            None => reply["pasted"] = json!(false),
        }
        Ok(reply)
    }
}

/// Into the session's composer, not submitted. `--force` types even when the
/// user has already started a line, so a dictation can finish a sentence.
fn paste(session: &str, text: &str) -> Result<()> {
    let output = Command::new("thurbox-cli")
        .args(["session", "send", session, text, "--no-enter", "--force"])
        .stdin(Stdio::null())
        .output()
        .context("run thurbox-cli")?;
    if !output.status.success() {
        bail!(
            "thurbox-cli session send: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
