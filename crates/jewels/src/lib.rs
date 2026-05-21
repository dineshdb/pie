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
    };
}

use serde_json::Value;

define_secrets! {
    // --- Infrastructure & Cloud ---
    "AWS Access Key ID" => r"\b(AKIA)[0-9A-Z]{16}\b",
    "AWS Secret Access Key" => r"\b([0-9a-zA-Z+/]{4})[0-9a-zA-Z+/]{36}\b",
    "Google API Key" => r"\b(AIza)[0-9A-Za-z\-_]{35}\b",
    "Azure Storage Account Key" => r"\b([a-zA-Z0-9+/]{86}==)\b",
    "Azure DevOps Token" => r"\b([a-z0-9]{52})\b",
    "Heroku API Key" => r"\b([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})\b",
    "Terraform Cloud Token" => r"\b([a-zA-Z0-9]{40}\.atlasv1\.[a-zA-Z0-9]{60,})\b",

    // --- AI & LLM ---
    "OpenAI API Key" => r"\b(sk-)[a-zA-Z0-9]{48}\b",
    "Anthropic API Key" => r"\b(sk-ant-api03-)[a-zA-Z0-9\-_]{80,}\b",

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
    "Generic API Key" => r"(?i)\b((?:api[_-]?key|token|secret|password|credential)[\s:=]+)[a-zA-Z0-9\-_]{16,}\b",
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SecretMatch {
    pub kind: &'static str,
    pub value: String,
}

pub fn scan(text: &str) -> Vec<SecretMatch> {
    let mut matches = Vec::new();
    let mut found_ranges = Vec::new();

    for (kind, re) in SECRET_REGEXES.iter() {
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
    matches
}

pub fn redact(text: &str) -> String {
    let mut redacted = text.to_string();
    for (_kind, re) in SECRET_REGEXES.iter() {
        redacted = re
            .replace_all(&redacted, |caps: &regex::Captures| {
                let full_match = caps.get(0).map_or("", |m| m.as_str());
                if let Some(prefix) = caps.get(1) {
                    let prefix_str = prefix.as_str();
                    let mask_len = full_match.len().saturating_sub(prefix_str.len());
                    format!("{}{}", prefix_str, "x".repeat(mask_len))
                } else {
                    "x".repeat(full_match.len())
                }
            })
            .to_string();
    }
    redacted
}

pub fn redact_json(value: &Value) -> Value {
    match value {
        Value::String(s) => {
            let redacted = redact(s);
            if redacted != *s {
                Value::String(redacted)
            } else {
                value.clone()
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
}
