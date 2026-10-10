//! The pane, run as thurbox runs it: `plugins/50_voice.lua` loaded into Lua 5.4
//! with the globals thurbox gives a plugin (`thurbox`, `state`, `store`, `run`,
//! `command`, `require`), driven by the key events thurbox delivers and the
//! answers its `run` capability publishes.
//!
//! The host here is a stand-in, but only for what thurbox itself does — queue a
//! program, publish its answer, call `on_action` with or without the `event`
//! argument. Every decision under test is the pane's own code. No program is
//! ever started: a `run` is recorded and answered by the test.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mlua::{Function, Lua, Table, Value};

const TOGGLE: &str = "voice.toggle";
const CANCEL: &str = "voice.cancel";

/// `lib.settings` and `lib.theme` as thurbox ships them, reduced to what the
/// pane reads: a setting's effective value from the published registry, and a
/// theme role per name.
const LIBS: &str = r#"
package.preload["lib.settings"] = function()
  local settings = {}
  function settings.get(plugin, id, fallback)
    for _, entry in ipairs(thurbox.registry.settings) do
      if entry.plugin == plugin and entry.id == id then
        if entry.value ~= nil then return entry.value end
        return fallback
      end
    end
    return fallback
  end
  return settings
end
package.preload["lib.theme"] = function()
  return setmetatable({}, { __index = function(_, role) return role end })
end
"#;

/// What kind of thurbox the pane is running under.
#[derive(Clone, Copy)]
enum Keyboard {
    /// A thurbox from before key releases: `on_action(action)` only, and no
    /// `thurbox.keyboard`.
    OldHost,
    /// `thurbox.keyboard.releases` as published.
    Releases(&'static str),
}

type Runs = Rc<RefCell<Vec<(String, String)>>>;

/// Answers from programs a live host really ran: key, ok, stdout, stderr.
type Finished = Arc<Mutex<Vec<(String, bool, String, String)>>>;

struct Host {
    lua: Lua,
    plugin: Table,
    /// Every program the pane asked `run` for, in order, by run key.
    runs: Runs,
    keyboard: Keyboard,
    now: f64,
    /// Set when `run` executes programs for real, against a [`Daemon`].
    finished: Option<Finished>,
}

fn thurbox(lua: &Lua) -> Table {
    lua.globals().get("thurbox").unwrap()
}

fn registry(lua: &Lua, field: &str) -> Table {
    thurbox(lua)
        .get::<Table>("registry")
        .unwrap()
        .get(field)
        .unwrap()
}

impl Host {
    fn new(keyboard: Keyboard) -> Self {
        Self::with(keyboard, None)
    }

    /// A host whose `run` starts each program the way thurbox does — `sh -c`,
    /// on a thread, the answer published when it exits — against `daemon`'s
    /// isolated install.
    fn live(keyboard: Keyboard, daemon: &Daemon) -> Self {
        Self::with(keyboard, Some(daemon.env()))
    }

    fn with(keyboard: Keyboard, env: Option<Vec<(String, String)>>) -> Self {
        let lua = Lua::new();
        lua.load(LIBS).exec().unwrap();
        let globals = lua.globals();
        let published = lua
            .load(
                r#"{
                  sessions = {
                    { id = "11111111-aaaa", name = "api" },
                    { id = "22222222-bbbb", name = "web" },
                  },
                  runs = {},
                  registry = { keys = {}, settings = {} },
                }"#,
            )
            .eval::<Table>()
            .unwrap();
        if let Keyboard::Releases(releases) = keyboard {
            let table = lua.create_table().unwrap();
            table.set("releases", releases).unwrap();
            published.set("keyboard", table).unwrap();
        }
        globals.set("thurbox", published).unwrap();
        let store = lua.create_table().unwrap();
        store.set("selected", "11111111-aaaa").unwrap();
        globals.set("store", store).unwrap();
        globals.set("state", lua.create_table().unwrap()).unwrap();

