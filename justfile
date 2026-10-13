# Development tasks. `just --list` shows everything.

# Default: run all checks
default: check test

# Format code
fmt:
    cargo fmt --all

# Check formatting (CI)
fmt-check:
    cargo fmt --all -- --check

# Run clippy; warnings are errors
check:
    cargo clippy --all-targets -- -D warnings

# Unit tests, no database needed (`just test guard` runs a subset)
test *args:
    cargo test --lib {{args}}

# Start and seed the test databases: Postgres, MySQL and SQL Server in containers, plus a SQLite file
dbs-up:
    scripts/test-dbs.sh up

# Say which test databases are running
dbs-status:
    scripts/test-dbs.sh status

# Remove the test databases
dbs-down:
    scripts/test-dbs.sh down

# With `just dbs-up` running, the tests use those databases and leave them up, which is quicker when
# iterating. Otherwise they get fresh ones that are removed afterwards.
# Engine tests against real Postgres, MySQL, SQL Server and SQLite (`just test-dbs postgres`)
test-dbs *args:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! scripts/test-dbs.sh status >/dev/null 2>&1; then
        scripts/test-dbs.sh up
        trap 'scripts/test-dbs.sh down' EXIT
    fi
    scripts/test-dbs.sh test {{args}}

# Full CI pipeline locally
ci: fmt-check check test test-dbs

# Run kikkert against the test databases, starting them if needed (`just dev demo-postgres`)
dev *args:
    scripts/test-dbs.sh status >/dev/null 2>&1 || scripts/test-dbs.sh up
    cargo run -- --config scripts/demo.toml {{args}}

# Run kikkert with your own config (`just run orders-prod`, `just run --check orders-prod`)
run *args:
    cargo run -- {{args}}

# Build in release mode
build:
    cargo build --release

# Install kikkert into ~/.cargo/bin
install:
    cargo install --path . --locked

# Clean build artifacts
clean:
    cargo clean
