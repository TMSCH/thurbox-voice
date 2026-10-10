//! What the cleanup model is told about where the dictation is going.
//!
//! Four sources, each named in `[context] sources` and sent only when named:
//! words that are always in play when talking to thurbox plus the user's
//! glossary (so "toolbox" becomes "thurbox"), the target session's own
//! metadata, the last of what is on its screen, and the user's memory files.
//! Names that appear here are very likely what was said, which is the whole
//! trick.
//!
//! Only the session the dictation was started for is read, and only through
//! `thurbox-cli session get` and `session capture` — never another session,
//! a history, or the database. The whole is held to `max_chars`.

use std::process::Command;

use serde_json::Value;

use crate::config::{self, ContextConfig, Source};

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

/// One memory file's most. A notes file is meant to be a page of words, not a
/// document; past this it would crowd the screen out of the budget.
const MEMORY_FILE_CHARS: usize = 4000;

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
/// Every lookup is best-effort: a session thurbox cannot describe — deleted,
/// or on a host that is not answering — or a memory file that is gone just
/// contributes less.
pub fn build(config: &ContextConfig, session: Option<&str>) -> Context {
    let mut words: Vec<String> = Vec::new();
    if config.uses(Source::Glossary) {
        words.extend(BUILTIN.iter().map(|w| w.to_string()));
        words.extend(config.glossary.iter().cloned());
    }

    let mut about = String::new();
    let mut agent = None;
    // Asked even when the session is not sent: which agent it runs decides
    // which backend `auto` picks, and that stays on this machine.
    let info =
        session.and_then(|id| thurbox_json(&["session", "get", id, "--json", "--no-verify"]));
    if let Some(info) = &info {
        let field = |k: &str| info[k].as_str().map(str::to_string);
        agent = field("agent");
        if config.uses(Source::Session) {
            about.push_str("Dictating to a coding-agent session in thurbox.\n");
            if let Some(name) = field("name") {
                about.push_str(&format!("Session: {name}\n"));
                words.push(name);
            }
            if let Some(agent) = &agent {
                about.push_str(&format!("Agent: {agent}\n"));
            }
            for tree in info["worktrees"].as_array().into_iter().flatten() {
                let repo = tree["repo_path"].as_str().unwrap_or_default();
                let repo_name = repo.rsplit('/').next().unwrap_or(repo);
                let branch = tree["branch"].as_str().unwrap_or_default();
                about.push_str(&format!("Repo: {repo_name} (branch {branch})\n"));
                words.push(repo_name.to_string());
                words.push(branch.to_string());
            }
        }
    }

    let memory = if config.uses(Source::Memory) {
        memory(&config.memory_files)
    } else {
        String::new()
    };
    let screen = match session {
        Some(id) if config.uses(Source::Screen) => {
            cap_screen(screen(id, config.screen_lines), SCREEN_CHARS)
        }
        _ => Vec::new(),
    };

    words.retain(|w| !w.trim().is_empty());
    let mut seen = std::collections::HashSet::new();
    words.retain(|w| seen.insert(w.clone()));
    let vocabulary = words.join(", ");
    let mut head = String::new();
    if !vocabulary.is_empty() {
        head.push_str(&format!("Vocabulary likely in play: {vocabulary}\n"));
    }
    if !about.is_empty() {
        if !head.is_empty() {
            head.push('\n');
        }
        head.push_str(&about);
    }
    Context {
        agent,
        text: fit(head.trim_end(), &memory, &screen, config.max_chars),
        vocabulary,
    }
}

