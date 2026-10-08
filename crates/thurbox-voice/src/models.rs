//! The models thurbox-voice knows how to fetch, and the fetching.
//!
//! Every file is pinned to a Hugging Face commit and a SHA-256, so `pull` is
//! reproducible and a truncated or tampered download is refused rather than
//! loaded. Nothing here runs unless `pull` is asked for: models are never
//! fetched as a side effect of transcribing.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::engine::EngineId;

pub struct ModelFile {
    pub name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}

pub struct Model {
    pub engine: EngineId,
    pub description: &'static str,
    pub licence: &'static str,
    pub files: &'static [ModelFile],
}

// Each URL spells out its pinned revision in full; the test at the bottom
// checks every file of a model shares one.
pub const MODELS: &[Model] = &[
    Model {
        engine: EngineId::Parakeet,
        description: "NVIDIA Parakeet TDT 0.6B v3, int8 ONNX — 25 languages",
        licence: "CC-BY-4.0 (NVIDIA)",
        files: &[
            ModelFile {
                name: "encoder-model.int8.onnx",
                url: "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/encoder-model.int8.onnx",
                sha256: "6139d2fa7e1b086097b277c7149725edbab89cc7c7ae64b23c741be4055aff09",
                size: 652_183_999,
            },
            ModelFile {
                name: "decoder_joint-model.int8.onnx",
                url: "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/decoder_joint-model.int8.onnx",
                sha256: "eea7483ee3d1a30375daedc8ed83e3960c91b098812127a0d99d1c8977667a70",
                size: 18_202_004,
            },
            ModelFile {
                name: "nemo128.onnx",
                url: "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/nemo128.onnx",
                sha256: "a9fde1486ebfcc08f328d75ad4610c67835fea58c73ba57e3209a6f6cf019e9f",
                size: 139_764,
            },
            ModelFile {
                name: "vocab.txt",
                url: "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/vocab.txt",
                sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
                size: 93_939,
            },
        ],
    },
    Model {
        engine: EngineId::Whisper,
        description: "OpenAI Whisper large-v3-turbo, q5_0 GGML — 99 languages",
        licence: "MIT (OpenAI)",
        files: &[ModelFile {
            name: "ggml-large-v3-turbo-q5_0.bin",
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-large-v3-turbo-q5_0.bin",
            sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
            size: 574_041_195,
        }],
    },
];

pub fn model(engine: EngineId) -> &'static Model {
    MODELS
        .iter()
        .find(|m| m.engine == engine)
        .expect("every engine has a model entry")
}

impl Model {
    pub fn dir(&self, root: &Path) -> PathBuf {
        root.join(self.engine.as_str())
    }

    pub fn size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// Installed means every file is present at its pinned size. The hash is
    /// checked once, at download; re-hashing 650 MB on every load would cost
    /// more than the transcription.
    pub fn is_installed(&self, root: &Path) -> bool {
        let dir = self.dir(root);
        self.files.iter().all(|f| {
            fs::metadata(dir.join(f.name))
                .map(|m| m.len() == f.size)
                .unwrap_or(false)
        })
    }
}

/// Download every missing file of `model` into its directory under `root`.
pub fn pull(model: &Model, root: &Path) -> Result<()> {
    let dir = model.dir(root);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    for file in model.files {
        let dest = dir.join(file.name);
        if fs::metadata(&dest)
            .map(|m| m.len() == file.size)
            .unwrap_or(false)
        {
            eprintln!("  {} already present", file.name);
            continue;
        }
        download(file, &dest)?;
    }
    Ok(())
}

fn download(file: &ModelFile, dest: &Path) -> Result<()> {
    let part = dest.with_extension("part");
    let response = ureq::get(file.url)
        .call()
        .with_context(|| format!("GET {}", file.url))?;
    let mut body = response.into_body().into_reader();
    let mut out = File::create(&part).with_context(|| format!("create {}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut done: u64 = 0;
    let mut last_pct = u64::MAX;
    loop {
        let n = body.read(&mut buf).context("read download")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        done += n as u64;
        let pct = done * 100 / file.size.max(1);
        if pct != last_pct {
            last_pct = pct;
            eprint!(
                "\r  {} {:>3}% ({} / {})",
                file.name,
                pct,
                mb(done),
                mb(file.size)
            );
            io::stderr().flush().ok();
        }
    }
    eprintln!();
    out.sync_all()?;
    drop(out);

    let got = hex(&hasher.finalize());
    if got != file.sha256 {
        fs::remove_file(&part).ok();
        bail!(
            "{}: checksum mismatch (expected {}, got {got}) — download discarded",
            file.name,
            file.sha256
        );
    }
    fs::rename(&part, dest).with_context(|| format!("rename into {}", dest.display()))?;
    Ok(())
}

pub fn mb(bytes: u64) -> String {
    format!("{:.0} MB", bytes as f64 / 1_000_000.0)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARAKEET_BASE: &str =
        "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce";
    const WHISPER_BASE: &str =
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1";

    #[test]
    fn every_url_is_pinned_to_its_base() {
        for model in MODELS {
            let base = match model.engine {
                EngineId::Parakeet => PARAKEET_BASE,
                EngineId::Whisper => WHISPER_BASE,
            };
            for file in model.files {
                assert_eq!(file.url, format!("{base}/{}", file.name));
                assert_eq!(file.sha256.len(), 64);
            }
        }
    }
}
