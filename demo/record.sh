#!/usr/bin/env bash
# Record media/demo.gif: one real dictation through the voice strip.
#
#   demo/record.sh
#
# Everything on screen is the real thing: the installed `thurbox` and
# `thurbox-voice`, this checkout's plugin, the Parakeet model, and an
# `opencode` session whose composer receives the text. Only the voice is not a
# person. The microphone is a private PipeWire source fed a sentence that
# espeak-ng synthesises at record time, so the take is the same every run and
# nobody's microphone is opened.
#
# Isolation, so a recording never touches the thurbox you work in:
#   * thurbox runs on its own config, data dir, tmux socket and TMUX_TMPDIR;
#   * thurbox-voice runs on its own data dir (THURBOX_VOICE_HOME), with the
#     models linked read-only from your store, and its own config with the
#     cleanup pass off, so no transcript leaves the machine;
#   * HOME is a fresh directory, so opencode boots with no account or history;
#   * the daemon captures from the virtual source by name (PIPEWIRE_NODE). The
#     default source is not changed, and the script checks that it was not.
#
# Requirements: thurbox, thurbox-cli and thurbox-voice on PATH, with the
# Parakeet model pulled (`thurbox-voice pull parakeet`); opencode; tmux, git,
# python3 (>= 3.11), sqlite3; PipeWire with pipewire-alsa (pw-loopback,
# pw-play, pactl); espeak-ng; asciinema 2.x and agg; JetBrains Mono plus a
# symbol font for agg (FONT_DIR, FONT_FAMILY).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${OUT:-$REPO_ROOT/media/demo.gif}"

# What is said, and the voice saying it. en-gb-x-rp is the espeak-ng voice
# Parakeet transcribes word for word.
SENTENCE="${SENTENCE:-Add a test for an empty recording, then run the whole suite again.}"
VOICE="${VOICE:-en-gb-x-rp}"

COLS="${COLS:-110}"
ROWS="${ROWS:-30}"
FONT_SIZE="${FONT_SIZE:-16}"
FONT_DIR="${FONT_DIR:-/usr/share/fonts}"
FONT_FAMILY="${FONT_FAMILY:-JetBrains Mono,Noto Sans Symbols 2,Noto Sans Symbols}"
# The theme every thurbox clip is recorded in (metadata.active_theme).
THEME="${THEME:-doom}"

# Large files (the repo, the cast, opencode's cache) live under target/, which
# is gitignored. The unix sockets need a short path, so they and the files
# whose paths can reach the screen live under the runtime dir instead.
SBX="$REPO_ROOT/target/demo"
SHORT="${XDG_RUNTIME_DIR:-/tmp}/tvdemo"

SOURCE_NODE="tvdemo.mic"
SINK_NODE="tvdemo.in"

for bin in thurbox thurbox-cli thurbox-voice opencode tmux git python3 sqlite3 \
    pw-loopback pw-play pactl espeak-ng asciinema agg; do
    command -v "$bin" >/dev/null 2>&1 || { echo "error: $bin not found on PATH" >&2; exit 1; }
done
case "$(asciinema --version)" in
    "asciinema 2."*) ;;
    *) echo "error: asciinema 2.x is needed (the trim reads asciicast v2)" >&2; exit 1 ;;
esac

MODELS="${THURBOX_VOICE_MODELS:-$(env -u THURBOX_VOICE_HOME thurbox-voice config | sed -n 's/^data: *//p')/models}"
[ -d "$MODELS/parakeet" ] || {
    echo "error: no Parakeet model under $MODELS — run 'thurbox-voice pull parakeet'" >&2
    exit 1
}

# --- The sandbox -------------------------------------------------------------
LOOPBACK=""
cleanup() {
    tmux -L tvdemo-rec kill-server 2>/dev/null || true
    THURBOX_VOICE_HOME="$SHORT/voice" thurbox-voice quit >/dev/null 2>&1 || true
    # thurbox can bring its server back while it exits, so kill it twice.
    for _ in 1 2; do
        sleep 1
        tmux -L tvdemo kill-server 2>/dev/null || true
    done
    cp "$THURBOX_VOICE_HOME/daemon.log" "$SBX/" 2>/dev/null || true
    [ -n "$LOOPBACK" ] && kill "$LOOPBACK" 2>/dev/null || true
    rm -rf "$SHORT"
}
trap cleanup EXIT

rm -rf "$SBX" "$SHORT"
mkdir -p "$SBX"/{config,cache} "$SHORT"/{tmux,voice,home,data}