        let runs: Runs = Rc::default();
        let seen = Rc::clone(&runs);
        let finished: Option<Finished> = env.as_ref().map(|_| Finished::default());
        let done = finished.clone();
        let run = lua
            .create_function(move |lua, (key, program, _opts): (String, String, Value)| {
                let answers: Table = thurbox(lua).get("runs")?;
                if answers.get::<Value>(key.as_str())?.is_nil() {
                    let pending = lua.create_table()?;
                    pending.set("state", "pending")?;
                    answers.set(key.as_str(), pending)?;
                    seen.borrow_mut().push((key.clone(), program.clone()));
                    if let (Some(env), Some(done)) = (env.clone(), done.clone()) {
                        std::thread::spawn(move || {
                            let output = Command::new("sh")
                                .arg("-c")
                                .arg(&program)
                                .env_clear()
                                .envs(env)
                                .output()
                                .unwrap();
                            done.lock().unwrap().push((
                                key,
                                output.status.success(),
                                String::from_utf8_lossy(&output.stdout).into_owned(),
                                String::from_utf8_lossy(&output.stderr).into_owned(),
                            ));
                        });
                    }
                }
                Ok(())
            })
            .unwrap();
        globals.set("run", run).unwrap();
        // `command("set", …)` is how the pane changes its own setting; thurbox
        // republishes the registry with the new value.
        let command = lua
            .create_function(|lua, (verb, opts): (String, Table)| {
                if verb == "set" {
                    let text: String = opts.get("text")?;
                    let (plugin, id) = text.split_once('.').unwrap();
                    let value: Value = opts.get("value")?;
                    set_setting(lua, plugin, id, value)?;
                }
                Ok(())
            })
            .unwrap();
        globals.set("command", command).unwrap();

        let source = std::fs::read_to_string(pane_path()).unwrap();
        let plugin: Table = lua.load(&source).set_name("50_voice.lua").eval().unwrap();

        // Collect the declaration the way thurbox does: settings at their
        // defaults, keys at their declared chord.
        for entry in plugin
            .get::<Table>("settings")
            .unwrap()
            .sequence_values::<Table>()
        {
            let entry = entry.unwrap();
            let id: String = entry.get("id").unwrap();
            set_setting(&lua, "voice", &id, entry.get("default").unwrap()).unwrap();
        }
        let keys = registry(&lua, "keys");
        for entry in plugin
            .get::<Table>("keys")
            .unwrap()
            .sequence_values::<Table>()
        {
            let entry = entry.unwrap();
            let row = lua.create_table().unwrap();
            row.set("key", entry.get::<String>("key").unwrap()).unwrap();
            row.set("action", entry.get::<String>("action").unwrap())
                .unwrap();
            keys.push(row).unwrap();
        }

        Host {
            lua,
            plugin,
            runs,
            keyboard,
            now: 0.0,
            finished,
        }
    }

    fn declared(&self, field: &str, key: &str, name: &str) -> Table {
        self.plugin
            .get::<Table>(field)
            .unwrap()
            .sequence_values::<Table>()
            .map(Result::unwrap)
            .find(|t| t.get::<String>(key).unwrap() == name)
            .unwrap_or_else(|| panic!("voice declares {field} {name}"))
    }

    fn setting(&self, id: &str) -> String {
        registry(&self.lua, "settings")
            .sequence_values::<Table>()
            .map(Result::unwrap)
            .find(|s| s.get::<String>("id").unwrap() == id)
            .map(|s| s.get::<String>("value").unwrap())
            .unwrap()
    }

    /// What the user did in the settings modal, or what `ui.json` restored on
    /// launch: thurbox publishes the override as the setting's value.
    fn set(&self, id: &str, value: &str) {
        let value = Value::String(self.lua.create_string(value).unwrap());
        set_setting(&self.lua, "voice", id, value).unwrap();
    }

    fn select(&self, id: &str) {
        let store: Table = self.lua.globals().get("store").unwrap();
        store.set("selected", id).unwrap();
    }

    fn action(&self, action: &str, event: Option<&str>) -> bool {
        let handler: Function = self.plugin.get("on_action").unwrap();
        match (self.keyboard, event) {
            // An old thurbox never passes an argument, and never a release.
            (Keyboard::OldHost, Some("release")) => true,
            (Keyboard::OldHost, _) | (_, None) => handler.call(action).unwrap(),
            (_, Some(event)) => {
                let args = self.lua.create_table().unwrap();
                args.set("event", event).unwrap();
                handler.call((action, args)).unwrap()
            }
        }
    }

    fn press(&self) -> bool {
        self.action(TOGGLE, Some("press"))
    }

