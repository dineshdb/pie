install:
    cargo install --path . -f
    just sign
    just sign

# Re-apply the local code-signing identity to the installed binaries. Cargo
# produces an ad-hoc signature, which the macOS application firewall treats
# as unidentified — signed binaries are auto-allowed to receive inbound
# connections, unsigned ones are silently dropped.
sign IDENTITY="pie-local-codesign":
    codesign --force --sign "{{IDENTITY}}" "$HOME/.local/share/cargo/bin/pie"
    codesign --force --sign "{{IDENTITY}}" "$HOME/.local/share/cargo/bin/mem"

prepare:
    #!/usr/bin/env bash
    set -euo pipefail
    DB_PATH=target/sqlx_prepare.db
    rm -f "$DB_PATH"
    for file in crates/pie-core/src/db/migrations/*.sql; do
        echo "Applying migration: $file"
        sqlite3 "$DB_PATH" < "$file"
    done
    DATABASE_URL="sqlite:$DB_PATH" cargo sqlx prepare
    rm -f "$DB_PATH"

test:
    repo test
    test.py

lint:
    cargo clippy --fix --allow-dirty --allow-staged
