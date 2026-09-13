//! tui.rs — the full-screen ratatui front end (H5).
//!
//! Layout: conversation (left) + status rail (right) on top, input line
//! below; asks replace the input area with a modal (y/n/a/Esc, or free
//! text). Ctrl-b toggles the block-debug view. The agent runs on a worker
//! thread fed by the ui::UiBus; Ctrl-C cancels the in-flight run
//! gracefully (journaled error + back to input), a second Ctrl-C within 1s
//! quits.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::prelude as rp;
use ratatui::prelude::Stylize;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::budget::Budget;
use super::config;
use super::journal::Journal;
use super::provider::Provider;
use super::ui::{UiBus, UiEv};
use ratatui::prelude::Line;

const RAIL_W: u16 = 26;

/// One visible conversation row (kind drives the colour + prefix).
#[derive(Debug)]
pub struct Row {
    pub kind: String,
    pub text: String,
}

#[derive(Debug)]
pub struct Status {
    pub provider: String,
    pub model: String,
    pub cost: f64,
    pub spent: f64,
    pub iterations: usize,
    pub cage: String,
    pub running: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct AskPrompt {
    pub question: String,
    pub answer: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    Input,
    Ask(AskPrompt),
    Quit,
}

#[derive(Debug)]
pub struct App {
    pub rows: Vec<Row>,
    pub status: Status,
    pub input: String,
    pub scroll: u16,
    pub mode: Mode,
    pub blocks: bool,
    pub enter_pending: bool,
    last_ctrl_c: Instant,
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

impl App {
    pub fn new(cfg: &config::Config, provider: &Provider, budget: &Budget) -> Self {
        let cage = if cfg.cage.fallback == "raw" {
            "RAW!".into()
        } else {
            "caged".into()
        };
        Self {
            rows: Vec::new(),
            status: Status {
                provider: provider.name.clone(),
                model: provider.model.clone(),
                cost: 0.0,
                spent: round2(budget.spent_usd),
                iterations: 0,
                cage,
                running: false,
            },
            input: String::new(),
            scroll: 0,
            mode: Mode::Input,
            blocks: false,
            enter_pending: false,
            last_ctrl_c: Instant::now(),
        }
    }

    pub fn push(&mut self, kind: &str, text: String) {
        self.rows.push(Row {
            kind: kind.into(),
            text,
        });
        self.scroll = 0; // follow the tail
    }

    /// Apply one agent event to the view state.
    pub fn apply(&mut self, ev: &UiEv) {
        match ev.kind.as_str() {
            "delta" => {
                // fold the token stream into one dim block: append to the
                // last row when it is already the open thinking block.
                if !self.rows.is_empty() {
                    let i = self.rows.len() - 1;
                    if self.rows[i].kind == "delta" {
                        let mut last_text: String = self.rows[i].text.clone();
                        last_text.push_str(ev.text.as_str());
                        self.rows[i] = Row {
                            kind: "delta".into(),
                            text: last_text,
                        };
                        return;
                    }
                }
                self.push("delta", ev.text.clone());
            }
            "tool" => self.push("tool", ev.text.clone()),
            "result" => self.push("result", ev.text.clone()),
            "status" => {
                let t = ev.text.as_str();
                // "cost=0.12 spent=1.34"
                if let Some(i) = t.find("spent=") {
                    let cost_s = t[5..i].trim();
                    let spent_s = t[i + 6..].trim();
                    self.status.cost = cost_s.parse::<f64>().unwrap_or(0.0);
                    self.status.spent = spent_s.parse::<f64>().unwrap_or(0.0);
                    self.status.iterations += 1;
                }
            }
            "ask" | "stuck" => {
                self.push(ev.kind.clone().as_str(), ev.text.clone());
                self.mode = Mode::Ask(AskPrompt {
                    question: ev.text.clone(),
                    answer: String::new(),
                });
            }
            "error" => self.push("error", ev.text.clone()),
            "answer" => self.push("answer", ev.text.clone()),
            "done" => {
                self.status.running = false;
                if !ev.text.is_empty() {
                    self.push("answer", ev.text.clone());
                }
            }
            _ => {}
        }
    }

    /// Cap the rendered inbox so a very long run cannot grow it forever.
    pub fn capped_rows(&self) -> Vec<&Row> {
        let n = self.rows.len();
        let start = n.saturating_sub(500);
        let mut out: Vec<&Row> = vec![];
        for i in start..n {
            out.push(&self.rows[i]);
        }
        out
    }

