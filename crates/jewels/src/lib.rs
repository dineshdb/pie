#[macro_export]
macro_rules! define_secrets {
    ($($name:expr => $regex:expr),* $(,)?) => {
        pub static SECRET_REGEXES: std::sync::LazyLock<Vec<(&'static str, regex::Regex)>> = std::sync::LazyLock::new(|| {
            vec![
                $(
                    ($name, regex::Regex::new($regex).expect("Invalid regex")),
                )*
            ]
        });

        pub static SECRET_REGEX_SET: std::sync::LazyLock<regex::RegexSet> = std::sync::LazyLock::new(|| {
            regex::RegexSet::new(&[
                $(
                    $regex,
                )*
            ]).expect("Invalid regex set")
        });
    };
}

use serde_json::Value;
use std::borrow::Cow;

define_secrets! {
    // --- Infrastructure & Cloud ---
    "AWS Access Key ID" => r"\b(AKIA)[0-9A-Z]{16}\b",
    "Google API Key" => r"\b(AIza)[0-9A-Za-z\-_]{35}\b",
    "Azure Storage Account Key" => r"\b([a-zA-Z0-9+/]{86}==)\b",
    "Azure DevOps Token" => r"\b([a-z0-9]{52})\b",
    "Heroku API Key" => r"\b([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})\b",
    "Terraform Cloud Token" => r"\b([a-zA-Z0-9]{40}\.atlasv1\.[a-zA-Z0-9]{60,})\b",

    // --- AI & LLM ---
    "OpenAI API Key" => r"\b(sk-)[a-zA-Z0-9]{20,}\b",
    "Anthropic API Key" => r"\b(sk-ant-api03-)[a-zA-Z0-9\-_]{20,}\b",
    "OpenRouter API Key" => r"\b(sk-or-v1-)[a-f0-9]{64}\b",

    // --- Source Control & Registry ---
    "GitHub Personal Access Token" => r"\b(ghp_)[a-zA-Z0-9]{36}\b",
    "GitHub Fine-Grained PAT" => r"\b(github_pat_)[a-zA-Z0-9_]{20,}\b",
    "GitLab Personal Access Token" => r"\b(glpat-)[0-9a-zA-Z\-_]{20}\b",
    "NPM Access Token" => r"\b(npm_)[a-zA-Z0-9]{36}\b",
    "PyPI API Token" => r"\b(pypi-AgEIcHlwaS5vcmc)[0-9A-Za-z\-_]{50,}\b",
    "Docker Hub Token" => r"\b(dbt_)[a-zA-Z0-9]{20,}\b",

    // --- Communication ---
    "Slack Bot Token" => r"\b(xoxb-)[0-9]{11,13}-[0-9]{11,13}-[a-zA-Z0-9]{24}\b",
    "Slack User Token" => r"\b(xoxp-)[0-9]{11,13}-[0-9]{11,13}-[a-zA-Z0-9]{24}\b",
    "Slack Webhook" => r"\b(https://hooks\.slack\.com/services/T[a-zA-Z0-9_]{8}/B[a-zA-Z0-9_]{8}/[a-zA-Z0-9_]{24})\b",
    "Discord Bot Token" => r"\b([MN][A-Za-z\d]{23}\.[\w-]{6}\.[\w-]{27,38})\b",
    "Twilio API Key" => r"\b(SK)[0-9a-fA-F]{32}\b",
    "Twilio Account SID" => r"\b(AC)[0-9a-fA-F]{32}\b",
    "PagerDuty API Key" => r"\b([a-zA-Z0-9]{20})\b",

    // --- Payments & Marketing ---
    "Stripe Secret Key" => r"\b(sk_(?:live|test)_)[0-9a-zA-Z]{24,128}\b",
    "Stripe Restricted Key" => r"\b(rk_(?:live|test)_)[0-9a-zA-Z]{24,128}\b",
    "Square Access Token" => r"\b(sq0atp-)[0-9A-Za-z\-_]{22}\b",
    "SendGrid API Key" => r"\b(SG\.)[a-zA-Z0-9\-_]{22}\.[a-zA-Z0-9\-_]{43}\b",
    "Mailgun API Key" => r"\b(key-)[0-9a-zA-Z]{32}\b",
    "Mailchimp API Key" => r"\b([0-9a-f]{32}-us[0-9]{1,2})\b",

    // --- Monitoring & Utilities ---
    "Datadog API Key" => r"\b([a-z0-9]{32})\b",
    "Cloudflare API Token" => r"\b([a-zA-Z0-9\-_]{40})\b",
    "Postman API Key" => r"\b(PMAK-)[a-f0-9]{24}-[a-f0-9]{24}\b",
    "Bitly Access Token" => r"\b([a-f0-9]{40})\b",

    // --- Generic ---
    "Generic API Key" => r"(?i)\b((?:api[_-]?key|token|secret|password|credential)[\s:=]+)[a-zA-Z0-9\-_]{8,}\b",
}

