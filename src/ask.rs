//! ask.rs — the human-in-the-loop layer (port of ask.py).
//!
//! Two kinds of asks: *judgment* (design forks, ambiguity, stuck — the model
//! calls ask_human) and *approval* (policy flagged a command; hard gate —
//! the command cannot run without consent). In H4 both prompt on the same
//! terminal via stdin/stdout; the journal events are the seam (the TUI
//! modals in H5 render the same events).

use serde_json::Value;

use super::journal::Journal;
use super::util::{mapof, mapof2};

fn read_line() -> String {
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    line.trim().to_string()
}

/// Interactive prompt with optional numbered choices. `answer` non-None =
/// non-interactive / auto-granted path (returns it without prompting, so
/// scripts never hang).
fn prompt(question: &str, options: Option<&[&str]>, answer: Option<String>) -> String {
    if let Some(a) = answer { return a }
    println!("\x1b[33m[curam:ask]\x1b[0m {}", question);
    match options {
        Some(opts) => {
            for (i, opt) in opts.iter().enumerate() {
                println!("  \x1b[36m{}\x1b[0m {}", i + 1, opt);
            }
            let raw = read_line();
            let n = str::parse::<i64>(raw.as_str()).unwrap_or(-1);
            if n >= 1 && n <= opts.len() as i64 {
                return opts[n as usize - 1].to_string();
            }
            if raw.is_empty() {
                "(no reply)".into()
            } else {
                raw
            }
        }
        None => {
            let raw = read_line();
            if raw.is_empty() { "(no reply)".into() } else { raw }
        }
    }
}

/// Judgment ask: `ask_human` tool calls. Logs `ask_human` + `human_answer`.
pub fn ask_judgment(
    journal: &mut Journal,
    question: &str,
    options: Option<&[&str]>,
    answer: Option<String>,
) -> Result<String, String> {
    let mut m = mapof("question", question.into());
    if let Some(o) = options {
        m.insert(
            "options".to_owned(),
            o.iter().map(|s| (*s).into()).collect::<Vec<Value>>().into(),
        );
    }
    journal.log("ask_human", &Value::Object(m))?;
    let ans = prompt(question, options, answer);
    journal.log("human_answer", &Value::Object(mapof("answer", ans.clone().into())))?;
    Ok(ans)
}

/// Hard approval gate for policy-flagged commands. Returns true when the
/// command may run. `grant` non-None = non-interactive / auto-granted path.
pub fn ask_approval(
    journal: &mut Journal,
    command: &str,
    reason: &str,
    grant: Option<bool>,
) -> Result<bool, String> {
    journal.log("approval_requested", &Value::Object(mapof2(
        "command", command.into(),
        "reason", reason.into(),
    )))?;
    let granted = match grant {
        Some(g) => g,
        None => {
            let q = format!("Run command? {reason}\n    $ {command}");
            let reply = prompt(q.as_str(), None, None);
            reply == "allow once" || reply == "1" || reply == "y" || reply == "yes"
        }
    };
    journal.log(
        if granted { "approval_granted" } else { "tool_denied" },
        &Value::Object(mapof2("command", command.into(), "reason", reason.into())),
    )?;
    Ok(granted)
}

/// The model looped on the same tool call — nudge the human (H4: journal +
/// prompt; the exact UX wires to the TUI modal in H5).
pub fn ask_stuck(
    journal: &mut Journal,
    attempts: &[Value],
    answer: Option<String>,
) -> Result<String, String> {
    journal.log("ask_human", &Value::Object(mapof2(
        "kind", "stuck".into(),
        "attempts", serde_json::Value::Array(attempts.to_vec()),
    )))?;
    let ans = prompt(
        "I've tried the same thing 3+ times. How should I proceed?",
        Some(&["show me the failing output and I'll decide", "change approach yourself", "stop and summarize"]),
        answer,
    );
    journal.log("human_answer", &Value::Object(mapof("answer", ans.clone().into())))?;
    Ok(ans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal_in(dir: &std::path::Path) -> Journal {
        Journal::open(dir.join("j.db"), Some("sess".into())).unwrap()
    }

    #[test]
    fn approval_grant_and_deny_journal() {
        let dir = std::env::temp_dir().join("curam-h4-a").join(format!("{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut j = journal_in(&dir);
        // grant path (auto_grant) logs approval_requested + approval_granted
        assert_eq!(ask_approval(&mut j, "ls", "always-ask", Some(true)).unwrap(), true);
        assert_eq!(j.events(Some("approval_requested")).unwrap().len(), 1);
        assert_eq!(j.events(Some("approval_granted")).unwrap().len(), 1);
        // deny path logs tool_denied instead
        assert_eq!(ask_approval(&mut j, "rm x", "always-ask", Some(false)).unwrap(), false);
        assert_eq!(j.events(Some("tool_denied")).unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}