# Jewels 💎

> [!IMPORTANT]
> **Jewels** is integrated inside **[pie](https://github.com/dineshdb/pie)** for real-time secret scanning and "jewel protection". It ensures that sensitive credentials never leak into LLM contexts, logs, or command history.

A powerful Rust library and CLI tool to scan and redact secrets from text and JSON.

Jewels helps you identify and mask sensitive information like API keys, tokens, and credentials, ensuring they don't leak into logs, history, or LLM contexts.

## Features

- **Broad Detection**: Built-in support for dozens of services (AWS, Google Cloud, GitHub, Slack, Stripe, Discord, OpenAI, Anthropic, etc.).
- **Smart Redaction**: Preserves identifiable prefixes (e.g., `sk-`, `ghp_`) so you know *what* was redacted without exposing the secret itself.
- **Length Matching**: Redacted output matches the length of the original secret, preserving document structure and alignment.
- **Recursive JSON Support**: Deeply scan and redact secrets within complex JSON structures.
- **Standalone CLI**: Use it in your shell pipelines for quick scanning and redaction.

## Installation

### CLI

```bash
cargo install jewels
```

### Library

Add this to your `Cargo.toml`:

```toml
[dependencies]
jewels = "0.1.0"
```

## Usage

### CLI

```bash
# Scan for secrets (outputs to stdout, status to stderr)
echo "My key is sk-1234567890abcdef1234567890abcdef1234567890abcdef" | jewels scan

# Redact secrets
echo "My key is sk-1234567890abcdef1234567890abcdef1234567890abcdef" | jewels redact
# Output: My key is sk-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

### Library

```rust
use jewels::{scan, redact};

let text = "My key is sk-1234567890abcdef1234567890abcdef1234567890abcdef";
let redacted = redact(text);

// Redacted output matches the original length!
assert_eq!(redacted, "My key is sk-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");

let matches = scan(text);
assert_eq!(matches[0].kind, "OpenAI API Key");
```

## Supported Secrets

Precious detects secrets from:
- **Cloud**: AWS, Google Cloud, Azure, Heroku, Terraform.
- **Dev Tools**: GitHub, GitLab, NPM, PyPI, Docker Hub.
- **Messaging**: Slack, Discord, Twilio, PagerDuty.
- **Payments**: Stripe, Square, SendGrid, Mailgun, Mailchimp.
- **Monitoring**: Datadog, Cloudflare, Postman, Bitly.
- **Generic**: Generic API keys, tokens, and secrets via pattern matching.

## License

MIT