/// A secret defined purely by word length — no fixed prefix, so `\b` isn't reliable
/// (the character class may include non-`\w` chars like `+`).
/// Instead we tokenize into words (maximal sequences of allowed chars), then
/// check length. This prevents matching across `/`-delimited path components.
struct LengthSecret {
    name: &'static str,
    length: usize,
}

const LENGTH_SECRETS: &[LengthSecret] = &[LengthSecret {
    name: "AWS Secret Access Key",
    length: 40,
}];

/// Regex matching maximal sequences of allowed base64 characters.
static LENGTH_WORD_REGEX: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"[0-9a-zA-Z+]+").expect("Invalid word regex"));

/// Find length-based matches and redact them.
fn redact_lengths<'a>(text: &'a str) -> Cow<'a, str> {
    let re = &LENGTH_WORD_REGEX;
    let mut result = String::new();
    let mut last_end = 0;

    for m in re.find_iter(text) {
        let word = m.as_str();
        let word_len = word.len();

        let matched = LENGTH_SECRETS.iter().find(|s| s.length == word_len);

        result.push_str(&text[last_end..m.start()]);
        if matched.is_some() {
            result.push_str(&"x".repeat(word_len));
        } else {
            result.push_str(word);
        }
        last_end = m.end();
    }

    if last_end == 0 {
        Cow::Borrowed(text)
    } else {
        result.push_str(&text[last_end..]);
        Cow::Owned(result)
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SecretMatch {
    pub kind: &'static str,
    pub value: String,
}

pub fn scan(text: &str) -> Vec<SecretMatch> {
    let mut matches = Vec::new();
    let mut found_ranges = Vec::new();

    let set_matches = SECRET_REGEX_SET.matches(text);
    for idx in set_matches.into_iter() {
        let (kind, re) = &SECRET_REGEXES[idx];
        for m in re.find_iter(text) {
            let range = m.range();
            if found_ranges
                .iter()
                .any(|r: &std::ops::Range<usize>| r.start <= range.start && r.end >= range.end)
            {
                continue;
            }
            matches.push(SecretMatch {
                kind,
                value: m.as_str().to_string(),
            });
            found_ranges.push(range);
        }
    }

    // Also scan for length-based secrets
    for m in LENGTH_WORD_REGEX.find_iter(text) {
        let word = m.as_str();
        for secret in LENGTH_SECRETS {
            if word.len() == secret.length {
                let range = m.range();
                if !found_ranges
                    .iter()
                    .any(|r: &std::ops::Range<usize>| r.start <= range.start && r.end >= range.end)
                {
                    matches.push(SecretMatch {
                        kind: secret.name,
                        value: word.to_string(),
                    });
                    found_ranges.push(range);
                }
            }
        }
    }

    matches
}

pub fn redact<'a>(text: &'a str) -> Cow<'a, str> {
    // Step 1: length-based — tokenize into words, match by length
    let after_lengths = redact_lengths(text);

    // Step 2: regex-based — for prefix-based patterns (sk-, AKIA, etc.)
    let set_matches = SECRET_REGEX_SET.matches(&after_lengths);
    if !set_matches.matched_any() {
        return after_lengths;
    }

    let mut redacted: Cow<'_, str> = after_lengths;
    for idx in set_matches.into_iter() {
        let (_kind, re) = &SECRET_REGEXES[idx];
        let result = re.replace_all(&redacted, |caps: &regex::Captures| {
            let full_match = caps.get(0).map_or("", |m| m.as_str());
            if let Some(prefix) = caps.get(1) {
                let prefix_str = prefix.as_str();
                let mask_len = full_match.len().saturating_sub(prefix_str.len());
                format!("{}{}", prefix_str, "x".repeat(mask_len))
            } else {
                "x".repeat(full_match.len())
            }
        });
        if let Cow::Owned(s) = result {
            redacted = Cow::Owned(s);
        }
    }
    redacted
}