    fn release(&self) -> bool {
        self.action(TOGGLE, Some("release"))
    }

    /// One frame: the pane starts whatever it queued, and says what it shows.
    fn frame(&mut self) -> String {
        self.now += 0.5;
        let exited: Vec<_> = self
            .finished
            .as_ref()
            .map(|f| std::mem::take(&mut *f.lock().unwrap()))
            .unwrap_or_default();
        let answers: Table = thurbox(&self.lua).get("runs").unwrap();
        for (key, ok, stdout, stderr) in exited {
            let answer = self.lua.create_table().unwrap();
            answer
                .set("state", if ok { "done" } else { "failed" })
                .unwrap();
            answer.set("ok", ok).unwrap();
            answer.set("stdout", stdout).unwrap();
            answer.set("stderr", stderr).unwrap();
            answers.set(key.as_str(), answer).unwrap();
        }
        let render: Function = self.plugin.get("render").unwrap();
        let ctx = self.lua.create_table().unwrap();
        ctx.set("elapsed", self.now).unwrap();
        let node: Table = render.call(ctx).unwrap();
        let mut out = String::new();
        for line in node
            .get::<Table>("text")
            .unwrap()
            .sequence_values::<Table>()
        {
            for span in line.unwrap().sequence_values::<Table>() {
                out.push_str(&span.unwrap().get::<String>("text").unwrap());
            }
        }
        out
    }

    fn programs(&self) -> Vec<String> {
        self.runs.borrow().iter().map(|(_, p)| p.clone()).collect()
    }

    /// The answer to the oldest still-pending run whose program starts with
    /// `prefix`, as thurbox publishes it when the program exits.
    fn answer(&mut self, prefix: &str, ok: bool, stdout: &str, stderr: &str) {
        let answers: Table = thurbox(&self.lua).get("runs").unwrap();
        let pending = |key: &str| {
            answers
                .get::<Table>(key)
                .unwrap()
                .get::<String>("state")
                .unwrap()
                == "pending"
        };
        let key = self
            .runs
            .borrow()
            .iter()
            .find(|(key, program)| program.starts_with(prefix) && pending(key))
            .map(|(key, _)| key.clone())
            .unwrap_or_else(|| panic!("no pending run of {prefix:?} in {:?}", self.programs()));
        let answer = self.lua.create_table().unwrap();
        answer
            .set("state", if ok { "done" } else { "failed" })
            .unwrap();
        answer.set("ok", ok).unwrap();
        answer.set("stdout", stdout).unwrap();
        answer.set("stderr", stderr).unwrap();
        answer.set("status", if ok { 0 } else { 1 }).unwrap();
        answers.set(key.as_str(), answer).unwrap();
        self.frame();
    }

    fn count(&self, prefix: &str) -> usize {
        self.programs()
            .iter()
            .filter(|p| p.starts_with(prefix))
            .count()
    }

    fn starts(&self) -> usize {
        self.count("thurbox-voice start")
    }

    fn stops(&self) -> usize {
        self.count("thurbox-voice stop")
    }
}

fn set_setting(lua: &Lua, plugin: &str, id: &str, value: Value) -> mlua::Result<()> {
    let settings = registry(lua, "settings");
    for entry in settings.sequence_values::<Table>() {
        let entry = entry?;
        if entry.get::<String>("plugin")? == plugin && entry.get::<String>("id")? == id {
            return entry.set("value", value);
        }
    }
    let entry = lua.create_table()?;
    entry.set("plugin", plugin)?;
    entry.set("id", id)?;
    entry.set("value", value)?;
    settings.push(entry)
}

/// `THURBOX_VOICE_PANE` runs these tests against another copy of the pane — an
/// older release, to show a regression test fails without its fix.
fn pane_path() -> PathBuf {
    std::env::var_os("THURBOX_VOICE_PANE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/50_voice.lua")
        })
}

fn choices(setting: &Table) -> Vec<String> {
    setting
        .get::<Table>("choices")
        .expect("the setting declares its choices, so F6 steps through them")
        .sequence_values()
        .map(Result::unwrap)
        .collect()
}

const PARAKEET: &str = "Parakeet TDT 0.6B v3";
const WHISPER: &str = "Whisper large-v3-turbo";

// ── the model setting ───────────────────────────────────────────────────────

