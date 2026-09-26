install:
    cargo install --path . -f
    just sign

# Re-apply the local code-signing identity to the installed binaries. Cargo
# produces an ad-hoc signature, which the macOS application firewall treats
# as unidentified — signed binaries are auto-allowed to receive inbound
# connections, unsigned ones are silently dropped.
sign IDENTITY="pie-local-codesign":
    codesign --force --sign "{{IDENTITY}}" "$HOME/.local/share/cargo/bin/pie"

test:
    repo test
    test.py

lint:
    cargo clippy --fix --allow-dirty --allow-staged
