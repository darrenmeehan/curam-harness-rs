//! report.rs — turn the session journal into an evaluation report (port
//! of report.py). `curam report [--since 2h|30m|1d] [--json]` — the
//! "how did the harness actually perform" surface: cost, tool usage, asks,
//! skills, guardrail events, completion, timing.

use serde_json::Value;

use super::journal::{now_ts, EventRow};

/// One session's collapsed counters.
#[derive(Debug)]
pub struct SessionAgg {
    pub events: u64,
    pub tool_calls: u64,
    pub tool_errors: u64,
    pub denied: u64,
    pub asks: u64,
    pub approvals_requested: u64,
    pub approvals_granted: u64,
    pub cost_usd: f64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub errors: u64,
    pub skills_used: u64,
    tool_by_name: Vec<(String, u64)>,
    skill_by_name: Vec<(String, u64)>,
}

impl SessionAgg {
    fn new() -> Self {
        Self {
            events: 0,
            tool_calls: 0,
            tool_errors: 0,
            denied: 0,
            asks: 0,
            approvals_requested: 0,
            approvals_granted: 0,
            cost_usd: 0.0,
            prompt_tokens: 0,
            completion_tokens: 0,
            errors: 0,
            skills_used: 0,
            tool_by_name: Vec::new(),
            skill_by_name: Vec::new(),
        }
    }

    fn bump(&mut self, key: &str) {
        for i in 0..self.tool_by_name.len() {
            if self.tool_by_name[i].0.as_str() == key {
                self.tool_by_name[i] = (self.tool_by_name[i].0.clone(), self.tool_by_name[i].1 + 1);
                return;
            }
        }
        self.tool_by_name.push((key.into(), 1));
    }

    fn bump_skill(&mut self, key: &str) {
        for i in 0..self.skill_by_name.len() {
            if self.skill_by_name[i].0.as_str() == key {
                self.skill_by_name[i] =
                    (self.skill_by_name[i].0.clone(), self.skill_by_name[i].1 + 1);
                return;
            }
        }
        self.skill_by_name.push((key.into(), 1));
    }

    fn sorted_name_counts(&self) -> Vec<(String, u64)> {
        let mut v = self.tool_by_name.clone();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    fn sorted_skill_counts(&self) -> Vec<(String, u64)> {
        let mut v = self.skill_by_name.clone();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }
}

/// "2h" / "30m" / "1d" / "45s" → seconds (unknown unit defaults to hours).
fn parse_since(s: &str) -> u64 {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return 0; // treat as "all time"
    }
    let unit = &s[s.len() - 1..];
    let num = s[0..s.len() - 1].parse::<u64>().unwrap_or(0);
    match unit {
        "s" => num,
        "m" => num * 60,
        "d" => num * 86400,
        _ => num * 3600,
    }
}

fn fmt_usd(x: f64) -> String {
    // $0.1234 — round to 4 decimals like the POC
    let r = (x * 10_000.0).round() / 10_000.0;
    format!("${}", r)
}

/// Collapse events into per-session + totals. `cutoff` (unix secs) filters
/// which events count; None = all time.
pub fn aggregate(
    events: &[EventRow],
    cutoff: Option<f64>,
) -> (Vec<(String, SessionAgg)>, SessionAgg) {
    let mut sessions: Vec<(String, SessionAgg)> = Vec::new();
    let mut totals = SessionAgg::new();

    for e in events {
        if let Some(c) = cutoff {
            if e.ts < c {
                continue;
            }
        }
        totals.events += 1;
        let mut sidx: Option<usize> = None;
        for i in 0..sessions.len() {
            if sessions[i].0 == e.session_id {
                sidx = Some(i);
            }
        }
        let idx = match sidx {
            Some(i) => i,
            None => {
                sessions.push((e.session_id.clone(), SessionAgg::new()));
                sessions.len() - 1
            }
        };
        let s = &mut sessions[idx].1;
        s.events += 1;
        match e.etype.as_str() {
            "tool_call" => {
                totals.tool_calls += 1;
                s.tool_calls += 1;
                let name = e
                    .payload
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                totals.bump(name);
                s.bump(name);
            }
            "tool_denied" => {
                totals.denied += 1;
                s.denied += 1;
            }
            "ask_human" => {
                totals.asks += 1;
                s.asks += 1;
            }
            "approval_requested" => {
                totals.approvals_requested += 1;
                s.approvals_requested += 1;
            }
            "approval_granted" => {
                totals.approvals_granted += 1;
                s.approvals_granted += 1;
            }
            "cost" => {
                let c = e
                    .payload
                    .get("cost_usd")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                totals.cost_usd += c;
                s.cost_usd += c;
                let pt = e
                    .payload
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let ct = e
                    .payload
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                totals.prompt_tokens += pt;
                totals.completion_tokens += ct;
                s.prompt_tokens += pt;
                s.completion_tokens += ct;
            }
            "error" => {
                totals.errors += 1;
                s.errors += 1;
            }
            "skill_used" => {
                totals.skills_used += 1;
                s.skills_used += 1;
                let name = e
                    .payload
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                totals.bump_skill(name);
                s.bump_skill(name);
            }
            "tool_result" => {
                // no dedicated tool_errors event; approximate from ok:false
                let ok = e
                    .payload
                    .get("result")
                    .and_then(|r| r.get("ok"))
                    .and_then(|o| o.as_bool())
                    .unwrap_or(true);
                if !ok {
                    totals.tool_errors += 1;
                    s.tool_errors += 1;
                }
            }
            _ => {}
        }
    }
    sessions.sort_by(|a, b| a.0.cmp(&b.0));
    (sessions, totals)
}

