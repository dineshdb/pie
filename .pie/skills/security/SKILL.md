---
name: security
description: Secure coding patterns and security scanning tools.
---

# Security

Two modes: **proactive** (check while writing/reviewing) and **reactive** (scan on demand).

## OWASP Top 10 — Quick Prevention

| Vulnerability      | Prevention                          |
| ------------------ | ------------------------------------ |
| SQL Injection      | Parameterized queries, never concat  |
| XSS                | Output encoding, CSP headers         |
| Command Injection  | Avoid shell, use language APIs       |
| Path Traversal     | Validate and sanitize file paths     |
| SSRF               | URL allowlists, no raw user URLs     |
| Hardcoded Secrets  | Env vars or secret managers only     |
| Insecure Crypto    | SHA-256+, AES-256, no MD5/DES        |
| Insecure Transport | HTTPS everywhere, verify certs       |
| XXE                | Disable external entities in XML     |
| CSRF               | Tokens on state-changing requests    |

## Language Priority Rules

| Language | Check First                                        |
| -------- | -------------------------------------------------- |
| Python   | SQL injection, command injection, path traversal   |
| JS/TS    | XSS, prototype pollution, code injection           |
| Rust     | Memory safety, unsafe blocks, command injection    |
| Go       | SQL injection, command injection, path traversal   |

## Dependency Audit

```bash
npm audit                    # Node.js
npm audit fix                # Auto-fix
bun pm audit                 # Bun
cargo audit                  # Rust
pip-audit                    # Python
govulncheck ./...            # Go
```

## Secrets Detection

```bash
# Gitleaks
gitleaks detect --source . --report-path report.json

# TruffleHog
trufflehog filesystem ./

# Quick ripgrep scan
rg -i "AKIA[0-9A-Z]{16}" .              # AWS keys
rg "ghp_[a-zA-Z0-9]{36}" .              # GitHub tokens
rg "xox[pbar]-[a-zA-Z0-9-]{14,255}" .   # Slack tokens
rg -i "api[_-]?key['\"]?\s*[:=]" .      # API keys
rg -i "(mongo|mysql|postgres|redis)://[^ ]+:[^ ]+@" .  # DB URLs
```

## Static Analysis

```bash
# Semgrep (multi-language)
semgrep scan --config auto
semgrep scan --config p/security
semgrep scan --json --output report.json

# Bandit (Python)
bandit -r . -f json -o report.json

# CodeQL
codeql database create db --language=javascript
codeql database analyze db --format=sarif-latest --output results.sarif
```

## Container & Infrastructure

```bash
# Container scanning
trivy image python:3.11
grype node:18

# Terraform
tfsec . --format json --out report.json

# Kubernetes
kube-score score manifests/*.yaml
checkov -d k8s/
```

## Security Headers

```http
Strict-Transport-Security: max-age=31536000; includeSubDomains
X-Content-Type-Options: nosniff
X-Frame-Options: DENY
Content-Security-Policy: default-src 'self'
Referrer-Policy: no-referrer
```

```bash
# Check headers
curl -I https://example.com | grep -i "x-\|strict-\|content-\|referrer"
```

## CI/CD Integration

```yaml
# GitHub Actions security scan
- run: npm audit
- uses: trufflesecurity/trufflehog@main
- uses: returntocorp/semgrep-action@v1
  with:
    config: auto
```

## Pre-deployment Checklist

- [ ] Dependency audit passed
- [ ] No secrets in code (gitleaks)
- [ ] Security headers configured
- [ ] Input validation in place
- [ ] Output encoding implemented
- [ ] Auth/AuthZ tested
- [ ] Error handling doesn't leak info
- [ ] HTTPS enforced
