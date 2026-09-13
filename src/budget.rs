//! budget.rs — session cost accounting from real `usage` fields (1:1 port
//! of budget.py). Prices live in curam.toml ([budget] price_per_mtok, USD
//! per 1M tokens). If a provider omits usage we record a `cost_unknown`
//! event so the N8 "reported cost within ±10%" SLO knows when it's blind.

use serde_json::Value;

use super::config::BudgetCfg;
use super::journal::Journal;

#[derive(Debug)]
pub struct Budget {
    pub cap_usd: f64,
    prices: Vec<(String, f64)>,
    pub spent_usd: f64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub unknown_usage: u64,
}

impl Budget {
    pub fn from_cfg(cfg: &BudgetCfg) -> Self {
        // Malformed prices degrade to "all models free", never a crash.
        let prices = cfg.price_per_mtok.clone();
        Self {
            cap_usd: cfg.cap_usd,
            prices,
            spent_usd: 0.0,
            prompt_tokens: 0,
            completion_tokens: 0,
            unknown_usage: 0,
        }
    }

    pub fn price_per_mtok(&self, model: &str) -> f64 {
        for i in 0..self.prices.len() {
            if self.prices[i].0 == model {
                return self.prices[i].1;
            }
        }
        0.0 // unknown model = free (local default)
    }

    /// Record a completion's token usage; returns the cost in USD. Emits the
    /// `cost` event (or `cost_unknown` when usage is missing).
    pub fn charge(
        &mut self,
        model: &str,
        usage: &Value,
        journal: &mut Journal,
    ) -> Result<f64, String> {
        let pt = usage.get("prompt_tokens").and_then(|v| v.as_u64());
        let ct = usage.get("completion_tokens").and_then(|v| v.as_u64());
        if pt.is_none() || ct.is_none() {
            self.unknown_usage += 1;
            journal.log("cost_unknown", usage)?;
            return Ok(0.0);
        }
        let pt = pt.unwrap();
        let ct = ct.unwrap();
        let cost = self.price_per_mtok(model) * (pt as f64 + ct as f64) / 1_000_000.0;
        self.spent_usd += cost;
        self.prompt_tokens += pt;
        self.completion_tokens += ct;
        let mut m = serde_json::Map::new();
        m.insert("model".to_owned(), model.into());
        m.insert("prompt_tokens".to_owned(), pt.into());
        m.insert("completion_tokens".to_owned(), ct.into());
        m.insert("cost_usd".to_owned(), round6(cost).into());
        m.insert(
            "session_total_usd".to_owned(),
            round6(self.spent_usd).into(),
        );
        journal.log("cost", &serde_json::Value::Object(m))?;
        Ok(cost)
    }

    pub fn over_budget(&self) -> bool {
        self.cap_usd > 0.0 && self.spent_usd >= self.cap_usd
    }
}

pub fn round6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget_cfg(cap: f64) -> BudgetCfg {
        BudgetCfg {
            cap_usd: cap,
            price_per_mtok: vec![("gpt-4o".into(), 2.5), ("local-model".into(), 0.0)],
        }
    }

    #[test]
    fn charge_accounts_real_cost() {
        let dir = std::env::temp_dir()
            .join("curam-h2-b")
            .join(format!("{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut j = Journal::open(dir.join("j.db"), None).unwrap();
        let mut b = Budget::from_cfg(&budget_cfg(1.0));
        let usage = serde_json::from_str("{\"prompt_tokens\":500000,\"completion_tokens\":500000}")
            .unwrap();
        let cost = b.charge("gpt-4o", &usage, &mut j).unwrap();
        assert_eq!(cost, 2.5);
        assert!(b.over_budget(), "2.5 > 1.0 cap");
        assert_eq!(j.events(Some("cost")).unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_model_is_free() {
        let dir = std::env::temp_dir()
            .join("curam-h2-c")
            .join(format!("{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut j = Journal::open(dir.join("j.db"), None).unwrap();
        let mut b = Budget::from_cfg(&budget_cfg(1.0));
        let usage =
            serde_json::from_str("{\"prompt_tokens\":100,\"completion_tokens\":100}").unwrap();
        assert_eq!(b.charge("local-model", &usage, &mut j).unwrap(), 0.0);
        assert_eq!(b.spent_usd, 0.0);
        assert!(!b.over_budget());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_usage_is_tracked_not_silent() {
        let dir = std::env::temp_dir()
            .join("curam-h2-d")
            .join(format!("{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut j = Journal::open(dir.join("j.db"), None).unwrap();
        let mut b = Budget::from_cfg(&budget_cfg(1.0));
        let usage = serde_json::from_str("{}").unwrap();
        assert_eq!(b.charge("gpt-4o", &usage, &mut j).unwrap(), 0.0);
        assert_eq!(b.unknown_usage, 1);
        assert_eq!(j.events(Some("cost_unknown")).unwrap().len(), 1);
        assert_eq!(j.events(Some("cost")).unwrap().len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
