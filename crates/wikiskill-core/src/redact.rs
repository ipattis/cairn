//! Redaction pass: pattern rules plus an entropy check, run before any vault write,
//! git remote push or Jev call.
//!
//! It protects the vault and Jev, not inference: Fireworks still sees every rollout in
//! full, which is a terms question rather than a code one.

use std::sync::OnceLock;

use regex::Regex;

/// What the redactor replaced, for the note's provenance line.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RedactionReport {
    /// Rule name -> number of replacements.
    pub counts: Vec<(String, usize)>,
}

impl RedactionReport {
    pub fn total(&self) -> usize {
        self.counts.iter().map(|(_, n)| n).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// Folds another report in, summing per rule. Two passes over the same session (the
    /// condensed note and the full JSONL) would otherwise report `key×1, key×1`, which reads
    /// like two different secrets.
    pub fn merge(&mut self, other: &RedactionReport) {
        for (rule, hits) in &other.counts {
            match self.counts.iter_mut().find(|(name, _)| name == rule) {
                Some((_, existing)) => *existing += hits,
                None => self.counts.push((rule.clone(), *hits)),
            }
        }
    }

    pub fn summary(&self) -> String {
        if self.is_empty() {
            return "none".into();
        }
        self.counts
            .iter()
            .map(|(rule, n)| format!("{rule}×{n}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

struct Rule {
    name: &'static str,
    re: Regex,
    replacement: &'static str,
}

fn rules() -> &'static Vec<Rule> {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        vec![
            Rule {
                name: "aws-access-key-id",
                re: Regex::new(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b").unwrap(),
                replacement: "[REDACTED:aws-access-key-id]",
            },
            Rule {
                name: "aws-secret",
                re: Regex::new(
                    r#"(?i)aws_secret_access_key\s*[:=]\s*["']?[A-Za-z0-9/+=]{40}["']?"#,
                )
                .unwrap(),
                replacement: "aws_secret_access_key=[REDACTED:aws-secret]",
            },
            Rule {
                name: "bearer-token",
                re: Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{20,}").unwrap(),
                replacement: "Bearer [REDACTED:bearer-token]",
            },
            Rule {
                name: "provider-api-key",
                // fw_…, sk-…, anthropic/openai style keys, TypeSafe keys.
                re: Regex::new(r"\b(?:fw_[A-Za-z0-9]{16,}|sk-[A-Za-z0-9_-]{16,}|ts_[A-Za-z0-9]{16,})\b")
                    .unwrap(),
                replacement: "[REDACTED:provider-api-key]",
            },
            Rule {
                name: "github-token",
                re: Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{20,}\b").unwrap(),
                replacement: "[REDACTED:github-token]",
            },
            Rule {
                name: "private-key-block",
                re: Regex::new(
                    r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
                )
                .unwrap(),
                replacement: "[REDACTED:private-key-block]",
            },
            Rule {
                name: "jwt",
                re: Regex::new(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b")
                    .unwrap(),
                replacement: "[REDACTED:jwt]",
            },
            Rule {
                name: "env-assignment",
                re: Regex::new(
                    r#"(?i)\b([A-Z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|API_KEY|APIKEY|CREDENTIAL)[A-Z0-9_]*)\s*=\s*["']?([^\s"']{8,})["']?"#,
                )
                .unwrap(),
                replacement: "$1=[REDACTED:env-assignment]",
            },
            Rule {
                name: "email",
                re: Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b").unwrap(),
                replacement: "[REDACTED:email]",
            },
        ]
    })
}

/// Shannon entropy in bits per character.
pub fn entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    let mut total = 0usize;
    for b in s.bytes() {
        counts[b as usize] += 1;
        total += 1;
    }
    let total = total as f64;
    -counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / total;
            p * p.log2()
        })
        .sum::<f64>()
}

/// Tokens this long and this random are treated as secrets even without a matching rule.
const ENTROPY_MIN_LEN: usize = 24;
const ENTROPY_THRESHOLD: f64 = 4.0;

/// Applies the pattern rules, then the entropy check, to `input`.
pub fn redact(input: &str) -> (String, RedactionReport) {
    let mut text = input.to_string();
    let mut report = RedactionReport::default();

    for rule in rules() {
        let hits = rule.re.find_iter(&text).count();
        if hits > 0 {
            text = rule.re.replace_all(&text, rule.replacement).into_owned();
            report.counts.push((rule.name.to_string(), hits));
        }
    }

    let (text, entropy_hits) = redact_high_entropy(&text);
    if entropy_hits > 0 {
        report
            .counts
            .push(("high-entropy".to_string(), entropy_hits));
    }
    (text, report)
}

fn redact_high_entropy(text: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut hits = 0usize;
    let mut token = String::new();

    let flush = |token: &mut String, out: &mut String, hits: &mut usize| {
        if is_secretish(token) {
            out.push_str("[REDACTED:high-entropy]");
            *hits += 1;
        } else {
            out.push_str(token);
        }
        token.clear();
    };

    for ch in text.chars() {
        // Token characters are those a key could plausibly be made of.
        if ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '=' | '_' | '-') {
            token.push(ch);
        } else {
            flush(&mut token, &mut out, &mut hits);
            out.push(ch);
        }
    }
    flush(&mut token, &mut out, &mut hits);
    (out, hits)
}

fn is_secretish(token: &str) -> bool {
    if token.len() < ENTROPY_MIN_LEN {
        return false;
    }
    // Hex digests and decimal runs are common in traces and not secrets by themselves.
    let hex_only = token.chars().all(|c| c.is_ascii_hexdigit());
    if hex_only {
        return false;
    }
    let has_upper = token.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = token.chars().any(|c| c.is_ascii_lowercase());
    let has_digit = token.chars().any(|c| c.is_ascii_digit());
    if !(has_upper && has_lower && has_digit) {
        return false;
    }
    entropy(token) >= ENTROPY_THRESHOLD
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_provider_keys_and_reports_them() {
        let (out, report) =
            redact("export FIREWORKS_API_KEY=fw_3xampleKEY012345678901234567\n");
        assert!(!out.contains("fw_3xample"), "{out}");
        assert!(report.total() >= 1);
        assert!(report.summary().contains('×'));
    }

    #[test]
    fn redacts_aws_key_ids_and_private_keys() {
        let (out, _) = redact("id AKIAIOSFODNN7EXAMPLE end");
        assert!(out.contains("[REDACTED:aws-access-key-id]"));
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nabc\ndef\n-----END RSA PRIVATE KEY-----";
        let (out, _) = redact(pem);
        assert_eq!(out, "[REDACTED:private-key-block]");
    }

    #[test]
    fn entropy_catches_unlabelled_secrets() {
        let (out, report) = redact("token is Xk2Lm9QpZa4Tb7Rc1Ve8Nd5Yh3Gj6Uw");
        assert!(out.contains("[REDACTED:high-entropy]"), "{out}");
        assert!(report.counts.iter().any(|(r, _)| r == "high-entropy"));
    }

    #[test]
    fn leaves_ordinary_trace_text_alone() {
        let text = "running cargo test --package wikiskill-core -- vault::tests\n\
                    commit 4f9a1c2b8e7d6f5a4b3c2d1e0f9a8b7c6d5e4f3a\n\
                    1 passed, 0 failed in 12.4s";
        let (out, report) = redact(text);
        assert_eq!(out, text, "unexpected redaction: {}", report.summary());
        assert!(report.is_empty());
    }

    #[test]
    fn redaction_is_idempotent() {
        let once = redact("key fw_3xampleKEY012345678901234567").0;
        let twice = redact(&once).0;
        assert_eq!(once, twice);
    }
}
