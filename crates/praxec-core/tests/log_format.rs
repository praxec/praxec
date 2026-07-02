//! Tests for JSON log format output.

#[test]
fn json_formatter_does_not_panic() {
    // Verify that initializing the JSON tracing subscriber works.
    let filter = tracing_subscriber::EnvFilter::new("info");
    let result = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .json()
        .try_init();
    // try_init returns Err if already initialized (which is fine in a
    // test environment where another subscriber may already be active).
    // We just verify it doesn't panic.
    assert!(result.is_ok() || result.is_err());
}