#[test]
fn the_speech_model_is_a_choice_in_settings_not_free_text() {
    let host = Host::new(Keyboard::Releases("reported"));
    let engine = host.declared("settings", "id", "engine");
    assert_eq!(choices(&engine), ["parakeet", "whisper"]);
    assert_eq!(engine.get::<String>("default").unwrap(), "parakeet");
    let desc: String = engine.get("desc").unwrap();
    assert!(
        desc.contains("Speech recognition model") && desc.contains("not the cleanup"),
        "the row says which model it is: {desc}"
    );
}

#[test]
fn the_chosen_model_is_used_and_shown_from_start_to_outcome() {
    let mut host = Host::new(Keyboard::Releases("reported"));
    // Restored from ui.json on launch, exactly as a choice made in F6.
    host.set("engine", "whisper");
    assert!(
        host.frame().contains(WHISPER),
        "the idle strip names the model"
    );
    host.press();
    host.frame();
    assert_eq!(
        host.programs()[0],
        "thurbox-voice start --session '11111111-aaaa' --engine 'whisper'"
    );
    assert!(host.frame().contains(WHISPER));
    host.answer("thurbox-voice start", true, "recording with whisper", "");
    let recording = host.frame();
    assert!(recording.contains("REC") && recording.contains(WHISPER));
    host.press();
    let transcribing = host.frame();
    assert!(
        transcribing.contains("transcribing") && transcribing.contains(WHISPER),
        "the model is on screen throughout transcription: {transcribing}"
    );
    host.answer(
        "thurbox-voice stop",
        true,
        "dictated 12 chars · Whisper large-v3-turbo · cleanup anthropic:claude-haiku-5-5",
        "",
    );
    let outcome = host.frame();
    assert!(outcome.contains("dictated 12 chars") && outcome.contains(WHISPER));
    assert!(outcome.contains("→ api"), "{outcome}");
}

#[test]
fn a_model_change_mid_recording_does_not_touch_the_recording() {
    let mut host = Host::new(Keyboard::Releases("reported"));
    host.press();
    host.frame();
    host.answer("thurbox-voice start", true, "recording with parakeet", "");
    host.set("engine", "whisper");
    let recording = host.frame();
    assert!(
        recording.contains(PARAKEET),
        "the strip shows the model this recording was started with: {recording}"
    );
    host.press();
    let transcribing = host.frame();
    assert!(transcribing.contains(PARAKEET), "{transcribing}");
    // `stop` names no engine: the daemon transcribes with the one it captured.
    assert_eq!(host.programs()[1], "thurbox-voice stop");
    host.answer(
        "thurbox-voice stop",
        true,
        "dictated 3 chars · Parakeet TDT 0.6B v3",
        "",
    );
    // The next dictation takes the new choice.
    host.press();
    host.frame();
    assert!(host.programs()[2].ends_with("--engine 'whisper'"));
}

#[test]
fn the_palette_switch_cycles_the_same_setting() {
    let host = Host::new(Keyboard::Releases("reported"));
    host.action("voice.engine", None);
    assert_eq!(host.setting("engine"), "whisper");
    host.action("voice.engine", None);
    assert_eq!(host.setting("engine"), "parakeet");
}

// ── the recording mode ──────────────────────────────────────────────────────

#[test]
fn recording_mode_is_a_choice_that_defaults_to_toggle() {
    let host = Host::new(Keyboard::Releases("reported"));
    let mode = host.declared("settings", "id", "mode");
    assert_eq!(choices(&mode), ["toggle", "hold"]);
    assert_eq!(mode.get::<String>("default").unwrap(), "toggle");
    let key = host.declared("keys", "action", TOGGLE);
    assert_eq!(key.get::<String>("key").unwrap(), "ctrl+space");
    assert!(
        key.get::<Option<bool>>("release").unwrap().unwrap_or(false),
        "the chord asks thurbox for its release"
    );
}

#[test]
fn toggle_press_starts_press_stops_and_releases_do_nothing() {
    let mut host = Host::new(Keyboard::Releases("reported"));
    host.press();
    host.release();
    host.frame();
    host.answer("thurbox-voice start", true, "recording with parakeet", "");
    host.release();
    assert!(
        host.frame().contains("REC"),
        "a release does not stop a toggle"
    );
    assert_eq!(host.stops(), 0);
    host.press();
    host.frame();
    host.release();
    host.frame();
    assert_eq!((host.starts(), host.stops()), (1, 1));
}

