//! sli.rs — the SLO surface (port of ui.sli_summary): counts per event
//! type + cost rollup, straight from the journal. Every SLI stays one
//! query over the events table (docs/curam-slos.md §5).

use super::journal::EventRow;

/// Render the summary block for a session (or the whole journal).
pub fn summary(events: &[EventRow]) -> String {
    let mut kinds: Vec<(String, u64)> = Vec::new();
    let mut cost = 0.0;
    for e in events {
        bump_kind(&mut kinds, e.etype.as_str());
        if e.etype == "cost" {
            cost += e
                .payload
                .get("cost_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
        }
    }
    let asks = count(&kinds, "ask_human");
    let approvals = count(&kinds, "approval_requested");
    let granted = count(&kinds, "approval_granted");
    let denied = count(&kinds, "tool_denied");
    let calls = count(&kinds, "tool_call");
    let results = count(&kinds, "tool_result");
    let pending = calls - results - denied;

    let skill_uses = count(&kinds, "skill_used");
    let mut skill_rows = Vec::new();
    for e in events {
        if e.etype == "skill_used" {
            skill_rows.push(format!(
                "    {}: x{} (desc {}ch, body {} lines)",
                e.payload
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default(),
                e.payload.get("uses").and_then(|v| v.as_u64()).unwrap_or(0),
                e.payload
                    .get("desc_chars")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                e.payload
                    .get("body_lines")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            ));
        }
    }

    let mut lines = Vec::new();
    lines.push(format!("-- sli ({} events)", events.len()));
    lines.push(format!(
        "  tool_calls: {}   results: {}   denied: {}   pending: {}",
        calls, results, denied, pending
    ));
    lines.push(format!(
        "  asks: {}   approvals: {} (granted {})",
        asks, approvals, granted
    ));
    lines.push(format!("  cost: ${}", round4(cost)));
    lines.push(format!("  skills used: {} total calls", skill_uses));
    for r in skill_rows {
        lines.push(r);
    }
    let kind_str = kinds
        .iter()
        .map(|(k, n)| format!("{}:{}", k, n))
        .collect::<Vec<_>>()
        .join(", ");
    lines.push(format!("  by type:        {}", kind_str));
    lines.join("\n")
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

fn bump_kind(kinds: &mut Vec<(String, u64)>, name: &str) {
    for i in 0..kinds.len() {
        if kinds[i].0 == name {
            kinds[i].1 += 1;
            return;
        }
    }
    kinds.push((name.into(), 1));
}

fn count(kinds: &[(String, u64)], name: &str) -> u64 {
    for i in 0..kinds.len() {
        if kinds[i].0 == name {
            return kinds[i].1;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(etype: &str, payload: &str) -> EventRow {
        EventRow {
            seq: 0,
            session_id: "s".into(),
            ts: 0.0,
            etype: etype.into(),
            payload: serde_json::from_str(payload).unwrap_or_default(),
        }
    }

    #[test]
    fn summary_counts_and_rolls_up_cost() {
        let events = vec![
            ev("user_message", "{\"text\":\"hi\"}"),
            ev("tool_call", "{\"name\":\"read\"}"),
            ev("tool_result", "{\"ok\":true}"),
            ev("tool_call", "{\"name\":\"shell\"}"),
            ev("tool_denied", "{\"reason\":\"policy\"}"),
            ev("ask_human", "{\"kind\":\"judgment\"}"),
            ev("approval_requested", "{}"),
            ev("approval_granted", "{}"),
            ev("cost", "{\"cost_usd\":0.0416}"),
            ev("cost", "{\"cost_usd\":0.0020}"),
            ev(
                "skill_used",
                "{\"name\":\"git-workflow\",\"uses\":2,\"desc_chars\":80,\"body_lines\":30}",
            ),
        ];
        let out = summary(&events);
        assert!(
            out.contains("tool_calls: 2   results: 1   denied: 1   pending: 0"),
            "tool tallies"
        );
        assert!(out.contains("cost: $0.0436"), "cost rollup");
        assert!(
            out.contains("asks: 1   approvals: 1 (granted 1)"),
            "ask tallies"
        );
        assert!(out.contains("skills used: 1 total calls"), "skill metric");
        assert!(
            out.contains("git-workflow: x2 (desc 80ch, body 30 lines)"),
            "skill detail"
        );
    }
}
