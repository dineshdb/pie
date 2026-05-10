install:
    cargo install --path . -f

prepare:
    #!/usr/bin/env bash
    set -euo pipefail
    DB_PATH=target/sqlx_prepare.db
    rm -f "$DB_PATH"
    for file in src/db/migrations/*.sql; do
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
    repo lint