export TMUX_TMPDIR="$SHORT/tmux"
export THURBOX_CONFIG_DIR="$SBX/config"
export THURBOX_DATA_DIR="$SHORT/data"
export THURBOX_SOCKET="tvdemo"
unset THURBOX_SOCKET_FOR TMUX
export HOME="$SHORT/home"
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_DATA_HOME="$HOME/.local/share"
export XDG_STATE_HOME="$HOME/.local/state"
export XDG_CACHE_HOME="$SBX/cache"
export THURBOX_VOICE_HOME="$SHORT/voice"
export THURBOX_VOICE_CONFIG="$SBX/voice.toml"
export OPENCODE_DISABLE_AUTOUPDATE=true NO_UPDATE_NOTIFIER=1

ln -s "$MODELS" "$THURBOX_VOICE_HOME/models"
cat > "$THURBOX_VOICE_CONFIG" <<'EOF'
# The demo shows the speech engine alone: nothing is sent to a cleanup backend.
[cleanup]
enabled = false
EOF

# Both reach the network, and each would put something on screen that dates
# the clip or changes the machine.
cat > "$THURBOX_CONFIG_DIR/settings.toml" <<'EOF'
[features]
version_check = false
auto_update = false
EOF
printf 'default = "opencode"\n\n[[agents]]\nname = "opencode"\ncommand = "opencode"\n' \
    > "$THURBOX_CONFIG_DIR/agents.toml"

# --- The project the sessions work on ---------------------------------------
REPO="$HOME/voice-app"
git init -q -b main "$REPO"
printf '# voice-app\n' > "$REPO/README.md"
git -C "$REPO" add -A
git -C "$REPO" -c user.email=demo@example.com -c user.name=demo -c commit.gpgsign=false \
    commit -qm init

