//! The two engines behind one call: samples in, text out.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use transcribe_rs::onnx::parakeet::{ParakeetModel, ParakeetParams};
use transcribe_rs::onnx::Quantization;
use transcribe_rs::whisper_cpp::{WhisperEngine, WhisperInferenceParams};

use crate::models;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum EngineId {
    Parakeet,
    Whisper,
}

impl EngineId {
    pub fn as_str(self) -> &'static str {
        match self {
            EngineId::Parakeet => "parakeet",
            EngineId::Whisper => "whisper",
        }
    }
}

pub enum Engine {
    Parakeet(Box<ParakeetModel>),
    Whisper(Box<WhisperEngine>),
}

impl Engine {
    /// Load `id`'s model from under `root`. Returns how long the load took,
    /// which is the number that decides whether keeping a model warm matters.
    pub fn load(id: EngineId, root: &Path) -> Result<(Self, Duration)> {
        let model = models::model(id);
        if !model.is_installed(root) {
            anyhow::bail!(
                "{} is not installed — run `thurbox-voice pull {}` ({})",
                id.as_str(),
                id.as_str(),
                models::mb(model.size())
            );
        }
        let dir = model.dir(root);
        let started = Instant::now();
        let engine = match id {
            EngineId::Parakeet => Engine::Parakeet(Box::new(
                ParakeetModel::load(&dir, &Quantization::Int8)
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .context("load parakeet")?,
            )),
            EngineId::Whisper => Engine::Whisper(Box::new(
                WhisperEngine::load(&dir.join(model.files[0].name))
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .context("load whisper")?,
            )),
        };
        Ok((engine, started.elapsed()))
    }

    /// Transcribe 16 kHz mono samples. `vocabulary` biases Whisper towards
    /// words it would otherwise mishear; Parakeet has no equivalent and
    /// ignores it.
    pub fn transcribe(&mut self, samples: &[f32], vocabulary: Option<&str>) -> Result<String> {
        let text = match self {
            Engine::Parakeet(model) => {
                model
                    .transcribe_with(samples, &ParakeetParams::default())
                    .map_err(|e| anyhow::anyhow!("{e}"))?
                    .text
            }
            Engine::Whisper(engine) => {
                engine
                    .transcribe_with(
                        samples,
                        &WhisperInferenceParams {
                            initial_prompt: vocabulary.map(str::to_string),
                            ..Default::default()
                        },
                    )
                    .map_err(|e| anyhow::anyhow!("{e}"))?
                    .text
            }
        };
        Ok(text.trim().to_string())
    }
}
