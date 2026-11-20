//! What the cleanup model is told about where the dictation is going.
//!
//! Three layers, most general first: words that are always in play when
//! talking to thurbox (so "toolbox" becomes "thurbox"), the user's own
//! glossary, and the target session — its name, repo, branch, agent, and the
//! last of what is on its screen. Names that appear here are very likely what
//! was said, which is the whole trick.

use std::process::Command;

use crate::fleet;

/// Always in play when dictating to a coding agent inside thurbox. Product and
/// tool names a speech model has rarely or never heard.
const BUILTIN: &[&str] = &[
    "thurbox",
    "Fleet",
    "thurbox-voice",
    "tmux",
    "worktree",
    "Claude Code",
    "Claude",
    "Codex",
    "Gemini CLI",
    "opencode",
    "MCP",
    "PR",
    "CI",
    "GitHub",
    "cargo",
    "nextest",
    "clippy",
    "rustfmt",
    "Lua",
    "ratatui",
    "kubectl",
    "pnpm",
    "npx",
];

/// Lines of the agent's screen to include. Enough for the last exchange,
/// little enough that the cleanup stays fast.
const SCREEN_LINES: usize = 60;
const SCREEN_CHARS: usize = 4000;

#[derive(Default)]
pub struct Context {
    /// The coding agent the text is for, when known — what `auto` follows.
    pub agent: Option<String>,
    /// For the cleanup model.
    pub text: String,
    /// For Whisper's `initial_prompt`: the same words, comma-separated.
    pub vocabulary: String,
}

/// Build the context for a dictation into `session` (when there is one).
/// Every lookup is best-effort: a Task Fleet cannot describe, or a session
/// whose screen is not capturable here, just contributes less.
pub fn build(glossary: &[String], session: Option<&fleet::Session>) -> Context {
    let mut words: Vec<String> = BUILTIN.iter().map(|w| w.to_string()).collect();
    words.extend(glossary.iter().cloned());

    let mut text = String::new();
    let mut agent = None;
    if let Some(session) = session {
        let status = &session.task_json["status"];
        // The harness (`claude-coder`, `codex`, …) names the agent's family.
        agent = status["execution"]["harness"]
            .as_str()
            .or_else(|| session.task_json["spec"]["harness"].as_str())
            .map(str::to_string);
        text.push_str("Dictating to a coding-agent session in thurbox.\n");
        text.push_str(&format!("Session: {}\n", session.task));
        words.push(session.task.clone());
        if let Some(agent) = &agent {
            text.push_str(&format!("Agent: {agent}\n"));
        }
        for dir in status["workspace"]["dirs"].as_array().into_iter().flatten() {
            let Some(dir) = dir.as_str() else { continue };
            let repo = dir.rsplit('/').next().unwrap_or(dir).to_string();
            match branch(dir) {
                Some(branch) => {
                    text.push_str(&format!("Repo: {repo} (branch {branch})\n"));
                    words.push(branch);
                }
                None => text.push_str(&format!("Repo: {repo}\n")),
            }
            words.push(repo);
        }
        if let Some(raw) = session.screen(SCREEN_LINES) {
            text.push_str("\nThe agent's screen, most recent last:\n");
            text.push_str(&tail(&raw, SCREEN_LINES, SCREEN_CHARS));
            text.push('\n');
        }
    }

    words.retain(|w| !w.trim().is_empty());
    words.dedup();
    let vocabulary = words.join(", ");
    let text = format!("Vocabulary likely in play: {vocabulary}\n\n{text}");
    Context {
        agent,
        text,
        vocabulary,
    }
}

fn branch(dir: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", dir, "branch", "--show-current"])
        .output()
        .ok()?;
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !branch.is_empty()).then_some(branch)
}

fn tail(raw: &str, lines: usize, chars: usize) -> String {
    let kept: Vec<&str> = raw
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    let start = kept.len().saturating_sub(lines);
    let mut out = kept[start..].join("\n");
    if out.len() > chars {
        let cut = out.len() - chars;
        let cut = (cut..out.len())
            .find(|&i| out.is_char_boundary(i))
            .unwrap_or(out.len());
        out = out[cut..].to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thurbox_is_always_in_the_vocabulary() {
        let context = build(&["Spotpay".to_string()], None);
        assert!(context.vocabulary.starts_with("thurbox, "));
        assert!(context.vocabulary.contains("Spotpay"));
        assert!(context.text.contains("thurbox"));
        assert!(context.agent.is_none());
    }

    #[test]
    fn a_screen_keeps_its_last_non_blank_lines() {
        let raw = "a\n   \n\nb   \n\nc\n";
        assert_eq!(tail(raw, 2, 100), "b\nc");
        assert_eq!(tail(raw, 10, 3), "b\nc");
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        let out = tail("ééééé", 1, 3);
        assert!(out.len() <= 4 && out.chars().all(|c| c == 'é'));
    }
}