#[test]
fn hold_records_from_press_to_release() {
    let mut host = Host::new(Keyboard::Releases("reported"));
    host.set("mode", "hold");
    assert!(
        host.frame().contains("hold ctrl+space"),
        "idle says how to talk"
    );
    host.press();
    host.frame();
    host.answer("thurbox-voice start", true, "recording with parakeet", "");
    assert!(host.frame().contains("release ctrl+space to stop"));
    host.release();
    host.frame();
    assert_eq!((host.starts(), host.stops()), (1, 1));
    assert!(host.frame().contains("transcribing"));
}

#[test]
fn a_release_while_the_start_is_still_pending_stops_once_it_has_started() {
    let mut host = Host::new(Keyboard::Releases("reported"));
    host.set("mode", "hold");
    host.press();
    host.frame();
    host.release();
    host.frame();
    // Nothing is sent over the start still in flight: a stop racing it could
    // reach the daemon first and leave the microphone on.
    assert_eq!(host.stops(), 0, "{:?}", host.programs());
    assert!(host.frame().contains("starting"));
    host.answer("thurbox-voice start", true, "recording with parakeet", "");
    host.frame();
    assert_eq!(host.stops(), 1, "the stop follows the start");
}

#[test]
fn a_second_press_while_the_start_is_pending_also_waits_for_it() {
    let mut host = Host::new(Keyboard::Releases("reported"));
    host.press();
    host.frame();
    host.press();
    host.frame();
    assert_eq!(host.stops(), 0);
    host.answer("thurbox-voice start", true, "recording with parakeet", "");
    host.frame();
    assert_eq!((host.starts(), host.stops()), (1, 1));
}

#[test]
fn a_cancel_while_the_start_is_pending_waits_for_it_too() {
    let mut host = Host::new(Keyboard::Releases("reported"));
    host.press();
    host.frame();
    host.action(CANCEL, None);
    host.frame();
    assert_eq!(host.count("thurbox-voice cancel"), 0);
    host.answer("thurbox-voice start", true, "recording with parakeet", "");
    host.frame();
    assert_eq!(host.programs().last().unwrap(), "thurbox-voice cancel");
    host.answer("thurbox-voice cancel", true, "cancelled", "");
    assert!(host.frame().contains("discarded"));
}

#[test]
fn a_cancel_while_starting_is_not_undone_by_a_later_press_or_release() {
    for hold in [false, true] {
        let mut host = Host::new(Keyboard::Releases("reported"));
        if hold {
            host.set("mode", "hold");
        }
        host.press();
        host.frame();
        host.action(CANCEL, None);
        if hold {
            host.release();
        } else {
            host.press();
        }
        host.frame();
        host.answer("thurbox-voice start", true, "recording with parakeet", "");
        host.frame();
        assert_eq!(
            host.stops(),
            0,
            "a cancelled recording is never transcribed"
        );
        assert_eq!(host.programs().last().unwrap(), "thurbox-voice cancel");
    }
}

#[test]
fn a_failed_start_leaves_nothing_pending() {
    let mut host = Host::new(Keyboard::Releases("reported"));
    host.set("mode", "hold");
    host.press();
    host.frame();
    host.release();
    host.answer(
        "thurbox-voice start",
        false,
        "",
        "Error: whisper is not installed",
    );
    assert!(host.frame().contains("not installed"));
    assert_eq!(host.stops(), 0);
    // And the next press is a fresh start, not a stale stop.
    host.press();
    host.frame();
    assert_eq!(host.starts(), 2);
}

#[test]
fn the_target_session_is_captured_at_start() {
    let mut host = Host::new(Keyboard::Releases("reported"));
    host.press();
    host.frame();
    host.answer("thurbox-voice start", true, "recording with parakeet", "");
    host.select("22222222-bbbb");
    let recording = host.frame();
    assert!(recording.contains("→ api"), "{recording}");
    host.press();
    host.frame();
    host.answer(
        "thurbox-voice stop",
        true,
        "dictated 3 chars · Parakeet TDT 0.6B v3",
        "",
    );
    assert!(host.frame().contains("→ api"));
    assert!(host.programs()[0].contains("--session '11111111-aaaa'"));
}

