//! Instruction parsing — extract identifiable mentions from text.
//!
//! Pure parsing with no I/O or registry dependencies.
//! Resolution against skill/agent registries happens separately.

use std::collections::HashSet;

/// Parsed instructions that extract all identifiable `/name` mentions from text.
///
/// Does not know whether `/name` refers to a skill or an agent.
/// That determination requires registry lookup, done externally.
#[derive(Debug, Clone)]
pub struct Instructions {
    /// The raw input text.
    pub raw: String,
    /// All `/name` mentions found in the input.
    pub mentions: HashSet<String>,
}

impl Instructions {
    /// Parse a text string, extracting all `/name` mentions.
    ///
    /// A `/mention` is a `/` followed by `[a-zA-Z][a-zA-Z0-9_-]*`,
    /// preceded by whitespace, start of string, or punctuation
    /// (but not `/` or `:`, which indicate URLs or paths).
    pub fn new(input: impl Into<String>) -> Self {
        let raw = input.into();
        let mentions = Self::extract_mentions(&raw);
        Self { raw, mentions }
    }

    fn extract_mentions(input: &str) -> HashSet<String> {
        let mut mentions = HashSet::new();
        let bytes = input.as_bytes();
        let len = bytes.len();
        let mut i = 0;

        while i < len {
            let Some(&b'/') = bytes.get(i) else {
                i += 1;
                continue;
            };

            // Boundary: preceded by start, whitespace, or punctuation (not '/' or ':')
            if let Some(&prev) = i.checked_sub(1).and_then(|j| bytes.get(j)) {
                if prev == b'/' || prev == b':' {
                    i += 1;
                    continue;
                }
                if !prev.is_ascii_whitespace() && !prev.is_ascii_punctuation() {
                    i += 1;
                    continue;
                }
            }

            // Mention starts with [a-zA-Z]
            let start = i + 1;
            match bytes.get(start) {
                Some(first) if first.is_ascii_alphabetic() => {
                    // Collect [a-zA-Z0-9_-]*
                    let mut end = start;
                    while let Some(&c) = bytes.get(end) {
                        if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' {
                            end += 1;
                        } else {
                            break;
                        }
                    }
                    mentions.insert(input[start..end].to_string());
                    i = end;
                }
                _ => {
                    i += 1;
                }
            }
        }

        mentions
    }

    /// Merge mentions from additional text into this instance.
    /// Does not change `raw` — only augments the mention set.
    pub fn merge_mentions(&mut self, extra: &str) {
        self.mentions.extend(Self::extract_mentions(extra));
    }
}

impl From<&str> for Instructions {
    fn from(input: &str) -> Self {
        Self::new(input)
    }
}

impl From<String> for Instructions {
    fn from(input: String) -> Self {
        Self::new(input)
    }
}

impl std::fmt::Display for Instructions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}

impl serde::Serialize for Instructions {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mentions(input: &str) -> HashSet<String> {
        Instructions::from(input).mentions.clone()
    }

    #[test]
    fn single_mention() {
        let m = mentions("/review this file");
        assert!(m.contains("review"));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn multiple_mentions() {
        let m = mentions("/review and /explore");
        assert!(m.contains("review"));
        assert!(m.contains("explore"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn duplicate_mentions_deduped() {
        let m = mentions("/review and /review again");
        assert_eq!(m.len(), 1);
        assert!(m.contains("review"));
    }

    #[test]
    fn no_mentions() {
        let m = mentions("hello world");
        assert!(m.is_empty());
    }

    #[test]
    fn url_does_not_extract() {
        let m = mentions("see https://example.com/path");
        assert!(m.is_empty(), "URLs should not produce mentions");
    }

    #[test]
    fn double_slash_path_skipped() {
        let m = mentions("look at //usr/bin");
        assert!(m.is_empty(), "// should not produce mentions");
    }

    #[test]
    fn colon_slash_skipped() {
        let m = mentions("file:///path/to/thing");
        assert!(m.is_empty(), ":/// should not produce mentions");
    }

    #[test]
    fn embedded_in_word_ignored() {
        let m = mentions("a/review/b");
        assert!(
            !m.contains("review"),
            "/ embedded in a word should not extract"
        );
    }

    #[test]
    fn hyphenated_names() {
        let m = mentions("/code-review this");
        assert!(m.contains("code-review"));
    }

    #[test]
    fn underscore_names() {
        let m = mentions("/my_skill here");
        assert!(m.contains("my_skill"));
    }

    #[test]
    fn mention_at_end_of_string() {
        let m = mentions("use /explore");
        assert!(m.contains("explore"));
    }

    #[test]
    fn mention_before_punctuation() {
        let m = mentions("run /review.");
        assert!(m.contains("review"));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn mention_before_comma() {
        let m = mentions("use /review, then /explore");
        assert!(m.contains("review"));
        assert!(m.contains("explore"));
    }

    #[test]
    fn mention_in_parens() {
        let m = mentions("use (/review) now");
        assert!(m.contains("review"));
    }

    #[test]
    fn mention_in_brackets() {
        let m = mentions("see [/explore] for details");
        assert!(m.contains("explore"));
    }

    #[test]
    fn quoted_mention() {
        let m = mentions("he said \"/review\" loudly");
        assert!(m.contains("review"));
    }

    #[test]
    fn numeric_start_ignored() {
        let m = mentions("use /123abc");
        assert!(
            !m.contains("123abc"),
            "mentions must start with alphabetic char"
        );
    }

    #[test]
    fn empty_input() {
        let m = mentions("");
        assert!(m.is_empty());
    }

    #[test]
    fn slash_only() {
        let m = mentions("/");
        assert!(m.is_empty());
    }

    #[test]
    fn complex_real_world_query() {
        let m = mentions(
            "/review the changes using /developer skill, see https://github.com/foo/bar/pull/42 for context",
        );
        assert!(m.contains("review"));
        assert!(m.contains("developer"));
        assert_eq!(m.len(), 2, "should only extract the two real mentions");
    }

    #[test]
    fn many_mentions_all_captured() {
        let m = mentions("/a /b /c /d /e /f /g /h /i /j");
        assert_eq!(m.len(), 10);
    }

    #[test]
    fn display_trait() {
        let instr = Instructions::new("/review this");
        assert_eq!(format!("{instr}"), "/review this");
    }
}
