install:
    cargo install --path . -f

prepare:
    cargo sqlx prepare --workspace --all -- --all-targets

test:
    repo test
    test.py

lint:
    cargo clippy --fix --allow-dirty --allow-staged
    repo lint