#[test]
fn hold_falls_back_to_toggle_where_releases_are_not_reported() {
    for keyboard in [Keyboard::Releases("unsupported"), Keyboard::OldHost] {
        let mut host = Host::new(keyboard);
        host.set("mode", "hold");
        let idle = host.frame();
        assert!(
            idle.contains("no key releases") && idle.contains("press to start"),
            "says hold is unavailable and what happens instead: {idle}"
        );
        host.press();
        host.frame();
        host.answer("thurbox-voice start", true, "recording with parakeet", "");
        assert!(host.frame().contains("ctrl+space to stop"));
        host.press();
        host.frame();
        assert_eq!((host.starts(), host.stops()), (1, 1));
    }
}

#[test]
fn a_lost_release_never_leaves_a_hold_recording_stuck() {
    let mut host = Host::new(Keyboard::Releases("negotiated"));
    host.set("mode", "hold");
    host.press();
    host.frame();
    host.answer("thurbox-voice start", true, "recording with parakeet", "");
    // The terminal never reported the release (the window lost focus, say):
    // the next press stops.
    host.press();
    host.frame();
    assert_eq!(host.stops(), 1);
}

#[test]
fn presses_during_transcription_are_ignored_not_queued() {
    let mut host = Host::new(Keyboard::Releases("reported"));
    host.press();
    host.frame();
    host.answer("thurbox-voice start", true, "recording with parakeet", "");
    host.press();
    host.frame();
    host.press();
    host.release();
    host.frame();
    assert_eq!((host.starts(), host.stops()), (1, 1));
    host.answer(
        "thurbox-voice stop",
        true,
        "dictated 3 chars · Parakeet TDT 0.6B v3",
        "",
    );
    host.frame();
    assert_eq!(host.starts(), 1, "nothing was queued behind the stop");
}

#[test]
fn the_mode_in_force_is_captured_at_start() {
    let mut host = Host::new(Keyboard::Releases("reported"));
    host.set("mode", "hold");
    host.press();
    host.frame();
    host.answer("thurbox-voice start", true, "recording with parakeet", "");
    host.set("mode", "toggle");
    host.release();
    host.frame();
    assert_eq!(host.stops(), 1, "the release of a hold still stops it");
}

// ── live: the pane driving the real helper and daemon ───────────────────────

/// An isolated `thurbox-voice` install: its own data directory and config, a
/// quiet stand-in microphone, cleanup off, and a `thurbox-cli` that refuses
/// everything, so no real session can be touched. The models are sparse files
/// of the pinned sizes: present as far as `start` checks, never loaded, since
/// a quiet recording has no speech to transcribe.
struct Daemon {
    home: PathBuf,
}

/// The Parakeet files `start` checks for, at their pinned sizes.
const PARAKEET_FILES: &[(&str, u64)] = &[
    ("encoder-model.int8.onnx", 652_183_999),
    ("decoder_joint-model.int8.onnx", 18_202_004),
    ("nemo128.onnx", 139_764),
    ("vocab.txt", 93_939),
];