fn counts_json(counts: &Vec<(String, u64)>) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in counts {
        m.insert(k.clone().to_owned(), (*v).into());
    }
    serde_json::Value::Object(m)
}

fn session_json(s: &SessionAgg) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("events".to_owned(), s.events.into());
    m.insert("tool_calls".to_owned(), s.tool_calls.into());
    m.insert("tool_errors".to_owned(), s.tool_errors.into());
    m.insert("denied".to_owned(), s.denied.into());
    m.insert("asks".to_owned(), s.asks.into());
    m.insert("cost_usd".to_owned(), s.cost_usd.into());
    m.insert("prompt_tokens".to_owned(), s.prompt_tokens.into());
    m.insert("completion_tokens".to_owned(), s.completion_tokens.into());
    m.insert("errors".to_owned(), s.errors.into());
    m.insert("skills_used".to_owned(), s.skills_used.into());
    m.insert(
        "tool_by_name".to_owned(),
        counts_json(&s.sorted_name_counts()),
    );
    m.insert(
        "skill_by_name".to_owned(),
        counts_json(&s.sorted_skill_counts()),
    );
    serde_json::Value::Object(m)
}

fn totals_json(t: &SessionAgg) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("events".to_owned(), t.events.into());
    m.insert("sessions".to_owned(), 0.into());
    m.insert("tool_calls".to_owned(), t.tool_calls.into());
    m.insert("tool_errors".to_owned(), t.tool_errors.into());
    m.insert("denied".to_owned(), t.denied.into());
    m.insert("asks".to_owned(), t.asks.into());
    m.insert(
        "approvals_requested".to_owned(),
        t.approvals_requested.into(),
    );
    m.insert("approvals_granted".to_owned(), t.approvals_granted.into());
    m.insert("cost_usd".to_owned(), t.cost_usd.into());
    m.insert("prompt_tokens".to_owned(), t.prompt_tokens.into());
    m.insert("completion_tokens".to_owned(), t.completion_tokens.into());
    m.insert("errors".to_owned(), t.errors.into());
    m.insert("skills_used".to_owned(), t.skills_used.into());
    m.insert(
        "tool_by_name".to_owned(),
        counts_json(&t.sorted_name_counts()),
    );
    m.insert(
        "skill_by_name".to_owned(),
        counts_json(&t.sorted_skill_counts()),
    );
    serde_json::Value::Object(m)
}

/// Render the report for a set of events. `source` is the journal path
/// string shown in the header; `since` is the raw CLI arg (for display).
pub fn render(events: &[EventRow], source: &str, since: Option<&str>, as_json: bool) -> String {
    let cutoff = since.map(|s| now_ts() - (parse_since(s) as f64));
    let (sessions, t) = aggregate(events, cutoff);

    if as_json {
        let mut m = serde_json::Map::new();
        m.insert("source".to_owned(), source.into());
        m.insert("since".to_owned(), since.unwrap_or("all").into());
        m.insert("totals".to_owned(), totals_json(&t));
        let mut sm = serde_json::Map::new();
        for (sid, s) in &sessions {
            sm.insert(sid.clone().to_owned(), session_json(s));
        }
        m.insert("sessions".to_owned(), serde_json::Value::Object(sm));
        return serde_json::to_string(&serde_json::Value::Object(m))
            .expect("report json serializes");
    }

    let source_owned: String = source.to_string();
    let since_str = since.unwrap_or("all time");
    let mut lines = Vec::new();
    lines.push(format!("curam report  ({source_owned})"));
    lines.push(format!("  since:        {since_str}"));
    lines.push(format!("  sessions:     {}", sessions.len()));
    lines.push(format!("  events:       {}", t.events));
    lines.push(format!(
        "  tool_calls:   {}  (denied {}, errors {})",
        t.tool_calls, t.denied, t.tool_errors
    ));
    lines.push(format!(
        "  asks:         {}  approvals {} ({} granted)",
        t.asks, t.approvals_requested, t.approvals_granted
    ));
    lines.push(format!("  skills used:  {}", t.skills_used));
    lines.push(format!("  cost:         {}", fmt_usd(t.cost_usd)));
    lines.push(format!(
        "  tokens:       {} in / {} out",
        t.prompt_tokens, t.completion_tokens
    ));
    lines.push(format!("  errors:       {}", t.errors));
    if !t.tool_by_name.is_empty() {
        lines.push(format!(
            "  tools by name: {}",
            fmt_counts(&t.sorted_name_counts())
        ));
    }
    if !t.skill_by_name.is_empty() {
        lines.push(format!(
            "  skills by name: {}",
            fmt_counts(&t.sorted_skill_counts())
        ));
    }
    lines.push(String::new());
    for (sid, s) in &sessions {
        let short_id = if sid.len() > 12 {
            sid[0..12].to_string()
        } else {
            sid.clone()
        };
        lines.push(format!(
            "  session {}: {} calls, {} asks, {} cost, {} events",
            short_id,
            s.tool_calls,
            s.asks,
            fmt_usd(s.cost_usd),
            s.events
        ));
        if !s.tool_by_name.is_empty() {
            lines.push(format!(
                "      tools: {}",
                fmt_counts(&s.sorted_name_counts())
            ));
        }
        if !s.skill_by_name.is_empty() {
            lines.push(format!(
                "      skills: {}",
                fmt_counts(&s.sorted_skill_counts())
            ));
        }
    }
    lines.join("\n")
}

