//! Parse Claude Code's on-screen dialogs (permission prompts, questions)
//! from `tmux capture-pane` text, and pick the option an approve/deny maps to.
//!
//! The registry only says `waiting`; the option labels exist only on screen,
//! so this is necessarily a screen-scrape. It is deliberately conservative:
//! when the expected option is not found, ccctl refuses instead of guessing.

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PromptOption {
    pub index: usize,
    pub label: String,
    pub selected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Prompt {
    /// "permission" (Yes/No style tool approval) or "question" (anything else
    /// with numbered options, e.g. AskUserQuestion, held-message dialog).
    pub kind: String,
    pub question: Option<String>,
    pub options: Vec<PromptOption>,
    /// Up to ~12 non-empty lines above the question (tool name, command...).
    pub context: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Once,
    Session,
    Always,
    Deny,
}

const BOX: &[char] = &[
    '│', '┃', '║', '╭', '╮', '╰', '╯', '─', '━', '┌', '┐', '└', '┘', '|',
];
const CURSORS: &[char] = &['❯', '›', '>', '▶', '→'];

fn clean(line: &str) -> String {
    line.trim()
        .trim_matches(|c| BOX.contains(&c))
        .trim()
        .to_string()
}

/// Parse `"❯ 1. Yes"` / `"  2. No, and tell Claude..."` into (index, label, selected).
fn parse_option(line: &str) -> Option<(usize, String, bool)> {
    let mut s = line;
    let mut selected = false;
    if let Some(c) = s.chars().next()
        && CURSORS.contains(&c)
    {
        selected = true;
        s = s[c.len_utf8()..].trim_start();
    }
    let dot = s.find('.')?;
    let num: usize = s[..dot].trim().parse().ok()?;
    if num == 0 || num > 20 || dot > 3 {
        return None;
    }
    let label = s[dot + 1..].trim();
    if label.is_empty() {
        return None;
    }
    Some((num, label.to_string(), selected))
}

/// Find the bottom-most contiguous numbered option list on screen.
pub fn parse_prompt(screen: &str) -> Option<Prompt> {
    let lines: Vec<String> = screen.lines().map(clean).collect();
    // Scan upward for the last option line, then grow the block upward.
    let last = lines.iter().rposition(|l| parse_option(l).is_some())?;
    let mut first = last;
    let mut i = last;
    while i > 0 {
        i -= 1;
        let l = &lines[i];
        if parse_option(l).is_some() {
            first = i;
            continue;
        }
        // A wrapped label continues on the line right below its option line.
        if !l.is_empty() && !l.ends_with('?') && i > 0 && parse_option(&lines[i - 1]).is_some() {
            continue;
        }
        break;
    }
    let mut options = Vec::new();
    for l in &lines[first..=last] {
        if let Some((n, label, sel)) = parse_option(l) {
            options.push(PromptOption {
                index: n,
                label,
                selected: sel,
            });
        } else if let Some(prev) = options.last_mut()
            && !l.is_empty()
        {
            prev.label.push(' ');
            prev.label.push_str(l);
        }
    }
    // Options must be 1..=k in order, else it's probably a numbered list in prose.
    if options.is_empty() || options.iter().enumerate().any(|(k, o)| o.index != k + 1) {
        return None;
    }
    let qpos = lines[..first].iter().rposition(|l| !l.is_empty());
    let question = qpos.map(|p| lines[p].clone()).filter(|q| q.ends_with('?'));
    let ctx_end = if question.is_some() {
        qpos.unwrap()
    } else {
        first
    };
    let context: Vec<String> = lines[..ctx_end]
        .iter()
        .rev()
        .filter(|l| !l.is_empty())
        .take(12)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let lower = |s: &str| s.to_lowercase();
    let has_yes = options.iter().any(|o| lower(&o.label).starts_with("yes"));
    let has_no = options.iter().any(|o| lower(&o.label).starts_with("no"));
    let kind = if has_yes && has_no {
        "permission"
    } else {
        "question"
    };
    Some(Prompt {
        kind: kind.into(),
        question,
        options,
        context,
    })
}

impl Prompt {
    pub fn selected_index(&self) -> usize {
        self.options
            .iter()
            .find(|o| o.selected)
            .map(|o| o.index)
            .unwrap_or(1)
    }

    /// Pick the option matching `choice`, by label.
    pub fn pick(&self, choice: Choice) -> Option<&PromptOption> {
        let l = |o: &&PromptOption| o.label.to_lowercase();
        let yes = |o: &&PromptOption| l(o).starts_with("yes");
        match choice {
            Choice::Once => self.options.iter().find(|o| {
                let s = l(o);
                yes(o)
                    && !s.contains("don't ask")
                    && !s.contains("always")
                    && !s.contains("session")
                    && !s.contains(", and")
            }),
            Choice::Session => self
                .options
                .iter()
                .find(|o| yes(o) && l(o).contains("session") && !l(o).contains("don't ask")),
            Choice::Always => self.options.iter().find(|o| {
                let s = l(o);
                yes(o)
                    && (s.contains("don't ask again")
                        || s.contains("always allow")
                        || s.contains("always"))
            }),
            Choice::Deny => self.options.iter().find(|o| l(o).starts_with("no")),
        }
    }
}

/// Keys that move the selection from `from` to `to` and confirm.
pub fn nav_keys(from: usize, to: usize) -> Vec<String> {
    let mut k = Vec::new();
    if to > from {
        k.extend(std::iter::repeat_n("Down".to_string(), to - from));
    } else {
        k.extend(std::iter::repeat_n("Up".to_string(), from - to));
    }
    k.push("Enter".into());
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASH: &str = "\
 some output above
╭──────────────────────────────────────────────────╮
│ Bash command                                     │
│                                                  │
│   rm -rf build/                                  │
│   Clean build dir                                │
│                                                  │
│ Do you want to proceed?                          │
│ ❯ 1. Yes                                         │
│   2. Yes, and don't ask again for rm commands in │
│ /home/x/proj                                     │
│   3. No, and tell Claude what to do differently  │
│ (esc)                                            │
╰──────────────────────────────────────────────────╯
";

    #[test]
    fn parses_bash_permission() {
        let p = parse_prompt(BASH).unwrap();
        assert_eq!(p.kind, "permission");
        assert_eq!(p.question.as_deref(), Some("Do you want to proceed?"));
        assert_eq!(p.options.len(), 3);
        assert!(p.options[0].selected);
        assert_eq!(
            p.options[1].label,
            "Yes, and don't ask again for rm commands in /home/x/proj"
        );
        assert!(p.context.iter().any(|c| c.contains("rm -rf build/")));
        assert_eq!(p.pick(Choice::Once).unwrap().index, 1);
        assert_eq!(p.pick(Choice::Always).unwrap().index, 2);
        assert!(p.pick(Choice::Session).is_none());
        assert_eq!(p.pick(Choice::Deny).unwrap().index, 3);
    }

    const EDIT: &str = "\
 Edit file
 src/main.rs
 Do you want to make this edit to main.rs?
   1. Yes
 ❯ 2. Yes, allow all edits during this session (shift+tab)
   3. No, and tell Claude what to do differently (esc)
";

    #[test]
    fn parses_unboxed_session_option() {
        let p = parse_prompt(EDIT).unwrap();
        assert_eq!(p.selected_index(), 2);
        assert_eq!(p.pick(Choice::Session).unwrap().index, 2);
        assert_eq!(p.pick(Choice::Once).unwrap().index, 1);
        assert!(p.pick(Choice::Always).is_none());
    }

    #[test]
    fn question_kind_and_rejects_prose_lists() {
        let q = "Which DB?\n❯ 1. Postgres\n  2. SQLite\n";
        assert_eq!(parse_prompt(q).unwrap().kind, "question");
        assert!(parse_prompt("Steps:\n2. foo\n3. bar\n").is_none());
        assert!(parse_prompt("nothing here\n> ").is_none());
    }

    #[test]
    fn nav() {
        assert_eq!(nav_keys(1, 3), vec!["Down", "Down", "Enter"]);
        assert_eq!(nav_keys(2, 1), vec!["Up", "Enter"]);
        assert_eq!(nav_keys(1, 1), vec!["Enter"]);
    }
}