    /// Dispatch one key event. Returns the reply for the worker's ask
    /// channel, or None. Mutates the view (input, scroll, mode, cancel).
    pub fn on_key(
        &mut self,
        key: KeyCode,
        modifiers: KeyModifiers,
        cancel: &Arc<AtomicBool>,
    ) -> Option<String> {
        // Ctrl-C: first cancels the in-flight run (or arms the quit
        // window), the second within 1s quits.
        if modifiers.contains(KeyModifiers::CONTROL) && key == KeyCode::Char('c') {
            let now = Instant::now();
            if now - self.last_ctrl_c <= Duration::from_millis(1000) {
                self.mode = Mode::Quit;
            } else {
                self.last_ctrl_c = now;
                cancel.store(true, Ordering::Relaxed);
            }
            return None;
        }
        if modifiers.contains(KeyModifiers::CONTROL) && key == KeyCode::Char('b') {
            self.blocks = !self.blocks;
            return None;
        }
        match self.mode {
            Mode::Quit => None,
            Mode::Ask(_) => match key {
                KeyCode::Char(c) if c == 'y' => {
                    self.mode = Mode::Input;
                    Some("yes".into())
                }
                KeyCode::Char(c) if c == 'n' => {
                    self.mode = Mode::Input;
                    Some("no".into())
                }
                KeyCode::Char(c) if c == 'a' => {
                    self.mode = Mode::Input;
                    Some("allow".into())
                }
                KeyCode::Char(c) => {
                    let mut a: String = self.ask_answer();
                    a.push_str(c.to_string().as_str());
                    if let Mode::Ask(p) = &mut self.mode {
                        p.answer = a;
                    }
                    None
                }
                KeyCode::Enter => {
                    // typed free-text reply (or yes on empty → approval)
                    let answer = self.ask_answer();
                    self.mode = Mode::Input;
                    Some(if answer.is_empty() {
                        "yes".into()
                    } else {
                        answer
                    })
                }
                KeyCode::Backspace => {
                    if let Mode::Ask(p) = &mut self.mode {
                        let a = p.answer.clone();
                        if !a.is_empty() {
                            p.answer = a[0..a.len() - 1].to_string();
                        }
                    }
                    None
                }
                KeyCode::Esc => {
                    self.mode = Mode::Input;
                    Some("no".into())
                }
                _ => None,
            },
            Mode::Input => match key {
                KeyCode::Enter if !self.input.is_empty() && !self.status.running => {
                    self.enter_pending = true;
                    None
                }
                KeyCode::Char(c) => {
                    self.input.push_str(c.to_string().as_str());
                    None
                }
                KeyCode::Backspace => {
                    if !self.input.is_empty() {
                        self.input = self.input[0..self.input.len() - 1].to_string();
                    }
                    None
                }
                KeyCode::Esc => {
                    self.input = String::new();
                    None
                }
                KeyCode::PageUp => {
                    self.scroll += 4;
                    None
                }
                KeyCode::PageDown => {
                    self.scroll = self.scroll.saturating_sub(4);
                    None
                }
                _ => None,
            },
        }
    }

    fn ask_answer(&self) -> String {
        match &self.mode {
            Mode::Ask(p) => p.answer.clone(),
            _ => String::new(),
        }
    }

