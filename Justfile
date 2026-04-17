install:
    cargo install --path . -f

test:
    repo test
    test.py

lint:
    cargo clippy --fix --allow-dirty --allow-staged
    repo lint
