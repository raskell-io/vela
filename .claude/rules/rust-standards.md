# Rust Coding Standards

> Minimum Rust version: **1.93.0** (Edition 2024)

---

## Error Handling

### Use `thiserror` for library code, `anyhow` for application code

```rust
// Domain errors
#[derive(Debug, thiserror::Error)]
pub enum DeployError {
    #[error("health check failed after {attempts} attempts for {app}")]
    HealthCheckFailed { app: String, attempts: u32 },
    #[error("app '{0}' not found")]
    AppNotFound(String),
}

// CLI / main.rs
fn main() -> anyhow::Result<()> { ... }
```

### No `unwrap()` or `expect()` outside tests

Use `?` operator. Return `Result`.

---

## Async

- Tokio runtime, `async fn` everywhere on the server
- `tokio::select!` for concurrent operations
- `tokio::join!` for independent parallel work

---

## Type Design

- Newtypes for domain concepts (`AppName`, `ReleaseId`)
- Enums over booleans (`DeployStrategy::BlueGreen | Sequential`)
- `Cow<'_, str>` when ownership is conditional

---

## Testing

- `#[cfg(test)] mod tests` in same file
- `#[tokio::test]` for async tests
- No flaky tests — controlled clocks, deterministic IDs
- Property tests where applicable (proptest)

---

## Logging

Use `tracing`, never `println!` or `log`:

```rust
tracing::info!(app = %name, release = %id, "deploy started");
tracing::error!(app = %name, err = %e, "health check failed");
```

---

## Before Committing

```bash
cargo fmt
cargo clippy -- -D warnings
cargo test
```