    /// Render the whole screen into the frame.
    pub fn render(&mut self, frame: &mut rp::Frame) {
        let area = frame.area();
        let layout = rp::Layout::vertical([rp::Constraint::Fill(1), rp::Constraint::Length(3)]);
        let areas = layout.split(area);
        let convo = areas[0];
        let bottom = areas[1];

        // bottom row: input prompt or ask modal
        let ask_mode = match &self.mode {
            Mode::Ask(p) => Some(p),
            _ => None,
        };
        if let Some(p) = ask_mode {
            let text = format!(
                "{}\n[y] allow  [n] deny  [a] allow-session  [Esc] deny  — reply: {}",
                p.question.clone(),
                p.answer.clone()
            );
            frame.render_widget(
                Paragraph::new(text.as_str())
                    .wrap(Wrap { trim: true })
                    .block(Block::new().borders(Borders::ALL).title(" curam ")),
                bottom,
            );
        } else {
            let prompt = if self.status.running {
                "[running — Ctrl-C to cancel]".into()
            } else {
                format!("curam> {}", self.input.clone())
            };
            frame.render_widget(
                Paragraph::new(prompt.as_str())
                    .wrap(Wrap { trim: true })
                    .block(Block::new().borders(Borders::ALL).title(" curam ")),
                bottom,
            );
        }

        // conversation + status rail
        let row_layout =
            rp::Layout::horizontal([rp::Constraint::Fill(1), rp::Constraint::Length(RAIL_W)]);
        let row_areas = row_layout.split(convo);
        let main_area = row_areas[0];
        let rail_area = row_areas[1];

        let lines: Vec<Line<'static>> = self
            .capped_rows()
            .iter()
            .map(|row| line_for(&row.kind, &row.text))
            .collect::<Vec<rp::Line>>();
        frame.render_widget(
            Paragraph::new(rp::Text::from(lines))
                .wrap(Wrap { trim: true })
                .scroll((self.scroll, 0)),
            main_area,
        );

        let rail = rail_lines(&self.status, self.blocks);
        frame.render_widget(
            Paragraph::new(rp::Text::from(rail))
                .block(Block::new().borders(Borders::ALL).title(" status ")),
            rail_area,
        );

        // cursor at the end of the input line
        if !self.status.running {
            let w = bottom.width.saturating_sub(2) as usize;
            let x = if 10 + self.input.len() < w {
                10 + self.input.len()
            } else {
                w
            };
            frame.set_cursor(x as u16, bottom.y + 1);
        }
    }
}

/// Style one transcript row by kind (the POC palette, ported).
fn line_for(kind: &str, text: &str) -> Line<'static> {
    let mut t: String = text.to_string();
    if t.len() > 300 {
        t = t[0..300].to_string();
    }
    match kind {
        "delta" => rp::Line::from(format!("… {}", t).to_string()).dark_gray(),
        "thinking" => rp::Line::from(format!("… {}", t).to_string()).dim(),
        "tool" => rp::Line::from(format!("▶ {}", t).to_string()).cyan(),
        "result" => rp::Line::from(format!("· {}", t).to_string()).gray(),
        "error" => rp::Line::from(format!("✗ {}", t).to_string()).red(),
        "ask" | "stuck" => rp::Line::from(format!("? {}", t).to_string()).yellow(),
        "answer" => rp::Line::from(format!("→ {}", t).to_string()).green(),
        _ => rp::Line::from(text.to_string()),
    }
}

/// Status rail: provider/model, cost, iterations, cage, mode.
fn rail_lines(status: &Status, blocks: bool) -> Vec<Line<'static>> {
    let ver: &str = env!("CARGO_PKG_VERSION");
    let mode: String = if status.running {
        "working".into()
    } else {
        "idle".into()
    };
    let cost = round2(status.cost);
    let spent = round2(status.spent);
    let prov: String = status.provider.clone();
    let model: String = status.model.clone();
    let cage: String = status.cage.clone();
    let iters = status.iterations;
    let block_hint: String = if blocks {
        "block view ON".into()
    } else {
        "Ctrl-b: blocks".into()
    };
    let l1: String = format!("curam {}", ver);
    let l2: String = format!("{}  {}", prov, model);
    let l3: String = format!("cost=${}  spent=${}", cost, spent);
    let l4: String = format!("iterations {}", iters);
    let l5: String = format!("cage {}", cage);
    let l6: String = format!("mode {}", mode);
    vec![
        rp::Line::from(l1).bold(),
        rp::Line::from(l2).cyan(),
        rp::Line::from(l3).yellow(),
        rp::Line::from(l4),
        rp::Line::from(l5).dark_gray(),
        rp::Line::from(l6),
        rp::Line::from(block_hint).gray(),
    ]
}

/// Spawn one task run. Owns the journal; streams events to the UI bus.
pub fn spawn_task(
    cfg: config::Config,
    provider: Provider,
    budget: Budget,
    workdir: std::path::PathBuf,
    journal_path: std::path::PathBuf,
    task: String,
    agent_tx: mpsc::Sender<UiEv>,
    agent_reply_rx: mpsc::Receiver<String>,
    cancel: Arc<AtomicBool>,
) {
    let cancel_worker: Arc<AtomicBool> = cancel.clone();
    let my_tx = agent_tx.clone();
    let cfg_owned: config::Config = cfg;
    let task_owned: String = task;
    thread::spawn(move || {
        let mut jj = Journal::open(journal_path, None).unwrap();
        let mut a = super::agent::Agent::new(
            &cfg_owned, provider, budget, workdir, true, false, false, false,
        );
        a.ui_bus = Some(UiBus {
            tx: agent_tx,
            reply_rx: Some(agent_reply_rx),
            cancel: cancel_worker,
        });
        let ans = a.run_task(task_owned.as_str(), &mut jj);
        match ans {
            Err(e) => {
                let _ = my_tx
                    .send(UiEv {
                        kind: "error".into(),
                        text: e,
                    })
                    .ok();
                let _ = my_tx
                    .send(UiEv {
                        kind: "done".into(),
                        text: String::new(),
                    })
                    .ok();
            }
            Ok(None) => {
                let _ = my_tx
                    .send(UiEv {
                        kind: "done".into(),
                        text: String::new(),
                    })
                    .ok();
            }
            Ok(Some(text)) => {
                let _ = my_tx
                    .send(UiEv {
                        kind: "answer".into(),
                        text: text.clone(),
                    })
                    .ok();
                let _ = my_tx
                    .send(UiEv {
                        kind: "done".into(),
                        text: String::new(),
                    })
                    .ok();
            }
        }
    });
}

