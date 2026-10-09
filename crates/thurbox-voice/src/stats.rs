//! `compare.jsonl`: one line per transcription, so a week of real use says
//! which engine to default to.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct Entry {
    /// Seconds since the Unix epoch.
    pub at: u64,
    pub engine: String,
    /// Length of the clip after trimming, in seconds.
    pub audio_s: f64,
    pub load_s: f64,
    pub infer_s: f64,
    pub chars: usize,
    /// The clip's file, when it was saved or read from one — what lets two
    /// engines' lines about the same audio be put side by side.
    pub clip: Option<String>,
    /// The final text: cleaned when cleanup ran and was accepted.
    pub text: String,
    /// The speech engine's own text, when a cleanup pass ran after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<CleanupLog>,
}

#[derive(Serialize, Deserialize)]
pub struct CleanupLog {
    pub backend: String,
    pub secs: f64,
    /// Why the guard refused the output, when it did.
    pub rejected: Option<String>,
    /// What the model said when it was refused, so a refusal can be judged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_output: Option<String>,
}

pub fn append(root: &Path, entry: &Entry) -> Result<()> {
    fs::create_dir_all(root)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("compare.jsonl"))?;
    writeln!(file, "{}", serde_json::to_string(entry)?)?;
    Ok(())
}

pub fn print(root: &Path) -> Result<()> {
    let path = root.join("compare.jsonl");
    let Ok(body) = fs::read_to_string(&path) else {
        println!("no transcriptions logged yet ({})", path.display());
        return Ok(());
    };
    let mut by_engine: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        if let Ok(entry) = serde_json::from_str::<Entry>(line) {
            by_engine
                .entry(entry.engine.clone())
                .or_default()
                .push(entry);
        }
    }
    println!(
        "{:<10} {:>5} {:>10} {:>10} {:>11} {:>9}",
        "engine", "runs", "audio s", "median load", "median infer", "× realtime"
    );
    for (engine, entries) in &by_engine {
        let audio: f64 = entries.iter().map(|e| e.audio_s).sum();
        let infer_total: f64 = entries.iter().map(|e| e.infer_s).sum();
        println!(
            "{:<10} {:>5} {:>10.1} {:>10.2}s {:>11.2}s {:>9.1}",
            engine,
            entries.len(),
            audio,
            median(entries.iter().map(|e| e.load_s)),
            median(entries.iter().map(|e| e.infer_s)),
            if infer_total > 0.0 {
                audio / infer_total
            } else {
                0.0
            },
        );
    }

    let mut by_backend: BTreeMap<&str, Vec<&CleanupLog>> = BTreeMap::new();
    for log in by_engine
        .values()
        .flatten()
        .filter_map(|e| e.cleanup.as_ref())
    {
        by_backend.entry(&log.backend).or_default().push(log);
    }
    if !by_backend.is_empty() {
        println!();
        println!(
            "{:<40} {:>5} {:>12} {:>9}",
            "cleanup", "runs", "median time", "rejected"
        );
        for (backend, logs) in &by_backend {
            println!(
                "{:<40} {:>5} {:>11.2}s {:>9}",
                backend,
                logs.len(),
                median(logs.iter().map(|l| l.secs)),
                logs.iter().filter(|l| l.rejected.is_some()).count(),
            );
        }
    }
    Ok(())
}

fn median(values: impl Iterator<Item = f64>) -> f64 {
    let mut v: Vec<f64> = values.collect();
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}
