//! Read Claude Code's on-screen dialogs (tool permission prompts,
//! AskUserQuestion questions, other selects) from `tmux capture-pane` text,
//! and pick the option an approve/deny/choose maps to.
//!
//! The registry only says `waiting` (plus a coarse `waitingFor`); option
//! labels exist only on screen, so this is necessarily a screen read. It is
//! deliberately conservative: when the expected option is not found, ccctl
//! refuses instead of guessing, and always hands back the raw screen so a
//! person (or script) can see what is being asked.
//!
//! Layouts handled (Claude Code 2.1.3xx):
//!   * boxed or unboxed `❯ 1. Yes / 2. Yes, and … / 3. No, …` permission menus
//!   * labels wrapped over several lines (narrow panes, long paths/commands)
//!   * AskUserQuestion: option + description line(s), a `────` rule before
//!     "Chat about this", tab header, footer hints, checkboxes
//!   * menus drawn without indexes (`hideIndexes`), cursor + aligned siblings
//!   * "This terminal is too short…" notices (reported, never acted on)

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PromptOption {
    /// 1-based position in the menu (what the arrow navigation counts).
    pub index: usize,
    pub label: String,
    /// Description / extra lines drawn under the label (AskUserQuestion).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub selected: bool,
}

impl PromptOption {
    /// Label plus detail, lowercased, for matching.
    fn haystack(&self) -> String {
        match &self.detail {
            Some(d) => format!("{} {}", self.label, d).to_lowercase(),
            None => self.label.to_lowercase(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Prompt {
    /// "permission" (Yes…/No… options) or "question" (any other menu).
    pub kind: String,
    pub question: Option<String>,
    pub options: Vec<PromptOption>,
    /// Up to 12 non-empty lines above the question (tool name, command...).
    pub context: Vec<String>,
    /// False for menus drawn without `1.`/`2.` indexes.
    pub indexed: bool,
}

/// Everything ccctl knows about what a pane is showing.
#[derive(Debug, Clone, Serialize)]
pub struct Inspection {
    pub prompt: Option<Prompt>,
    /// A blocking notice Claude drew instead of (or next to) the options,
    /// e.g. "This terminal is too short. Make it taller to choose Yes…".
    pub notice: Option<String>,
    /// Bottom of the screen, verbatim (right-trimmed), for humans/scripts.
    pub screen: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Once,
    Session,
    Always,
    Deny,
}

/// Lines of screen tail returned in `Inspection.screen`.
pub const SCREEN_TAIL_LINES: usize = 40;
/// Max lines allowed between two consecutive options (wraps + descriptions).
const MAX_GAP: usize = 8;
/// Max non-empty lines allowed below the last option (footer hints, status
/// line). More than this and the list is prose above some other UI.
const MAX_TRAIL: usize = 10;

const VERTICALS: &[char] = &['│', '┃', '║', '|', '╎', '┆'];
const CORNERS: &[char] = &['╭', '╮', '╰', '╯', '┌', '┐', '└', '┘', '├', '┤'];
const RULES: &[char] = &['─', '━', '═', '╌', '┄', '-', '—', '▔', '▁'];
const CURSORS: &[char] = &['❯', '›', '>', '▶', '→', '▸', '➤'];
const CHECKBOXES: &[&str] = &[
    "[ ]", "[x]", "[X]", "[✔]", "[✓]", "☐", "☒", "☑", "◯", "◉", "○", "●", "✔", "✓",
];
const NOTICE_MARKERS: &[&str] = &[
    "terminal is too short",
    "make the terminal taller",
    "until the first lines are back",
];

#[derive(Debug, Clone)]
struct Line {
    /// Box drawing removed, trimmed.
    text: String,
    /// Line is only a horizontal rule / box edge.
    rule: bool,
    /// Column of the first visible character after box verticals → spaces.
    indent: usize,
}

fn line_of(raw: &str) -> Line {
    let spaced: String = raw
        .chars()
        .map(|c| {
            if VERTICALS.contains(&c) || CORNERS.contains(&c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let text = spaced.trim().to_string();
    // A rule is all rule chars, or a titled rule such as Claude's prompt box
    // border `──── session-name ─` (starts with a run of box-drawing dashes).
    let rule = !text.is_empty()
        && (text.chars().all(|c| RULES.contains(&c) || c == ' ')
            || text
                .chars()
                .take(3)
                .filter(|c| "─━═╌┄".contains(*c))
                .count()
                == 3);
    let indent = spaced.chars().take_while(|c| *c == ' ').count();
    Line {
        text: if rule { String::new() } else { text },
        rule,
        indent,
    }
}

/// `"❯ 1. Yes"`, `"  2) No"`, `"3. [ ] Option"` -> (index, label, selected).
fn parse_option(text: &str) -> Option<(usize, String, bool)> {
    let (selected, s) = strip_cursor(text);
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let num: usize = digits.parse().ok()?;
    let rest = &s[digits.len()..];
    let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?;
    if !rest.starts_with(' ') || num == 0 {
        return None;
    }
    let label = strip_checkbox(rest.trim());
    if label.is_empty() {
        return None;
    }
    Some((num, label.to_string(), selected))
}

fn strip_cursor(text: &str) -> (bool, &str) {
    match text.chars().next() {
        Some(c) if CURSORS.contains(&c) => (true, text[c.len_utf8()..].trim_start()),
        _ => (false, text),
    }
}

fn strip_checkbox(s: &str) -> &str {
    for b in CHECKBOXES {
        if let Some(r) = s.strip_prefix(b) {
            return r.trim_start();
        }
    }
    s
}

fn is_footer(t: &str) -> bool {
    let l = t.to_lowercase();
    t.contains(" · ")
        || l.starts_with("esc ")
        || l.starts_with("enter ")
        || l.starts_with("tab")
        || l.starts_with("ctrl+")
        || l.starts_with("press ")
        || l.contains("to navigate")
        || l.contains("to cancel")
}

/// Does `next` continue a wrapped label rather than start a description?
fn is_wrap(prev: &str, next: &str) -> bool {
    let ends_open = !prev.ends_with(['.', '!', '?', ')', ':']);
    let starts_low = next.chars().next().is_some_and(|c| {
        c.is_lowercase() || c == '/' || c == '(' || c == '~' || c.is_ascii_digit()
    });
    ends_open && starts_low
}

/// Inspect a captured screen: parsed dialog (if any), notices, raw tail.
pub fn inspect(screen: &str) -> Inspection {
    let lower = screen.to_lowercase();
    let notice = screen
        .lines()
        .map(|l| line_of(l).text)
        .find(|t| {
            let tl = t.to_lowercase();
            NOTICE_MARKERS.iter().any(|m| tl.contains(m))
        })
        .filter(|_| NOTICE_MARKERS.iter().any(|m| lower.contains(m)));
    Inspection {
        prompt: parse_prompt(screen),
        notice,
        screen: screen_tail(screen, SCREEN_TAIL_LINES),
    }
}

/// Last `n` lines of the screen, trailing blank lines dropped, right-trimmed.
pub fn screen_tail(screen: &str, n: usize) -> String {
    let lines: Vec<&str> = screen.lines().map(str::trim_end).collect();
    let end = lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map_or(0, |p| p + 1);
    let start = end.saturating_sub(n);
    lines[start..end].join("\n")
}

/// Find the bottom-most option menu on screen.
pub fn parse_prompt(screen: &str) -> Option<Prompt> {
    let lines: Vec<Line> = screen.lines().map(line_of).collect();
    parse_indexed(&lines).or_else(|| parse_unindexed(&lines))
}

/// The menu must be the last thing on screen: few lines below it, and no
/// prompt input box (a cursor line right under a rule). Claude hides the
/// input box while a dialog is open, so a list above one is prose.
fn trailing_ok(lines: &[Line], last: usize) -> bool {
    let below = &lines[last + 1..];
    let input_box = below
        .windows(2)
        .any(|w| w[0].rule && strip_cursor(&w[1].text).0);
    !input_box && below.iter().filter(|l| !l.text.is_empty()).count() <= MAX_TRAIL
}

fn parse_indexed(lines: &[Line]) -> Option<Prompt> {
    let opts: Vec<(usize, usize, String, bool)> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| parse_option(&l.text).map(|(n, lab, sel)| (i, n, lab, sel)))
        .collect();
    // Try each option line as the bottom of a 1..=k chain, bottom-most first.
    for end in (0..opts.len()).rev() {
        let (end_line, k, _, _) = opts[end];
        if k < 2 || !trailing_ok(lines, end_line) {
            continue;
        }
        let mut chain = vec![end];
        let mut want = k - 1;
        let mut j = end;
        while want >= 1 && j > 0 {
            j -= 1;
            let (li, n, _, _) = opts[j];
            let (prev_li, _, _, _) = opts[*chain.last().unwrap()];
            if prev_li - li > MAX_GAP + 1 {
                break;
            }
            if n == want {
                chain.push(j);
                want -= 1;
            } else {
                break; // a stray number inside the block: not a menu
            }
        }
        if want != 0 {
            continue;
        }
        chain.reverse();
        let mut options = Vec::new();
        for (pos, &oi) in chain.iter().enumerate() {
            let (li, n, ref label, sel) = opts[oi];
            let stop = match chain.get(pos + 1) {
                Some(&next) => opts[next].0,
                None => trailing_block_end(lines, li),
            };
            let (label, detail) = split_label(label, &lines[li + 1..stop]);
            options.push(PromptOption {
                index: n,
                label,
                detail,
                selected: sel,
            });
        }
        // A live select always highlights exactly one option with the cursor;
        // numbered lists in Claude's replies never do.
        if options.iter().filter(|o| o.selected).count() != 1 {
            continue;
        }
        let first_line = opts[chain[0]].0;
        return Some(finish(lines, first_line, options, true));
    }
    None
}

/// After the last option, take directly-following non-footer lines (wrapped
/// tail such as "(esc)" or a description), stopping at blank/rule/footer.
fn trailing_block_end(lines: &[Line], li: usize) -> usize {
    let mut e = li + 1;
    while e < lines.len() && e <= li + 3 {
        let l = &lines[e];
        if l.text.is_empty() || l.rule || is_footer(&l.text) || parse_option(&l.text).is_some() {
            break;
        }
        e += 1;
    }
    e
}

/// Join wrapped label lines; everything else becomes the detail.
fn split_label(first: &str, extra: &[Line]) -> (String, Option<String>) {
    let mut label = first.to_string();
    let mut detail: Vec<String> = Vec::new();
    for l in extra {
        if l.text.is_empty() || l.rule || is_footer(&l.text) {
            continue;
        }
        if detail.is_empty() && is_wrap(&label, &l.text) {
            label.push(' ');
            label.push_str(&l.text);
        } else {
            detail.push(l.text.clone());
        }
    }
    let detail = (!detail.is_empty()).then(|| detail.join(" "));
    (label, detail)
}

/// Menus without indexes: a cursor line plus siblings at the same text column.
fn parse_unindexed(lines: &[Line]) -> Option<Prompt> {
    let ci = lines.iter().rposition(|l| {
        let (sel, rest) = strip_cursor(&l.text);
        sel && !rest.is_empty() && parse_option(&l.text).is_none()
    })?;
    let cur = &lines[ci];
    let cursor_len = cur.text.chars().next()?.len_utf8();
    let after = cur.text[cursor_len..].len() - cur.text[cursor_len..].trim_start().len();
    let col = cur.indent + 1 + after; // column where the label text starts
    let sibling =
        |l: &Line| !l.text.is_empty() && !l.rule && l.indent == col && !is_footer(&l.text);
    let mut first = ci;
    while first > 0 && sibling(&lines[first - 1]) {
        first -= 1;
    }
    let mut last = ci;
    while last + 1 < lines.len() && sibling(&lines[last + 1]) {
        last += 1;
    }
    let n = last - first + 1;
    if !(2..=9).contains(&n) || !trailing_ok(lines, last) {
        return None;
    }
    let options: Vec<PromptOption> = (first..=last)
        .enumerate()
        .map(|(k, i)| {
            let (sel, t) = strip_cursor(&lines[i].text);
            PromptOption {
                index: k + 1,
                label: strip_checkbox(t).to_string(),
                detail: None,
                selected: sel,
            }
        })
        .collect();
    if options.iter().any(|o| o.label.chars().count() > 160) {
        return None;
    }
    Some(finish(lines, first, options, false))
}

fn finish(lines: &[Line], first_line: usize, options: Vec<PromptOption>, indexed: bool) -> Prompt {
    // Question: nearest non-empty, non-rule line above the menu (within 6).
    let qpos = (first_line.saturating_sub(6)..first_line)
        .rev()
        .find(|&i| !lines[i].text.is_empty());
    let question = qpos
        .map(|p| lines[p].text.clone())
        .filter(|q| q.ends_with('?'));
    let ctx_end = if question.is_some() {
        qpos.unwrap()
    } else {
        first_line
    };
    let mut context: Vec<String> = lines[..ctx_end]
        .iter()
        .rev()
        .filter(|l| !l.text.is_empty())
        .take(12)
        .map(|l| l.text.clone())
        .collect();
    context.reverse();
    let starts = |o: &PromptOption, p: &str| o.label.to_lowercase().starts_with(p);
    let has_yes = options.iter().any(|o| starts(o, "yes"));
    let has_no = options.iter().any(|o| starts(o, "no"));
    let kind = if has_yes && has_no {
        "permission"
    } else {
        "question"
    };
    Prompt {
        kind: kind.into(),
        question,
        options,
        context,
        indexed,
    }
}

impl Prompt {
    pub fn selected_index(&self) -> usize {
        self.options
            .iter()
            .find(|o| o.selected)
            .map(|o| o.index)
            .unwrap_or(1)
    }

    /// Pick the permission option matching `choice`, by label.
    pub fn pick(&self, choice: Choice) -> Option<&PromptOption> {
        let yes = |o: &&PromptOption| o.label.to_lowercase().starts_with("yes");
        match choice {
            Choice::Once => self.options.iter().find(|o| {
                let s = o.haystack();
                yes(o)
                    && !s.contains("don't ask")
                    && !s.contains("always")
                    && !s.contains("session")
                    && !s.contains(", and")
            }),
            Choice::Session => self.options.iter().find(|o| {
                let s = o.haystack();
                yes(o) && s.contains("session") && !s.contains("don't ask")
            }),
            Choice::Always => self.options.iter().find(|o| {
                let s = o.haystack();
                yes(o) && (s.contains("don't ask again") || s.contains("always"))
            }),
            Choice::Deny => self
                .options
                .iter()
                .find(|o| o.label.to_lowercase().starts_with("no")),
        }
    }

    /// Resolve a `choose` argument: an option number, or a case-insensitive
    /// label that is an exact match, else a unique prefix, else a unique
    /// substring.
    pub fn find(&self, sel: &str) -> Result<&PromptOption, String> {
        let s = sel.trim();
        if let Ok(n) = s.parse::<usize>() {
            return self
                .options
                .iter()
                .find(|o| o.index == n)
                .ok_or_else(|| format!("no option {n} (menu has {})", self.options.len()));
        }
        let l = s.to_lowercase();
        let label = |o: &&PromptOption| o.label.to_lowercase();
        for pass in 0..3 {
            let hits: Vec<&PromptOption> = self
                .options
                .iter()
                .filter(|o| match pass {
                    0 => label(o) == l,
                    1 => label(o).starts_with(&l),
                    _ => label(o).contains(&l),
                })
                .collect();
            match hits.len() {
                0 => continue,
                1 => return Ok(hits[0]),
                _ => {
                    return Err(format!(
                        "'{sel}' matches {} options: {}",
                        hits.len(),
                        hits.iter()
                            .map(|o| format!("{}. {}", o.index, o.label))
                            .collect::<Vec<_>>()
                            .join(" | ")
                    ));
                }
            }
        }
        Err(format!("no option matches '{sel}'"))
    }
}

/// Map Claude Code's `waitingFor` onto ccctl's dialog kinds.
/// 2.1.328: unlisted dialog kinds default to "permission prompt" (tool
/// permissions); "input needed" = AskUserQuestion / MCP elicitation;
/// "dialog open" = other modal dialogs.
pub fn kind_from_waiting_for(w: &str) -> String {
    let l = w.to_lowercase();
    if l.contains("permission") {
        "permission".into()
    } else if l == "input needed" {
        "question".into()
    } else if l == "dialog open" {
        "dialog".into()
    } else {
        w.to_string()
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

    fn fixture(name: &str) -> String {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/dialogs")
            .join(name);
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    #[test]
    fn bash_permission_boxed() {
        let p = parse_prompt(&fixture("bash-boxed.txt")).unwrap();
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

    #[test]
    fn edit_permission_session_option() {
        let p = parse_prompt(&fixture("edit-unboxed.txt")).unwrap();
        assert_eq!(p.selected_index(), 2);
        assert_eq!(p.pick(Choice::Session).unwrap().index, 2);
        assert_eq!(p.pick(Choice::Once).unwrap().index, 1);
        assert!(p.pick(Choice::Always).is_none());
    }

    /// Regression (Nov 2026): long `git push` in a narrow pane,
    /// labels wrapped over 3+ lines broke the old one-continuation rule.
    #[test]
    fn git_push_narrow_pane_wrapped_labels() {
        let p = parse_prompt(&fixture("git-push-narrow.txt")).unwrap();
        assert_eq!(p.kind, "permission");
        assert_eq!(p.options.len(), 3);
        assert!(
            p.options[1]
                .label
                .starts_with("Yes, and don't ask again for git push")
        );
        assert!(
            p.options[1].label.ends_with("/monorepo"),
            "{:?}",
            p.options[1]
        );
        assert!(p.options[2].label.starts_with("No, and tell Claude"));
        assert!(p.context.iter().any(|c| c.contains("git push")));
        assert_eq!(p.pick(Choice::Always).unwrap().index, 2);
        assert_eq!(p.pick(Choice::Deny).unwrap().index, 3);
    }

    /// Regression (Nov 2026): AskUserQuestion with
    /// descriptions, a rule before "Chat about this", tab header and footer.
    #[test]
    fn ask_user_question_with_descriptions() {
        let p = parse_prompt(&fixture("ask-user-question.txt")).unwrap();
        assert_eq!(p.kind, "question");
        assert_eq!(
            p.question.as_deref(),
            Some("Who should do the second fairness review?")
        );
        assert_eq!(p.options.len(), 4);
        assert_eq!(p.options[0].label, "Fresh subagent");
        assert_eq!(
            p.options[0].detail.as_deref(),
            Some("Spawn a new reviewer with no prior context")
        );
        assert_eq!(p.options[3].label, "Chat about this");
        assert!(p.options[0].selected);
        assert_eq!(p.find("2").unwrap().label, "Me (the main session)");
        assert_eq!(p.find("fresh").unwrap().index, 1);
        assert_eq!(p.find("type something").unwrap().index, 3);
        assert!(p.find("zzz").is_err());
        assert!(p.pick(Choice::Once).is_none());
    }

    #[test]
    fn subagent_task_permission() {
        let p = parse_prompt(&fixture("task-permission.txt")).unwrap();
        assert_eq!(p.kind, "permission");
        assert_eq!(p.options.len(), 2);
        assert_eq!(p.pick(Choice::Once).unwrap().index, 1);
        assert_eq!(p.pick(Choice::Deny).unwrap().index, 2);
    }

    #[test]
    fn unindexed_menu() {
        let p = parse_prompt(&fixture("unindexed.txt")).unwrap();
        assert!(!p.indexed);
        assert_eq!(p.options.len(), 3);
        assert_eq!(p.options[1].label, "No, keep the current mode");
        assert!(p.options[0].selected);
    }

    #[test]
    fn too_short_notice_is_reported() {
        let i = inspect(&fixture("too-short.txt"));
        assert!(i.notice.unwrap().contains("too short"));
    }

    #[test]
    fn unrecognized_dialog_keeps_screen() {
        let s = fixture("unrecognized.txt");
        let i = inspect(&s);
        assert!(i.prompt.is_none());
        assert!(i.screen.contains("Press Enter to continue"));
    }

    #[test]
    fn prose_lists_are_not_menus() {
        // numbered list in Claude's answer, input box below: not a dialog
        let s = "Plan:\n1. Refactor parser\n2. Add fixtures\n3. Ship\n\n".to_string()
            + &"filler line\n".repeat(12)
            + "────────\n❯ \n────────\n";
        assert!(parse_prompt(&s).is_none());
        assert!(parse_prompt("Steps:\n2. foo\n3. bar\n").is_none());
        // numbered reply right above the idle input box (live false positive)
        let idle = "Answers:\n  1. Grader: tests\n  2. Reward: 0 or 1\n\n✻ Baked for 22s\n\n\
                    ──── my-session ─\n❯\n────────\n  -- INSERT -- ⏵⏵ auto mode on\n";
        assert!(parse_prompt(idle).is_none());
        // past prompts + tool output above a titled input-box rule (live false positive)
        let hist = "❯ /color cyan\n  ⎿  Session color set to: cyan\n\n\
                    ──────────────── finishes-scaffolds ─\n❯\n────────────────\n  -- INSERT --\n";
        assert!(parse_prompt(hist).is_none());
        // no cursor on any option: not a live select
        assert!(parse_prompt("Pick?\n  1. A\n  2. B\n").is_none());
        assert!(parse_prompt("nothing here\n> ").is_none());
        let q = "Which DB?\n❯ 1. Postgres\n  2. SQLite\n";
        assert_eq!(parse_prompt(q).unwrap().options.len(), 2);
        assert_eq!(parse_prompt(q).unwrap().kind, "question");
    }

    #[test]
    fn tail_and_kinds() {
        assert_eq!(screen_tail("a\nb\nc\n\n\n", 2), "b\nc");
        assert_eq!(kind_from_waiting_for("permission prompt"), "permission");
        assert_eq!(kind_from_waiting_for("input needed"), "question");
        assert_eq!(kind_from_waiting_for("dialog open"), "dialog");
        assert_eq!(kind_from_waiting_for("sandbox request"), "sandbox request");
    }

    #[test]
    fn nav() {
        assert_eq!(nav_keys(1, 3), vec!["Down", "Down", "Enter"]);
        assert_eq!(nav_keys(2, 1), vec!["Up", "Enter"]);
        assert_eq!(nav_keys(1, 1), vec!["Enter"]);
    }
}