/// Every readable memory file, each under a line naming it and capped. A file
/// that cannot be read is said so on stderr, the daemon's log, and skipped.
fn memory(files: &[String]) -> String {
    let mut out = String::new();
    for file in files {
        let path = config::expand(file);
        match std::fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => {
                let text = text.trim();
                let kept: String = text.chars().take(MEMORY_FILE_CHARS).collect();
                out.push_str(&format!("From {file}:\n{kept}\n"));
                if kept.len() < text.len() {
                    out.push_str("[cut]\n");
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("[context] memory file {} skipped: {e}", path.display()),
        }
    }
    out
}

/// The most screen sent, whatever `max_chars` leaves room for: the cap a
/// dictation had before the budget existed, so an unchanged config sends no
/// more of a session's screen than it did.
const SCREEN_CHARS: usize = 4000;

/// The newest lines of `screen` that fit in `chars`.
fn cap_screen(mut screen: Vec<String>, chars: usize) -> Vec<String> {
    let size = |lines: &[String]| lines.iter().map(|l| l.chars().count() + 1).sum::<usize>();
    while size(&screen) > chars + 1 && !screen.is_empty() {
        screen.remove(0);
    }
    screen
}

const SCREEN_HEADING: &str = "The agent's screen, most recent last:";
const MEMORY_HEADING: &str = "Notes from the user's memory files:";

/// Join the sections within `max` characters. The screen gives way first,
/// oldest line first, then the end of the memory files, then — only for a
/// budget too small for the session itself — everything past `max`.
fn fit(head: &str, memory: &str, screen: &[String], max: usize) -> String {
    let compose = |memory: &str, screen: &[String]| {
        let mut parts: Vec<String> = Vec::new();
        if !head.is_empty() {
            parts.push(head.to_string());
        }
        if !memory.trim().is_empty() {
            parts.push(format!("{MEMORY_HEADING}\n{}", memory.trim_end()));
        }
        if !screen.is_empty() {
            parts.push(format!("{SCREEN_HEADING}\n{}", screen.join("\n")));
        }
        parts.join("\n\n")
    };
    let len = |s: &str| s.chars().count();

    let mut from = 0;
    let mut text = compose(memory, screen);
    while len(&text) > max && from < screen.len() {
        from += 1;
        text = compose(memory, &screen[from..]);
    }
    if len(&text) > max {
        let without = len(&compose("", &[]));
        // Room left for the memory section, its heading and separator.
        let room = max.saturating_sub(without + MEMORY_HEADING.len() + 3);
        let kept: String = memory.chars().take(room).collect();
        text = compose(&kept, &[]);
    }
    if len(&text) > max {
        text = text.chars().take(max).collect();
    }
    text
}

fn thurbox_json(args: &[&str]) -> Option<Value> {
    let output = Command::new("thurbox-cli").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// The last non-blank lines of the session's screen, oldest first. A TUI
/// pads its screen with blank and space-only lines; they are noise to the
/// model.
fn screen(id: &str, lines: usize) -> Vec<String> {
    let Some(capture) = thurbox_json(&[
        "session",
        "capture",
        id,
        "--lines",
        &lines.to_string(),
        "--json",
    ]) else {
        return Vec::new();
    };
    tail(capture["output"].as_str().unwrap_or_default(), lines)
}

fn tail(raw: &str, lines: usize) -> Vec<String> {
    let kept: Vec<String> = raw
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    let start = kept.len().saturating_sub(lines);
    kept[start..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thurbox_is_always_in_the_vocabulary() {
        let config = ContextConfig {
            glossary: vec!["Spotpay".to_string()],
            ..ContextConfig::default()
        };
        let context = build(&config, None);
        assert!(context.vocabulary.starts_with("thurbox, "));
        assert!(context.vocabulary.contains("Spotpay"));
        assert!(context.text.contains("thurbox"));
        assert!(context.agent.is_none());
    }

    #[test]
    fn the_screen_keeps_its_own_cap_inside_the_budget() {
        // What a config with no [context] section sent before sources existed:
        // never more than SCREEN_CHARS of screen, newest lines kept.
        let screen: Vec<String> = (0..60)
            .map(|i| format!("{i:03} {}", "x".repeat(146)))
            .collect();
        let text = fit("", "", &cap_screen(screen, SCREEN_CHARS), 8000);
        let shown = text.split_once('\n').unwrap().1;
        assert!(shown.chars().count() <= SCREEN_CHARS, "{}", shown.len());
        assert!(shown.ends_with(&"x".repeat(146)) && shown.contains("059 "));
        assert!(!shown.contains("000 "));
    }

    #[test]
    fn without_the_glossary_source_no_words_are_sent() {
        let config = ContextConfig {
            sources: vec![Source::Session],
            glossary: vec!["Spotpay".to_string()],
            ..ContextConfig::default()
        };
        let context = build(&config, None);
        assert!(context.vocabulary.is_empty());
        assert!(context.text.is_empty());
    }

    #[test]
    fn a_screen_keeps_its_last_non_blank_lines() {
        let raw = "a\n   \n\nb   \n\nc\n";
        assert_eq!(tail(raw, 2), vec!["b", "c"]);
        assert_eq!(tail(raw, 10), vec!["a", "b", "c"]);
    }

    #[test]
    fn the_budget_cuts_the_oldest_screen_lines_then_the_memory() {
        let screen: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
        let text = fit("Session: s", "notes", &screen, 120);
        assert!(text.chars().count() <= 120, "{text}");
        assert!(text.contains("line 49") && !text.contains("line 0\n"));
        assert!(text.contains("notes") && text.contains("Session: s"));

        let memory = "m".repeat(500);
        let text = fit("Session: s", &memory, &screen, 100);
        assert!(text.chars().count() <= 100, "{text}");
        assert!(!text.contains("line"), "the screen went first: {text}");
        assert!(text.starts_with("Session: s") && text.contains("mmm"));
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        let text = fit("ééééé", "", &[], 3);
        assert_eq!(text, "ééé");
    }
}
