//! What the cleanup model is told about where the dictation is going.
//!
//! Three layers, most general first: words that are always in play when
//! talking to thurbox (so "toolbox" becomes "thurbox"), the user's own
//! glossary, and the target session — its name, repo, branch, agent, and the
//! last of what is on its screen. Names that appear here are very likely what
//! was said, which is the whole trick.

use std::process::Command;

use serde_json::Value;

/// Always in play when dictating to a coding agent inside thurbox. Product and
/// tool names a speech model has rarely or never heard.
const BUILTIN: &[&str] = &[
    "thurbox",
    "thurbox-cli",
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
/// Every lookup is best-effort: a session thurbox cannot describe, or a remote
/// one whose screen is not capturable here, just contributes less.
pub fn build(glossary: &[String], session: Option<&str>) -> Context {
    let mut words: Vec<String> = BUILTIN.iter().map(|w| w.to_string()).collect();
    words.extend(glossary.iter().cloned());

    let mut text = String::new();
    let mut agent = None;
    if let Some(id) = session {
        if let Some(info) = thurbox_json(&["session", "get", id, "--json", "--no-verify"]) {
            let field = |k: &str| info[k].as_str().map(str::to_string);
            agent = field("agent");
            text.push_str("Dictating to a coding-agent session in thurbox.\n");
            if let Some(name) = field("name") {
                text.push_str(&format!("Session: {name}\n"));
                words.push(name);
            }
            if let Some(agent) = &agent {
                text.push_str(&format!("Agent: {agent}\n"));
            }
            for tree in info["worktrees"].as_array().into_iter().flatten() {
                let repo = tree["repo_path"].as_str().unwrap_or_default();
                let repo_name = repo.rsplit('/').next().unwrap_or(repo);
                let branch = tree["branch"].as_str().unwrap_or_default();
                text.push_str(&format!("Repo: {repo_name} (branch {branch})\n"));
                words.push(repo_name.to_string());
                words.push(branch.to_string());
            }
        }
        if let Some(screen) = screen(id) {
            text.push_str("\nThe agent's screen, most recent last:\n");
            text.push_str(&screen);
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

fn thurbox_json(args: &[&str]) -> Option<Value> {
    let output = Command::new("thurbox-cli").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// The last non-blank lines of the session's screen. A TUI pads its screen
/// with blank and space-only lines; they are noise to the model.
fn screen(id: &str) -> Option<String> {
    let capture = thurbox_json(&[
        "session",
        "capture",
        id,
        "--lines",
        &SCREEN_LINES.to_string(),
        "--json",
    ])?;
    let raw = capture["output"].as_str()?;
    Some(tail(raw, SCREEN_LINES, SCREEN_CHARS))
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
