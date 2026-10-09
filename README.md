# thurbox-voice

Local dictation for [thurbox](https://github.com/Thurbeen/thurbox): speak a
prompt and it is transcribed **on your machine**. No audio leaves it.

**Status: early.** It works end to end; expect rough edges.

## Use it in thurbox

`ctrl+space` starts recording for the selected session, and `ctrl+space` again
stops it. The text is pasted into that session's input box **without being
submitted**, so you read it and press Enter yourself.

A one-line strip above the bars is always there, so nothing moves when you
start or stop. It shows one of:

- `○ ctrl+space to start talking → <session>` when idle;
- `● REC 0:07 → <session>` while recording;
- `⋯ transcribing`, then the outcome for a few seconds.

```bash
# 1. The helper (needs Rust and cmake: `brew install cmake`)
git clone https://github.com/TMSCH/thurbox-voice && cd thurbox-voice
cargo install --path crates/thurbox-voice
thurbox-voice pull parakeet              # 671 MB; `pull whisper` for the other engine

# 2. The pane
thurbox-cli plugin install git+https://github.com/TMSCH/thurbox-voice
# then in thurbox: Ctrl+,  →  ]  →  select "voice"  →  t   (trust it to run programs)
```

**3. The strip places itself.** The pane declares itself a strip, and
thurbox places strips above the bars on its own
([Thurbeen/thurbox#1370](https://github.com/Thurbeen/thurbox/pull/1370)).
That takes two rows: the voice line, then a gap.

If your `~/.config/thurbox/ui/layout.lua` is customised and predates strips,
`thurbox-cli plugin check` will say so. Add this once, before the message band
(`if status_rows() > 0 then`):

```lua
for _, strip in ipairs(ctx.strips or {}) do
  children[#children + 1] = { slot = strip.slot, len = strip.len }
end
```

**Requires** a thurbox with `run(…, { machine = "local" })`
([Thurbeen/thurbox#1369](https://github.com/Thurbeen/thurbox/pull/1369)), so the helper
runs on your machine (where the microphone is), even for a remote session.

**In the palette** (`Ctrl+P`):

- `voice: cancel the recording`
- `voice: switch speech engine`

**In settings** (`Ctrl+,`): `voice.engine`.

**Behind the key** is a daemon (`thurbox-voice start/stop/cancel/status/quit`).
It keeps the model loaded between dictations and exits after
`[voice] unload_after_secs` (default 600) of idle time. Its log is `daemon.log`
in the data directory.

## Architecture

```text
microphone ─▶ speech-to-text ─▶ LLM cleanup ─▶ thurbox session input
   cpal        Parakeet or        + context      thurbox-cli session send
               Whisper, local                    --no-enter
```

A daemon holds the microphone and the loaded model. On `start` it records
through `cpal`, resampled to 16 kHz mono. On `stop` it transcribes on your
machine with the selected engine: Parakeet TDT (ONNX) or Whisper
(whisper.cpp), both run through transcribe-rs. The transcript then goes to an
LLM that fixes misheard words, and only those. It is given the built-in word
list, your glossary, and the target session's name, repo, branch and agent,
plus the last ~60 lines of its screen. A guard keeps the raw text if the
output strays too far from it. The result is pasted into the session's input
box with `thurbox-cli session send --no-enter`, unsubmitted. See
[Context](#context) and [Cleanup pass](#cleanup-pass) for each stage.

## Engines

| Engine | Model | Download | Licence |
|---|---|---|---|
| `parakeet` | NVIDIA Parakeet TDT 0.6B v3, int8 ONNX | 671 MB | CC-BY-4.0 |
| `whisper` | OpenAI Whisper large-v3-turbo, q5_0 GGML (Metal on macOS) | 574 MB | MIT |

Models are pinned to a Hugging Face revision and checked against a SHA-256.
They are stored under `$THURBOX_VOICE_HOME`, else
`$XDG_DATA_HOME/thurbox-voice`, else `~/.local/share/thurbox-voice`.

## Build

You need Rust and `cmake` (for whisper.cpp): `brew install cmake` on macOS.

```bash
cargo install --path crates/thurbox-voice
```

## Try it

```bash
thurbox-voice pull parakeet
thurbox-voice pull whisper
thurbox-voice models

# Talk, then press Enter.
thurbox-voice test
thurbox-voice test --engine whisper --vocabulary "thurbox, kubectl, Spotpay"

# Keep a recording, then replay the same audio through both engines.
thurbox-voice test --save clip.wav
thurbox-voice test --file clip.wav --engine parakeet --engine whisper

# Median load and inference time per engine, from every run so far.
thurbox-voice stats
```

- **Microphone permission.** The first recording triggers a microphone prompt
  for your **terminal app**, not for thurbox-voice. If you get "the microphone
  delivered pure silence", allow the terminal in System Settings › Privacy &
  Security › Microphone and restart it.
- **Run log.** Every transcription is appended to `compare.jsonl` in the data
  directory, recording the engine, the trimmed audio length, the load and
  inference times, and the text.

## Context

The cleanup model is told what you are probably talking about:

- **Built-in words** that are always in play, like `thurbox`, `tmux`,
  `worktree`, `Claude Code`, `Codex` and `nextest`, so "toolbox" becomes
  "thurbox".
- **Your glossary**, set in `config.toml`:

  ```toml
  [context]
  glossary = ["Spotpay", "payouts", "ledger"]
  ```

- **The target session:** its name, repo, branch and agent, plus the last ~60
  lines of its screen (`thurbox-cli session capture`).

Whisper is also primed with the same word list.

## Cleanup pass

Speech models mishear jargon ("cargo next test"). An optional second pass has
an LLM fix misheard words, using whatever context it is given, before the text
is pasted. It is on by default once a backend is reachable. `--raw` skips it.

```toml
# ~/.config/thurbox-voice/config.toml (`thurbox-voice config` prints the path)
[cleanup]
enabled = true
backend = "auto"          # "auto" | "anthropic" | "openai" | "agent"

[cleanup.anthropic]       # key from ANTHROPIC_API_KEY
model = "claude-haiku-5-5"  # the default: the newest Haiku

[cleanup.openai]          # any OpenAI-compatible endpoint, key from OPENAI_API_KEY
model = "your-model-id"
base_url = "https://api.openai.com/v1"   # or http://localhost:11434/v1 (Ollama): fully local

[cleanup.agent]           # headless agent CLI, reusing its login
codex_model = "luna"      # a model family, resolved against Codex's catalog
# command = [...]         # or a command of your own; the prompt is appended
```

- **Choosing a backend.** `auto` follows `--agent`: `claude` prefers Anthropic,
  `codex` prefers OpenAI. When neither key is set, it falls back to that
  agent's own CLI, started lean: `claude -p --model haiku` (~1.7 s), or
  `codex exec` with the newest Luna at low effort (~2.5 s). Either one uses
  your subscription login.
- **Choosing a model.** The Anthropic backend calls the Messages API, which
  needs an exact model id: it defaults to `claude-haiku-5-5`, and
  `[cleanup.anthropic] model` overrides it. The `claude` CLI fallback passes
  the alias `haiku` instead, which Claude Code resolves to its newest Haiku.
- **Refused outputs.** If the cleaned text drifts too far from the raw
  transcript (length or shared words), it is refused and the raw text is kept.
  This is how an output that *answers* the dictation, instead of correcting it,
  is caught.

```bash
thurbox-voice cleanup --agent codex "run cargo next test"   # text only, no audio
thurbox-voice test --file clip.wav --agent claude --context-file ctx.txt
thurbox-voice stats                                          # adds per-backend cleanup timings
```

## Attribution

Parakeet TDT 0.6B v3 © NVIDIA, licensed under
[CC-BY-4.0](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3). The ONNX
export is by [istupakov](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx).
Engines are run through [transcribe-rs](https://github.com/cjpais/transcribe-rs).
