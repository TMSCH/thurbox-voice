# thurbox-voice

Local dictation for [thurbox](https://github.com/Thurbeen/thurbox): speak a
prompt and it is transcribed **on your machine**. No audio leaves it.

**Status: phase-0 spike.** This is a plain CLI for measuring the engines on real
prompts. The thurbox pane (`ctrl+space` to talk) and the background daemon come
next.

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

## Attribution

Parakeet TDT 0.6B v3 © NVIDIA, licensed under
[CC-BY-4.0](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3). The ONNX
export is by [istupakov](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx).
Engines are run through [transcribe-rs](https://github.com/cjpais/transcribe-rs).
