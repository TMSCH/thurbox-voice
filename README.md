# thurbox-voice

Local dictation for [thurbox](https://github.com/Thurbeen/thurbox): speak a
prompt and it is transcribed **on your machine**. No audio leaves it.

**Status: early.** It works end to end; expect rough edges.

## Use it in thurbox

`ctrl+space` starts recording for the selected session, and `ctrl+space` again
stops it. The text is pasted into that session's input box **without being
submitted**, so you read it and press Enter yourself. While you talk, the
message bar shows `● REC 0:07 → <session>`.

```bash
# 1. The helper (needs Rust and cmake: `brew install cmake`)
git clone https://github.com/TMSCH/thurbox-voice && cd thurbox-voice
cargo install --path crates/thurbox-voice
thurbox-voice pull parakeet              # 671 MB; `pull whisper` for the other engine

# 2. The pane
thurbox-cli plugin install git+https://github.com/TMSCH/thurbox-voice
# then in thurbox: Ctrl+,  →  ]  →  select "voice"  →  t   (trust it to run programs)
```

**Requires** a thurbox with `run(…, { machine = "local" })`
([Thurbeen/thurbox#TBD](https://github.com/Thurbeen/thurbox)), so the helper
runs on your machine (where the microphone is), even for a remote session.

**In the palette** (`Ctrl+P`):

- `voice: cancel the recording`
- `voice: switch speech engine`

**In settings** (`Ctrl+,`): `voice.engine`.

**Behind the key** is a daemon (`thurbox-voice start/stop/cancel/status/quit`).
It keeps the model loaded between dictations and exits after
`[voice] unload_after_secs` (default 600) of idle time. Its log is `daemon.log`
in the data directory.

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
model = "claude-haiku-4-5"

[cleanup.openai]          # any OpenAI-compatible endpoint, key from OPENAI_API_KEY
model = "your-model-id"
base_url = "https://api.openai.com/v1"   # or http://localhost:11434/v1 (Ollama): fully local

[cleanup.agent]           # headless agent CLI, reusing its login (slow, ~4-5 s)
command = ["codex", "exec", "--skip-git-repo-check"]
```

- **Choosing a backend.** `auto` follows `--agent`: `claude` prefers Anthropic,
  `codex` prefers OpenAI. When neither key is set, it falls back to that
  agent's own CLI (`claude -p --model haiku`, `codex exec`, `gemini -p`,
  `opencode run`).
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
