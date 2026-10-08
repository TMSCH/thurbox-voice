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
    pub cleanup: CleanupConfig,
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
    pub model: String,
    pub base_url: String,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            model: "claude-haiku-4-5".to_string(),
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

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// argv for a headless agent; the prompt is appended as the last argument
    /// and the answer read from stdout. Unset means: pick the preset for the
    /// agent being dictated to.
    pub command: Option<Vec<String>>,
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
        assert_eq!(config.cleanup.anthropic.model, "claude-haiku-4-5");
        assert!(config.cleanup.openai.model.is_none());
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
        assert_eq!(config.cleanup.openai.model.as_deref(), Some("luna"));
        assert_eq!(config.cleanup.agent.command.unwrap()[0], "codex");
    }

    #[test]
    fn a_typo_is_an_error_not_a_silent_default() {
        assert!(toml::from_str::<Config>("[cleanup]\nbakend = \"openai\"").is_err());
    }
}