fn fmt_counts(counts: &Vec<(String, u64)>) -> String {
    let parts: Vec<String> = counts
        .iter()
        .map(|(k, v)| format!("{}: {}", k, v))
        .collect::<Vec<String>>();
    format!("{{{}}}", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn row(ts: f64, sid: &str, etype: &str, payload: &str) -> EventRow {
        EventRow {
            seq: 0,
            session_id: sid.into(),
            ts,
            etype: etype.into(),
            payload: serde_json::from_str(payload).unwrap(),
        }
    }

    fn cfg() -> Vec<EventRow> {
        // two sessions worth of events, spread across two timestamps
        let mut evs: Vec<EventRow> = vec![
            row(1000.0, "sess-a", "user_message", "{\"text\":\"hi\"}"),
            row(1001.0, "sess-a", "tool_call", "{\"name\":\"read\"}"),
            row(
                1002.0,
                "sess-a",
                "tool_result",
                "{\"name\":\"read\",\"result\":{\"ok\":false}}",
            ),
            row(
                1003.0,
                "sess-a",
                "cost",
                "{\"cost_usd\":0.25,\"prompt_tokens\":100,\"completion_tokens\":50}",
            ),
            row(2000.0, "sess-b", "tool_call", "{\"name\":\"shell\"}"),
            row(
                2001.0,
                "sess-b",
                "skill_used",
                "{\"name\":\"git-workflow\"}",
            ),
            row(
                2002.0,
                "sess-b",
                "tool_denied",
                "{\"command\":\"rm -rf /\"}",
            ),
            row(
                2003.0,
                "sess-b",
                "cost",
                "{\"cost_usd\":0.75,\"prompt_tokens\":200,\"completion_tokens\":100}",
            ),
        ];
        evs[1].ts = 1001.0;
        evs[2].ts = 1002.0;
        evs[3].ts = 1003.0;
        evs
    }

    #[test]
    fn aggregate_counts_sessions_and_totals() {
        let evs = cfg();
        let (sessions, t) = aggregate(&evs, None);
        assert_eq!(sessions.len(), 2);
        assert_eq!(t.events, 8);
        assert_eq!(t.tool_calls, 2);
        assert_eq!(t.tool_errors, 1);
        assert_eq!(t.denied, 1);
        assert_eq!(t.asks, 0);
        assert_eq!(t.skills_used, 1);
        assert!((t.cost_usd - 1.0).abs() < 1e-9, "0.25 + 0.75");
        assert_eq!(t.prompt_tokens, 300);
        assert_eq!(t.completion_tokens, 150);
        // totals sorted-name map
        let names = t.sorted_name_counts();
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn since_filter_drops_old_events() {
        let evs = cfg();
        // cutoff 1500: only sess-b counts
        let (sessions, t) = aggregate(&evs, Some(1500.0));
        assert_eq!(t.events, 4);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0, "sess-b");
        assert!((t.cost_usd - 0.75).abs() < 1e-9);
    }

    #[test]
    fn render_text_and_json_include_the_surface() {
        let evs = cfg();
        let text = render(&evs, "/tmp/curam.db", None, false);
        assert!(text.contains("curam report"), "header");
        assert!(text.contains("tool_calls:   2"), "totals in text");
        assert!(text.contains("cost:         $1"), "cost rounded");
        let json = render(&evs, "/tmp/curam.db", Some("1d"), true);
        assert!(json.contains("/tmp/curam.db"), "json source");
        assert!(json.contains("\"sessions\""), "json sessions");
        assert!(json.contains("1d"), "json since");
    }

    #[test]
    fn parse_since_units() {
        assert_eq!(parse_since("45s"), 45);
        assert_eq!(parse_since("30m"), 1800);
        assert_eq!(parse_since("2h"), 7200);
        assert_eq!(parse_since("1d"), 86400);
    }
}
