//! sanitize.rs — PII / secret sanitizer (1:1 port of sanitize.py).
//!
//! The rule that matters: the JOURNAL keeps the RAW result (audit
//! completeness); only what crosses the provider boundary is scrubbed — the
//! main model's context and the classifier judge's input. Tiers mirror the
//! POC: off | deterministic (built). `local_llm` stays deferred, exactly as
//! in sanitize.py.

use regex::Regex;
use std::sync::OnceLock;

use super::config::PiiCfg;

pub const DEFAULT_PLACEHOLDER: &str = "{REDACTED}";

/// Ordered (kind, pattern) — first-match-wins per span, same order as
/// sanitize.py so behavior stays byte-identical. Flags are inline: (?i) for
/// key_value, (?s) for the DOTALL private-key block.
const PATTERNS: &[(&str, &str)] = &[
    ("openrouter", "sk-or-[A-Za-z0-9_-]{16,}"),
    ("openai", "\\bsk-[A-Za-z0-9]{16,}"),
    ("aws", "\\bAKIA[0-9A-Z]{16}\\b"),
    ("github_pat", "\\bghp_[A-Za-z0-9]{20,}\\b"),
    ("github_fine_grained", "\\bgithub_pat_[A-Za-z0-9_]{20,}\\b"),
    ("slack", "\\bxox[baprs]-[A-Za-z0-9-]{10,}\\b"),
    ("google", "\\bAIza[0-9A-Za-z_-]{30,}\\b"),
    (
        "jwt",
        "\\beyJ[A-Za-z0-9_-]{10,}\\.[A-Za-z0-9_-]{10,}\\.[A-Za-z0-9_-]{10,}\\b",
    ),
    (
        "email",
        "\\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\\.[A-Za-z]{2,}\\b",
    ),
    (
        "private_key",
        "(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
    ),
    ("generic_sk", "\\bsk-[A-Za-z0-9_-]{16,}\\b"),
    (
        "key_value",
        "(?i)\\b(?:token|secret|api[_-]?key)\\s*[=:]\\s*[\'\"]?[A-Za-z0-9_\\-./+=]{12,}",
    ),
];

fn compiled_patterns() -> &'static Vec<(String, Regex)> {
    static C: OnceLock<Vec<(String, Regex)>> = OnceLock::new();
    C.get_or_init(|| {
        PATTERNS
            .iter()
            .map(|(kind, pat)| {
                (
                    kind.to_string(),
                    Regex::new(pat).expect("valid sanitize regex"),
                )
            })
            .collect()
    })
}

#[derive(Debug)]
pub struct Sanitizer {
    level: String,
    placeholder: String,
    internal_domains: Vec<String>,
    pub spans_masked: u64,
}

impl Sanitizer {
    pub fn from_cfg(cfg: &PiiCfg) -> Self {
        Self {
            level: cfg.level.clone(),
            placeholder: if cfg.placeholder.is_empty() {
                DEFAULT_PLACEHOLDER.into()
            } else {
                cfg.placeholder.clone()
            },
            internal_domains: cfg.internal_domains.clone(),
            spans_masked: 0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.level == "deterministic" || self.level == "local_llm"
    }

    /// Mask secrets in `text`. No-op when disabled (or on empty input);
    /// otherwise pure apart from the masked-span metric.
    pub fn scrub(&mut self, text: &str) -> String {
        if !self.enabled() || text.is_empty() {
            return text.to_string();
        }
        let mut out = text.to_string();
        let mut masked = 0;
        for (kind, re) in compiled_patterns() {
            let replacement = format!("{}:{}", self.placeholder, kind);
            masked += re.find_iter(out.as_str()).count();
            let next = re
                .replace_all(out.as_str(), replacement.as_str())
                .to_string();
            out = next;
        }
        for dom in &self.internal_domains {
            let mut esc = String::new();
            for c in dom.chars() {
                if c == '.' {
                    esc.push_str("\\.");
                } else {
                    esc.push(c);
                }
            }
            let pat = Regex::new(format!("[\\w.-]*{}[\\w.-]*", esc).as_str())
                .expect("valid domain pattern");
            masked += pat.find_iter(out.as_str()).count();
            let replacement = format!("{}:domain", self.placeholder);
            out = pat
                .replace_all(out.as_str(), replacement.as_str())
                .to_string();
        }
        self.spans_masked += masked as u64;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scrubber(level: &str) -> Sanitizer {
        Sanitizer::from_cfg(&PiiCfg {
            level: level.into(),
            placeholder: "{REDACTED}".into(),
            internal_domains: Vec::new(),
        })
    }

    #[test]
    fn masks_secrets_and_emails() {
        let mut s = scrubber("deterministic");
        let out = s.scrub("key sk-or-abcdefghijklmnop123456 used here; mail a@b.com");
        assert!(
            !out.contains("sk-or-abcdefghijklmnop123456"),
            "secret masked"
        );
        assert!(!out.contains("a@b.com"), "email masked");
        assert!(out.contains("{REDACTED}:openrouter"), "labeled placeholder");
        assert!(out.contains("{REDACTED}:email"), "email placeholder");
        let masked = s.spans_masked;
        assert!(masked >= 2, "{masked}");
    }

    #[test]
    fn plain_text_is_unchanged() {
        let mut s = scrubber("deterministic");
        assert_eq!(s.scrub("plain text, no secrets"), "plain text, no secrets");
        assert_eq!(s.spans_masked, 0);
    }

    #[test]
    fn private_key_block_masked() {
        let mut s = scrubber("deterministic");
        let pk = "-----BEGIN RSA PRIVATE KEY-----\nABC==\n-----END RSA PRIVATE KEY-----";
        assert!(s.scrub(pk).contains("{REDACTED}:private_key"));
    }

    #[test]
    fn disabled_is_noop() {
        let mut s = scrubber("off");
        assert_eq!(
            s.scrub("key sk-or-abcdefghijklmnop123456"),
            "key sk-or-abcdefghijklmnop123456"
        );
    }

    #[test]
    fn internal_domains_masked() {
        let mut s = Sanitizer::from_cfg(&PiiCfg {
            level: "deterministic".into(),
            placeholder: "{REDACTED}".into(),
            internal_domains: vec!["internal.example".into()],
        });
        let out = s.scrub("reach me at host.internal.example.com:8080");
        assert!(!out.contains("host.internal.example.com"), "domain masked");
        assert!(out.contains("{REDACTED}:domain"), "domain placeholder");
    }
}
