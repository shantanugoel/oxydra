use std::sync::Once;

static TRACING_INIT: Once = Once::new();

/// Initialise the global tracing subscriber.
///
/// The log level is controlled by the `RUST_LOG` environment variable
/// (e.g. `RUST_LOG=debug` or `RUST_LOG=oxydra=debug,info`). When `RUST_LOG`
/// is not set the subscriber defaults to `INFO`.
///
/// This function is idempotent — subsequent calls after the first are no-ops.
pub fn init_tracing() {
    TRACING_INIT.call_once(|| {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_target(false)
            .with_ansi(false)
            .with_env_filter(filter)
            .try_init();
    });
}