pub fn redact_json(value: &Value) -> Value {
    match value {
        Value::String(s) => {
            let redacted = redact(s);
            match redacted {
                Cow::Owned(r) => Value::String(r),
                Cow::Borrowed(_) => value.clone(),
            }
        }
        Value::Array(arr) => Value::Array(arr.iter().map(redact_json).collect()),
        Value::Object(obj) => {
            let mut new_obj = serde_json::Map::new();
            for (k, v) in obj {
                new_obj.insert(k.clone(), redact_json(v));
            }
            Value::Object(new_obj)
        }
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_openai() {
        let text = "My key is sk-1234567890abcdef1234567890abcdef1234567890abcdef";
        let matches = scan(text);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].kind, "OpenAI API Key");
    }

    #[test]
    fn test_redact() {
        let text = "My key is sk-1234567890abcdef1234567890abcdef1234567890abcdef and AWS AKIA1234567890123456";
        let redacted = redact(text);
        assert_eq!(
            redacted,
            "My key is sk-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx and AWS AKIAxxxxxxxxxxxxxxxx"
        );
    }

    #[test]
    fn test_redact_anthropic() {
        let text = "sk-ant-api03-1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
        let redacted = redact(text);
        let prefix = "sk-ant-api03-";
        let expected = format!("{}{}", prefix, "x".repeat(text.len() - prefix.len()));
        assert_eq!(redacted, expected);
    }

    #[test]
    fn test_redact_slack() {
        let text = "xoxb-123456789012-123456789012-abcdefghijklmnopqrstuvwx";
        let redacted = redact(text);
        assert_eq!(
            redacted,
            "xoxb-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
        );
    }

    #[test]
    fn test_redact_github() {
        let text = "ghp_1234567890abcdef1234567890abcdef1234";
        let redacted = redact(text);
        assert_eq!(redacted, "ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");
    }

    #[test]
    fn test_redact_stripe() {
        let text = "sk_live_1234567890abcdef12345678";
        let redacted = redact(text);
        assert_eq!(redacted, "sk_live_xxxxxxxxxxxxxxxxxxxxxxxx");
    }

    #[test]
    fn test_redact_openrouter() {
        let text = "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";
        let redacted = redact(text);
        assert_eq!(
            redacted,
            "sk-or-v1-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
        );
    }

    #[test]
    fn test_aws_secret_length_based() {
        // 40-char alphanumeric word → should be redacted
        let text = "my key is abcdefghijklmnopqrstuvwxyz0123456789abcd";
        let redacted = redact(text);
        assert_eq!(
            redacted,
            "my key is xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
        );
    }

    #[test]
    fn test_path_not_redacted_by_aws_secret() {
        // Path with username and slashes — no single word is 40 chars
        let text = "/Users/dineshbhattarai/src/dineshdb/pie/src/handler.rs";
        let redacted = redact(text);
        assert_eq!(redacted, text);
    }

    #[test]
    fn test_short_word_not_redacted() {
        // 15-char word (dineshbhattarai) — not 40, should stay
        let text = "user dineshbhattarai logged in";
        let redacted = redact(text);
        assert_eq!(redacted, text);
    }

    #[test]
    fn test_40_char_word_in_path_segment_redacted() {
        // A single 40-char path segment — extremely unlikely but if it happens,
        // it's probably a secret
        let text = "/home/abcdefghijklmnopqrstuvwxyz0123456789abcd/file";
        let expected = "/home/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/file";
        let redacted = redact(text);
        assert_eq!(redacted, expected);
    }
}
