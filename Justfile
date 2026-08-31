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

# Cross-build the in-guest supervisor. Needs no C toolchain: rust-lld links a
# fully static musl binary, and the guest rootfs may have a different libc.
piebox-guest:
    #!/usr/bin/env bash
    set -euo pipefail
    # macOS reports arm64 where Rust says aarch64.
    ARCH="$(uname -m)"; [ "$ARCH" = arm64 ] && ARCH=aarch64
    TRIPLE="$ARCH-unknown-linux-musl"
    rustup target add "$TRIPLE"
    cargo build -p piebox-guest --release --target "$TRIPLE" \
        --config "target.$TRIPLE.linker=\"rust-lld\""
    file "target/$TRIPLE/release/piebox-guest"

# Build piebox and (on macOS) sign it with the hypervisor entitlement, without
# which libkrun's hv_vm_create() fails and no guest can start.
piebox profile="debug":
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "{{profile}}" = "release" ]; then
        cargo build -p piebox --release
    else
        cargo build -p piebox
    fi
    BIN="target/{{profile}}/piebox"
    if [ "$(uname -s)" = "Darwin" ]; then
        codesign -s - -f --entitlements crates/piebox/piebox.entitlements "$BIN"
        echo "signed $BIN"
    fi
    "$BIN" doctor
