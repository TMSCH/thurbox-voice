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
use std::path::PathBuf;
use std::rc::Rc;

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

struct Host {
    lua: Lua,
    plugin: Table,
    /// Every program the pane asked `run` for, in order, by run key.
    runs: Runs,
    keyboard: Keyboard,
    now: f64,
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
        let run = lua
            .create_function(move |lua, (key, program, _opts): (String, String, Value)| {
                let answers: Table = thurbox(lua).get("runs")?;
                if answers.get::<Value>(key.as_str())?.is_nil() {
                    let pending = lua.create_table()?;
                    pending.set("state", "pending")?;
                    answers.set(key.as_str(), pending)?;
                    seen.borrow_mut().push((key, program));
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

fn pane_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/50_voice.lua")
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