# --- The plugin, from this checkout, trusted to run programs ----------------
# The same `git+` route the README gives, cloned from this checkout's HEAD.
thurbox-cli plugin install "git+file://$REPO_ROOT" --text
paths=$(thurbox-cli config show --json | python3 -c '
import json, sys
paths = json.load(sys.stdin)["paths"]
print(paths["ui_dir"]); print(paths["ui_json"]); print(paths["database"])')
{ IFS= read -r UI_DIR; IFS= read -r UI_JSON; IFS= read -r DB; } <<< "$paths"

# A grant is a decision made in a running interface (Ctrl+, then ] then t), and
# there is no CLI for it. Write what that decision writes: the pin and digest
# plugins.lock recorded, keyed by the pane's absolute path.
python3 - "$UI_DIR" "$UI_JSON" <<'PY'
import json, pathlib, sys, tomllib
ui_dir, out = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
lock = tomllib.loads((ui_dir / "plugins.lock").read_text())
trusted = {}
for entry in lock.get("plugin", []):
    pane = entry["file"]
    trusted[str(ui_dir / pane)] = {
        "pin": f"{entry['src']}@{entry['version']}",
        "digest": entry["files"][pane],
    }
if not trusted:
    sys.exit("plugins.lock records no plugin")
out.write_text(json.dumps({"trusted": trusted}, indent=2))
PY
thurbox-cli plugin check --text

# --- Two sessions: the demo moves down to the second and dictates into it --
# Both built-in extensions announce themselves on screen, and `hooks` installs an
# opencode plugin that fails in a profile this bare. They activate on the first
# launch, so opt out the way `extension deactivate` records it.
sqlite3 "$DB" "
INSERT INTO metadata (key, value) VALUES ('builtin_hooks_optout', '1'), ('builtin_ui-skill_optout', '1')
  ON CONFLICT(key) DO UPDATE SET value = excluded.value;"
# thurbox opens with the first row selected, which is the first one created.
thurbox-cli session create --name fix-flaky-ci --repo-path "$REPO" \
    --worktree-branch fix/flaky-ci --text >/dev/null
thurbox-cli session create --name empty-recording-test --repo-path "$REPO" \
    --worktree-branch test/empty-recording --text >/dev/null
TARGET=$(thurbox-cli session list --json | python3 -c '
import json, sys
data = json.load(sys.stdin)
rows = data["sessions"] if isinstance(data, dict) else data
print(next(r["id"] for r in rows if r["name"] == "empty-recording-test"))')

sqlite3 "$DB" "
INSERT INTO metadata (key, value) VALUES ('v2_interface_acknowledged', '1')
  ON CONFLICT(key) DO UPDATE SET value = excluded.value;
INSERT INTO metadata (key, value) VALUES ('active_theme', '$THEME')
  ON CONFLICT(key) DO UPDATE SET value = excluded.value;"

# --- The microphone: a private source, and the sentence to play into it -----
espeak-ng -v "$VOICE" -s 150 -w "$SBX/sentence.wav" "$SENTENCE"
before=$(pactl get-default-source)
pw-loopback -n tvdemo -c 1 -m '[ MONO ]' \
    -i "{ media.class=Audio/Sink node.name=$SINK_NODE node.virtual=true priority.session=0 priority.driver=0 node.dont-fallback=true }" \
    -o "{ media.class=Audio/Source node.name=$SOURCE_NODE node.virtual=true priority.session=0 priority.driver=0 node.dont-fallback=true }" \
    >/dev/null 2>&1 &
LOOPBACK=$!
sleep 1.5
[ "$(pactl get-default-source)" = "$before" ] || {
    echo "error: the default source changed when the demo source appeared" >&2
    exit 1
}

# --- Record ------------------------------------------------------------------
CAST="$SBX/demo.cast"
tmux -L tvdemo-rec new-session -d -x "$COLS" -y "$ROWS" -c "$REPO" -s r \
    "PIPEWIRE_NODE=$SOURCE_NODE asciinema rec --overwrite --quiet --cols $COLS --rows $ROWS -c thurbox '$CAST'"

send() { tmux -L tvdemo-rec send-keys -t r "$@"; }
screen() { tmux -L tvdemo-rec capture-pane -p -t r; }
# Wait up to $2 seconds for the screen to show $1; fail the run if it never does,
# so a clip never ships a step that did not happen.
await() {
    for _ in $(seq 1 $(($2 * 10))); do
        screen | grep -qF -- "$1" && return 0
        sleep 0.1
    done
    echo "error: never saw '$1' on screen. Last screen:" >&2
    screen >&2
    exit 1
}
mark() { printf '%s\n' "$(date +%s.%N) $1" >> "$SBX/marks"; }

await "→ fix-flaky-ci" 30
sleep 4                              # opencode finishes drawing its composer
mark ready
# Select the session to dictate into. The strip names it.
send C-j
await "→ empty-recording-test" 5
sleep 1.5
send C-Space
await "● REC" 15
mark rec
sleep 0.6
pw-play --target "$SINK_NODE" "$SBX/sentence.wav"
sleep 0.6
send C-Space
await "dictated" 60
# The text is in the composer, and nothing submitted it.
sleep 1
thurbox-cli session capture "$TARGET" --lines 60 --text | grep -qF "empty recording" || {
    echo "error: the transcript is not on the target session's screen" >&2
    exit 1
}
sleep 4
mark end
send C-q
sleep 3
[ "$(pactl get-default-source)" = "$before" ] || echo "warning: the default source changed" >&2

# --- Cut and render ----------------------------------------------------------
# Keep from 1.5 s before `ready` to `end`; everything earlier collapses into the
# first frame, so the clip opens on a painted screen instead of a boot. The
# marks are wall-clock and the cast counts from its own start, so the two are
# lined up on the first frame that shows the recording.
python3 - "$CAST" "$SBX/trimmed.cast" "$SBX/marks" <<'PY'
import json, sys
cast, out, marks = sys.argv[1:4]
lines = open(cast).read().splitlines()
header = json.loads(lines[0])
stamps = dict(l.split(" ", 1)[::-1] for l in open(marks).read().splitlines())
frames = [json.loads(line) for line in lines[1:]]
rec = next(t for t, kind, data in frames if kind == "o" and "● REC" in data)
offset = rec - float(stamps["rec"])
start = float(stamps["ready"]) + offset - 1.5
end = float(stamps["end"]) + offset
events, early = [], ""
for t, kind, data in frames:
    if kind != "o":
        continue
    if t < start:
        early += data
    elif t <= end:
        events.append([round(t - start, 6), "o", data])
with open(out, "w") as f:
    f.write(json.dumps(header) + "\n")
    f.write(json.dumps([0.0, "o", early]) + "\n")
    for e in events:
        f.write(json.dumps(e) + "\n")
PY
mkdir -p "$(dirname "$OUT")"
agg --font-dir "$FONT_DIR" --font-family "$FONT_FAMILY" --font-size "$FONT_SIZE" \
    --fps-cap 15 --idle-time-limit 2 --last-frame-duration 3 \
    "$SBX/trimmed.cast" "$OUT"
echo "==> $OUT"
ls -la "$OUT"
