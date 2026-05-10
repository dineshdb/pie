install:
    cargo install --path . -f

prepare:
    mkdir -p target/ && touch target/pie.db && DATABASE_URL="sqlite:target/pie.db" cargo sqlx prepare --workspace --all -- --all-targets

test:
    repo test
    test.py

lint:
    cargo clippy --fix --allow-dirty --allow-staged
    repo lint
