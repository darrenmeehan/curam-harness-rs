//! repl.rs — the plain-ANSI REPL fallback (--no-tui / non-tty).
//!
//! A 1:1 port of the POC ui.py run_repl: coloured status line, `curam>`
//! prompt, /commands (/exit /sli /jsonl /verbose /quiet /help), tasks run
//! against ONE persistent agent so the conversation stays live, interim
//! thinking/commands printed as plain lines (native scrollback preserved).
//! The ratatui front end is exported as tui::run; this is the escape hatch.

use std::io::stdin;
use std::io::Write;

use super::agent::Agent;
use super::budget::Budget;
use super::config;
use super::journal::Journal;
use super::provider::Provider;
use super::sli;

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// The POC status line: provider · model · cost / cap · iterations.
fn status_line(a: &Agent, iters: usize) -> String {
    let spent = round2(a.spend());
    let cap_full = if a.cap() > 0.0 {
        format!("/${}", round2(a.cap()))
    } else {
        String::new()
    };
    let name: &str = a.provider_name();
    let model: &str = a.model();
    format!(
        "curam \x1b[36m{}\x1b[0m \x1b[36m{}\x1b[0m cost=\x1b[33m${}{}\x1b[0m iters={}",
        name, model, spent, cap_full, iters
    )
}

/// Run the REPL to its end (EOF, /exit, or Ctrl-C per the terminal).
pub fn run(
    cfg: config::Config,
    provider: Provider,
    budget: Budget,
    workdir: std::path::PathBuf,
    journal_path: std::path::PathBuf,
    quiet: bool,
) -> i32 {
    let mut j = match Journal::open(journal_path, None) {
        Err(e) => {
            eprintln!("curam repl: {e}");
            return 1;
        }
        Ok(x) => x,
    };
    let show_thinking = !quiet && cfg.ui.show_thinking;
    let show_commands = !quiet && cfg.ui.show_commands;
    let mut a = Agent::new(
        &cfg,
        provider,
        budget,
        workdir,
        true, // interactive: ask via the traditional prompt
        show_thinking,
        show_commands,
        false,
    );
    let mut iters = 0;

    println!("{}", status_line(&a, iters));
    println!("type a task, or /exit · /sli · /jsonl <path> · /verbose · /quiet · /help");

    loop {
        print!("\x1b[32mcuram>\x1b[0m ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        match stdin().read_line(&mut line) {
            Ok(0) => {
                println!();
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
        let t = line.as_str().trim();
        if t.is_empty() {
            continue;
        }
        match t {
            "/exit" | "/quit" | "/q" => break,
            "/sli" => {
                let evs = j.events(None).unwrap_or(vec![]);
                println!("{}", sli::summary(&evs));
                println!("{}", status_line(&a, iters));
                continue;
            }
            "/verbose" => {
                a.show_thinking = true;
                a.show_commands = true;
                println!("realtime view ON (thinking + commands)");
                println!("{}", status_line(&a, iters));
                continue;
            }
            "/quiet" => {
                a.show_thinking = false;
                a.show_commands = false;
                println!("realtime view OFF");
                println!("{}", status_line(&a, iters));
                continue;
            }
            "/help" => {
                println!("/exit · /sli · /jsonl <path> · /verbose · /quiet — or just type a task");
                println!("{}", status_line(&a, iters));
                continue;
            }
            _ => {
                if t.starts_with("/jsonl") {
                    let parts: Vec<&str> = t.split(' ').collect::<Vec<&str>>();
                    let target = if parts.len() > 1 {
                        parts[1].to_string()
                    } else {
                        "session.jsonl".into()
                    };
                    let out_fn = std::path::PathBuf::from(target.as_str());
                    match j.export_jsonl(Some(&out_fn)) {
                        Ok(out) => {
                            println!("journal exported → {out}");
                            println!("{}", status_line(&a, iters));
                        }
                        Err(e) => {
                            eprintln!("curam repl: jsonl export failed: {e}");
                        }
                    }
                    continue;
                }
                let task: String = t.to_string();
                println!();
                match a.run_task(task.as_str(), &mut j) {
                    Err(e) => {
                        eprintln!("curam: {e}");
                    }
                    Ok(done) => {
                        iters += 1;
                        if let Some(text) = done {
                            if text.len() > 400 {
                                println!("{}…", &text[0..400]);
                            } else {
                                println!("{}", text);
                            }
                        }
                    }
                }
                println!("{}", status_line(&a, iters));
            }
        }
    }
    0
}
