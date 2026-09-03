default: check

# Build debug
build:
    cargo build

# Build release
release:
    cargo build --release

# Run all tests
test:
    cargo test

# Run unit tests only (fast)
test-unit:
    cargo test --lib

# Run the same gates CI runs
check:
    cargo fmt --all -- --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo test --all-features

# Format code
fmt:
    cargo fmt

# Lint with clippy
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Run the MCP server
run:
    cargo run

# Advisories, licences, duplicate versions and registries
audit:
    cargo deny check

# Clean build artifacts
clean:
    cargo clean

# Install the git hooks through lefthook
setup:
    @command -v lefthook >/dev/null 2>&1 || { echo "lefthook is missing: brew install lefthook"; exit 1; }
    @command -v gitleaks >/dev/null 2>&1 || { echo "gitleaks is missing: brew install gitleaks"; exit 1; }
    git config --unset-all core.hooksPath || true
    lefthook install
    @echo "Hooks installed. lefthook validate checks the configuration."
