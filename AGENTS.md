# Mira - HTTP/HTTPS Debugging Proxy

Mira is an HTTP/HTTPS intercepting proxy similar to Charles Proxy and mitmproxy,
written in Rust. It allows developers to inspect and debug HTTP traffic.

## Quick Reference

**Before committing, always run:**

```bash
cargo fmt --all                                                         # Format code
cargo clippy --workspace --all-features --all-targets -- -D warnings    # Lint (must pass)
cargo test                                                              # Run tests
```

All three commands must pass without errors before code is considered ready.

## Build & Run

```bash
cargo build                 # Build (debug)
cargo build --release       # Build (release, optimized)
cargo run -p mira-app       # Run the desktop window
cargo run -p mira-cli       # Run the terminal UI
RUST_LOG=debug cargo run -p mira-app   # Run with more logging
cargo check                 # Check without building
```

## Testing

```bash
cargo test                    # Run all tests
cargo test test_name          # Run a single test by name
cargo test module_name::      # Run tests in a specific module
cargo test -- --nocapture     # Run tests with output
cargo test --lib              # Run only unit tests (no integration tests)
```

## Code Quality

### Formatting

```bash
cargo fmt --all               # Format all code
cargo fmt --all -- --check    # Check formatting without changing files
```

- Max line width: 100 characters
- Use 4 spaces for indentation
- Rust 2024 edition, format style edition 2024

### Linting

```bash
cargo clippy --workspace --all-features --all-targets -- -D warnings
```

The project has strict clippy lints enabled in `[workspace.lints]` in the root `Cargo.toml`.

## Code Style Guidelines

### General Rules

- No `.unwrap()` in production code (allowed in tests) - use `?` or `.ok_or_else()`
- Use `parking_lot::{Mutex, RwLock}` instead of `std::sync` equivalents
- Use `Self` instead of repeating the type name
- Inline format args: `format!("{x}")` not `format!("{}", x)`

### Imports

Default behavior of `rustfmt`. Do not change the granularity of any imports and preserve the original structure written by the developer.

### Comments

Short sentences. RFC 2119 keywords for obligations. Commit = imperative subject; body only for a fact the diff cannot show. Comments only where code needs clarification — never narration.

### Struct and Enum Members

When members of a `struct`, `enum`, or similar item carry doc comments, separate each
member with a blank line. The comment and the member it describes read as one unit, and
without the blank line adjacent units run together.

Undocumented members need no blank line between them.

### Error Handling

- Use `thiserror` for library errors, `anyhow` for application errors
- Prefer `Result<T, E>` over panics
- Use `expect()` only when failure is truly impossible or during initialization
- Never use `unwrap()` in library code

### Logging

Use the `tracing` crate macros, imported one per line. Put values in structured
fields rather than in the message:

```rust
use tracing::debug;
use tracing::warn;

debug!(host, port, "establishing tunnel");
warn!(id = request_id, error = %e, "upstream failed");
```

Logs go to a file, never stdout, because the TUI owns the terminal.

### Documentation

- Document all public APIs with `///` doc comments
- Include examples in doc comments for complex functions
- Use `//!` for module-level documentation
- Keep comments concise and up-to-date

## Contributing

### Branches

Always create a branch before starting a new feature or fix. Never commit directly to
`main`. Each branch MUST cover one feature or fix only. A small, focused pull request is
reviewed much faster than a large one that mixes unrelated changes.

Branch names MUST start with a prefix that says what the branch does:

| Prefix      | Use for                                     |
| ----------- | ------------------------------------------- |
| `feature/`  | New functionality                           |
| `bugfix/`   | Fixes for non-urgent bugs                   |
| `hotfix/`   | Urgent fixes for released code              |
| `refactor/` | Code restructuring with no behavior change  |
| `docs/`     | Documentation only                          |
| `chore/`    | Tooling, dependencies, CI, and other upkeep |

Example: `feature/websocket-inspection`, `bugfix/tls-handshake-timeout`.

### Merging

Squash commits before merging. Each pull request MUST land on `main` as a single commit.
