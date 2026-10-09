//! `config.toml`: which model cleans transcripts up, and how to reach it.
//!
//! Every field is optional, and a missing file is the defaults, so the helper
//! works with no config at all; cleanup then needs only a key in the
//! environment or an agent CLI on `PATH`.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub voice: VoiceConfig,
    pub context: ContextConfig,
    pub cleanup: CleanupConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VoiceConfig {
    /// The speech engine the daemon uses unless a `start` names another.
    pub engine: crate::engine::EngineId,
    /// The daemon exits after this long with nothing to do, which is what
    /// gives the model's memory back. Its next `start` reloads it.
    pub unload_after_secs: u64,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            engine: crate::engine::EngineId::Parakeet,
            unload_after_secs: 600,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    /// Words you say that a speech model will not know: your company, your
    /// services, your colleagues' names. Added to the built-in thurbox list.
    pub glossary: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// Follow the agent being dictated to; fall back to whatever is reachable.
    #[default]
    Auto,
    Anthropic,
    Openai,
    Agent,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanupConfig {
    pub enabled: bool,
    pub backend: Backend,
    pub anthropic: AnthropicConfig,
    pub openai: OpenAiConfig,
    pub agent: AgentConfig,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: Backend::Auto,
            anthropic: AnthropicConfig::default(),
            openai: OpenAiConfig::default(),
            agent: AgentConfig::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnthropicConfig {
    /// A Messages API model id. The API has no family alias like `haiku`, so
    /// this is an exact id; the default is the newest Haiku.
    pub model: String,
    pub base_url: String,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            model: "claude-haiku-5-5".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiConfig {
    /// No default worth guessing: OpenAI's fast models change often, and a
    /// local server names its own. Unset means this backend is not configured.
    pub model: Option<String>,
    /// Any OpenAI-compatible endpoint — Ollama, LM Studio and llama.cpp's
    /// server included, which is what keeps a fully local pipeline possible.
    pub base_url: String,
    /// Environment variable holding the key. A local server needs none.
    pub api_key_env: String,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            model: None,
            base_url: "https://api.openai.com/v1".to_string(),
            api_key_env: "OPENAI_API_KEY".to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// argv for a headless agent; the prompt is appended as the last argument
    /// and the answer read from stdout. Unset means: pick the preset for the
    /// agent being dictated to.
    pub command: Option<Vec<String>>,
    /// The Codex model family the codex preset uses — a name without a
    /// version, resolved against Codex's own catalog at run time, so a new
    /// release is picked up without a change here. An exact model id works
    /// too.
    pub codex_model: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            command: None,
            codex_model: "luna".to_string(),
        }
    }
}

/// `$THURBOX_VOICE_CONFIG`, else `$XDG_CONFIG_HOME/thurbox-voice/config.toml`,
/// else `~/.config/thurbox-voice/config.toml`.
pub fn path() -> PathBuf {
    if let Some(file) = std::env::var_os("THURBOX_VOICE_CONFIG") {
        return PathBuf::from(file);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".config")
        });
    base.join("thurbox-voice/config.toml")
}

pub fn load() -> Result<Config> {
    let path = path();
    match fs::read_to_string(&path) {
        Ok(body) => toml::from_str(&body).with_context(|| format!("parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_is_the_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.cleanup.enabled);
        assert_eq!(config.cleanup.backend, Backend::Auto);
        assert_eq!(config.cleanup.anthropic.model, "claude-haiku-5-5");
        assert!(config.cleanup.openai.model.is_none());
        assert_eq!(config.voice.engine, crate::engine::EngineId::Parakeet);
        assert!(config.context.glossary.is_empty());
    }

    #[test]
    fn voice_and_context_parse() {
        let config: Config = toml::from_str(
            "[voice]\nengine = \"whisper\"\nunload_after_secs = 60\n\n[context]\nglossary = [\"Spotpay\"]\n",
        )
        .unwrap();
        assert_eq!(config.voice.engine, crate::engine::EngineId::Whisper);
        assert_eq!(config.voice.unload_after_secs, 60);
        assert_eq!(config.context.glossary, vec!["Spotpay"]);
    }

    #[test]
    fn the_documented_example_parses() {
        let config: Config = toml::from_str(
            r#"
            [cleanup]
            enabled = true
            backend = "openai"

            [cleanup.anthropic]
            model = "claude-haiku-4-5"

            [cleanup.openai]
            model = "luna"
            base_url = "http://localhost:11434/v1"

            [cleanup.agent]
            command = ["codex", "exec", "--skip-git-repo-check"]
            "#,
        )
        .unwrap();
        assert_eq!(config.cleanup.backend, Backend::Openai);
        // A pinned model overrides the default.
        assert_eq!(config.cleanup.anthropic.model, "claude-haiku-4-5");
        assert_eq!(config.cleanup.openai.model.as_deref(), Some("luna"));
        assert_eq!(config.cleanup.agent.command.unwrap()[0], "codex");
    }

    #[test]
    fn a_typo_is_an_error_not_a_silent_default() {
        assert!(toml::from_str::<Config>("[cleanup]\nbakend = \"openai\"").is_err());
    }
}