impl Daemon {
    fn new(name: &str) -> Self {
        // A Unix socket path is capped near 100 bytes, which a target
        // directory under a deep checkout can exceed; HOME's cache is short.
        let home = PathBuf::from(std::env::var_os("HOME").unwrap())
            .join(".cache/thurbox-voice-tests")
            .join(format!("{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let models = home.join("models/parakeet");
        std::fs::create_dir_all(&models).unwrap();
        for (file, size) in PARAKEET_FILES {
            std::fs::File::create(models.join(file))
                .unwrap()
                .set_len(*size)
                .unwrap();
        }
        std::fs::write(home.join("config.toml"), "[cleanup]\nenabled = false\n").unwrap();
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let stub = bin.join("thurbox-cli");
        std::fs::write(
            &stub,
            "#!/bin/sh\necho 'thurbox-cli is stubbed in tests' >&2\nexit 1\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&stub, perms).unwrap();
        Daemon { home }
    }

    fn env(&self) -> Vec<(String, String)> {
        let helper = Path::new(env!("CARGO_BIN_EXE_thurbox-voice"))
            .parent()
            .unwrap();
        let path = format!(
            "{}:{}:{}",
            self.home.join("bin").display(),
            helper.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let home = self.home.display().to_string();
        vec![
            ("PATH".into(), path),
            ("HOME".into(), home.clone()),
            ("THURBOX_VOICE_HOME".into(), home.clone()),
            ("THURBOX_VOICE_CONFIG".into(), format!("{home}/config.toml")),
            ("THURBOX_VOICE_TEST_INPUT".into(), "quiet".into()),
            // Belt and braces: should the stand-in ever not be taken, no
            // audio system is reachable from here either.
            ("ALSA_CONFIG_PATH".into(), format!("{home}/no-alsa.conf")),
            ("PULSE_SERVER".into(), format!("unix:{home}/no-pulse")),
            ("PIPEWIRE_REMOTE".into(), format!("{home}/no-pipewire")),
        ]
    }

    fn cli(&self, args: &[&str]) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_thurbox-voice"))
            .args(args)
            .env_clear()
            .envs(self.env())
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// What the daemon itself says: the ground truth a strip can disagree with.
    fn status(&self) -> serde_json::Value {
        serde_json::from_str(self.cli(&["status"]).trim()).unwrap()
    }

    fn recording(&self) -> bool {
        let status = self.status();
        if status["state"] == "recording" {
            assert_eq!(
                status["device"], "test input (quiet)",
                "never a real microphone"
            );
            return true;
        }
        false
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.cli(&["quit"]);
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

impl Host {
    /// Frames until the strip shows `wanted`, the way thurbox keeps rendering
    /// while programs run.
    fn until(&mut self, wanted: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let shown = self.frame();
            if shown.contains(wanted) {
                return shown;
            }
            assert!(
                Instant::now() < deadline,
                "the strip never showed {wanted:?}; last: {shown:?}; ran {:?}",
                self.programs()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn live_a_second_press_while_starting_stops_the_recording() {
    let daemon = Daemon::new("second-press");
    let mut host = Host::live(Keyboard::Releases("unsupported"), &daemon);
    host.press();
    host.frame();
    // The start is still launching the daemon: the user, or a terminal's
    // auto-repeat, presses again.
    host.press();
    host.until("no speech heard");
    assert!(!daemon.recording(), "the microphone was left on");
    // And the shortcut still works afterwards.
    host.press();
    host.until("REC");
    assert!(daemon.recording());
    host.press();
    host.until("no speech heard");
    assert!(!daemon.recording());
}

#[test]
fn live_a_hold_released_before_the_start_finishes_stops_it() {
    let daemon = Daemon::new("hold-release");
    let mut host = Host::live(Keyboard::Releases("reported"), &daemon);
    host.set("mode", "hold");
    host.press();
    host.frame();
    host.release();
    host.until("no speech heard");
    assert!(!daemon.recording());
}

#[test]
fn live_cancel_while_starting_discards_the_recording() {
    let daemon = Daemon::new("cancel");
    let mut host = Host::live(Keyboard::Releases("reported"), &daemon);
    host.press();
    host.frame();
    host.action(CANCEL, None);
    host.until("discarded");
    assert!(!daemon.recording());
}

#[test]
fn live_stop_works_after_the_selection_moves() {
    let daemon = Daemon::new("focus");
    let mut host = Host::live(Keyboard::Releases("reported"), &daemon);
    host.press();
    host.until("REC");
    host.select("22222222-bbbb");
    assert!(host.frame().contains("→ api"));
    host.press();
    host.until("no speech heard");
    assert!(!daemon.recording());
}

#[test]
fn live_a_pane_that_lost_track_of_a_recording_can_still_stop_it() {
    let daemon = Daemon::new("reload");
    {
        let mut before = Host::live(Keyboard::Releases("reported"), &daemon);
        before.press();
        before.until("REC");
    }
    // An interface reload starts the pane over with empty state while the
    // daemon is still recording.
    assert!(daemon.recording());
    let mut host = Host::live(Keyboard::Releases("reported"), &daemon);
    host.press();
    host.until("REC");
    host.press();
    host.until("no speech heard");
    assert!(!daemon.recording());
}

#[test]
fn live_cancel_reaches_a_recording_the_pane_lost_track_of() {
    let daemon = Daemon::new("reload-cancel");
    {
        let mut before = Host::live(Keyboard::Releases("reported"), &daemon);
        before.press();
        before.until("REC");
    }
    let mut host = Host::live(Keyboard::Releases("reported"), &daemon);
    host.action(CANCEL, None);
    host.until("discarded");
    assert!(!daemon.recording());
}

#[test]
fn the_palette_has_a_stop_that_reaches_any_recording() {
    let host = Host::new(Keyboard::Releases("reported"));
    let stop = host.declared("commands", "action", "voice.stop");
    assert!(stop.get::<String>("desc").unwrap().contains("stop"));

    let daemon = Daemon::new("palette-stop");
    {
        let mut before = Host::live(Keyboard::Releases("reported"), &daemon);
        before.press();
        before.until("REC");
        before.action("voice.stop", None);
        before.until("no speech heard");
        assert!(!daemon.recording());
        before.press();
        before.until("REC");
    }
    // After a reload, too.
    let mut host = Host::live(Keyboard::Releases("reported"), &daemon);
    host.action("voice.stop", None);
    host.until("no speech heard");
    assert!(!daemon.recording());
    // And with nothing recording, it says so.
    host.action("voice.stop", None);
    host.until("not recording");
}

#[test]
fn live_a_press_never_takes_over_another_sessions_recording_as_its_own() {
    let daemon = Daemon::new("other-session");
    {
        let mut before = Host::live(Keyboard::Releases("reported"), &daemon);
        before.press();
        before.until("REC");
    }
    // After a reload the user picks another session and holds the key to talk
    // to it: the running recording is for "api", not for them.
    let mut host = Host::live(Keyboard::Releases("reported"), &daemon);
    host.set("mode", "hold");
    host.select("22222222-bbbb");
    host.press();
    host.release();
    let shown = host.until("already recording");
    assert!(shown.contains("→ api"), "{shown}");
    assert!(
        daemon.recording(),
        "a release does not stop someone else's recording"
    );
    // Only a deliberate press, with the strip naming its session, stops it.
    host.press();
    host.until("no speech heard");
    assert!(!daemon.recording());
}

#[test]
fn a_failed_status_is_reported_not_taken_for_idle() {
    let mut host = Host::new(Keyboard::Releases("reported"));
    host.action("voice.stop", None);
    host.frame();
    host.answer(
        "thurbox-voice status",
        false,
        "",
        "Error: no answer from the thurbox-voice daemon",
    );
    let shown = host.frame();
    assert!(
        shown.contains("no answer from the thurbox-voice daemon"),
        "{shown}"
    );
}

#[test]
fn live_a_daemon_whose_socket_is_gone_exits_even_while_recording() {
    let daemon = Daemon::new("orphan");
    let mut host = Host::live(Keyboard::Releases("reported"), &daemon);
    host.press();
    host.until("REC");
    // What a killed test run leaves: its directory deleted, the daemon alive.
    std::fs::remove_file(daemon.home.join("daemon.sock")).unwrap();
    let log = daemon.home.join("daemon.log");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(&log)
        .unwrap_or_default()
        .contains("socket is gone")
    {
        assert!(
            Instant::now() < deadline,
            "the daemon kept running without its socket"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_palette_cancel_or_stop_survives_finding_another_sessions_recording() {
    const RUNNING: &str = r#"{"device":"mic","engine":"parakeet","ok":true,"running":true,"session":"11111111-aaaa","state":"recording"}"#;
    // Alone, and after a press's own stop (a quick second press) got there
    // first.
    for (action, then, press_first) in [
        (CANCEL, "thurbox-voice cancel", false),
        ("voice.stop", "thurbox-voice stop", false),
        ("voice.stop", "thurbox-voice stop", true),
    ] {
        let mut host = Host::new(Keyboard::Releases("reported"));
        host.select("22222222-bbbb");
        host.press();
        host.frame();
        if press_first {
            host.press();
        }
        host.action(action, None);
        host.answer(
            "thurbox-voice start",
            false,
            "",
            "Error: already recording — stop or cancel first",
        );
        host.frame();
        host.answer("thurbox-voice status", true, RUNNING, "");
        host.frame();
        assert_eq!(
            host.programs().last().unwrap(),
            then,
            "what the palette asked for still happens: {:?}",
            host.programs()
        );
    }
}