/// The TUI event loop. Returns the process exit code (0 ok, 1 runtime).
pub fn run(
    cfg: config::Config,
    provider: Provider,
    budget: Budget,
    workdir: std::path::PathBuf,
    journal_path: std::path::PathBuf,
) -> i32 {
    let mut terminal = match ratatui::try_init() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("curam tui: terminal init failed: {e}");
            return 1;
        }
    };
    let (agent_tx, ui_rx) = mpsc::channel::<UiEv>();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut active_reply_tx: Option<mpsc::Sender<String>> = None;
    let mut app = App::new(&cfg, &provider, &budget);
    let cfg_owned: config::Config = cfg;

    loop {
        // 1. keys
        if event::poll(Duration::from_millis(24)).unwrap_or(false) {
            if let Ok(ev) = event::read() {
                let kev = ev.as_key_press_event();
                if let Some(k) = kev {
                    let reply = app.on_key(k.code, k.modifiers, &cancel);
                    if app.mode == Mode::Quit {
                        break;
                    }
                    if let Some(r) = reply {
                        if let Some(tx) = &active_reply_tx {
                            let _ = tx.send(r).ok();
                        }
                    }
                }
            }
        }
        if app.mode == Mode::Quit {
            break;
        }

        // 2. submit a task (Enter parked enter_pending in Input mode)
        if app.enter_pending && !app.status.running && !app.input.is_empty() {
            app.enter_pending = false;
            let task: String = app.input.clone();
            app.input = String::new();
            app.status.running = true;
            cancel.store(false, Ordering::Relaxed);
            let (reply_tx, agent_reply_rx) = mpsc::channel::<String>();
            active_reply_tx = Some(reply_tx);
            spawn_task(
                cfg_owned.clone(),
                provider.clone(),
                budget.clone(),
                workdir.clone(),
                journal_path.clone(),
                task,
                agent_tx.clone(),
                agent_reply_rx,
                cancel.clone(),
            );
        }

        // 3. drain agent events
        if app.status.running {
            loop {
                match ui_rx.try_recv() {
                    Ok(ev) => app.apply(&ev),
                    Err(_) => break,
                }
            }
        }

        // 4. draw
        let done = terminal.draw(|frame| app.render(frame));
        if done.is_err() {
            break;
        }
    }
    ratatui::try_restore().ok();
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::prelude as rp;

    fn sample_app() -> App {
        let cfg = config::Loader::new(None, None).load().unwrap();
        let p = Provider::new(&cfg, None, None).unwrap();
        let b = Budget::from_cfg(&cfg.budget);
        let mut app = App::new(&cfg, &p, &b);
        app.push("user", "add a README".into());
        app.push("tool", "read {\"path\":\"README.md\"}".into());
        app.push("result", "{\"ok\":true}".into());
        app.status.cost = 0.5;
        app.status.spent = 0.5;
        app
    }

    #[test]
    fn tui_layout_renders_within_24x80() {
        let mut app = sample_app();
        let mut term = rp::Terminal::new(rp::backend::TestBackend::new(80, 24)).unwrap();
        let ok = term.draw(|frame| app.render(frame));
        assert!(ok.is_ok(), "draw under 24x80 must not panic");
    }

    #[test]
    fn tui_renders_input_and_ask_modes() {
        let mut app = sample_app();
        app.input = "hello curam".into();
        let mut term = rp::Terminal::new(rp::backend::TestBackend::new(80, 24)).unwrap();
        assert!(
            term.draw(|frame| app.render(frame)).is_ok(),
            "input mode renders"
        );
        let mut app2 = sample_app();
        app2.mode = Mode::Ask(AskPrompt {
            question: "proceed?".into(),
            answer: "".into(),
        });
        let mut term2 = rp::Terminal::new(rp::backend::TestBackend::new(80, 24)).unwrap();
        assert!(
            term2.draw(|frame| app2.render(frame)).is_ok(),
            "ask modal renders"
        );
    }

    #[test]
    fn tui_applies_delta_folding_and_ask_modal() {
        let mut app = sample_app();
        app.apply(&UiEv {
            kind: "delta".into(),
            text: "Hel".into(),
        });
        app.apply(&UiEv {
            kind: "delta".into(),
            text: "lo".into(),
        });
        assert_eq!(app.rows[app.rows.len() - 1].text, "Hello");
        let ask = UiEv {
            kind: "ask".into(),
            text: "rm -rf / (destructive)".into(),
        };
        app.apply(&ask);
        assert!(
            app.mode
                == Mode::Ask(AskPrompt {
                    question: "rm -rf / (destructive)".into(),
                    answer: String::new(),
                })
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let reply = app.on_key(KeyCode::Char('n'), KeyModifiers::NONE, &cancel);
        assert_eq!(reply, Some("no".into()));
        assert!(app.mode == Mode::Input);
    }
}
