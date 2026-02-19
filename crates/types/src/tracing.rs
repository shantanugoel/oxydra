use std::sync::Once;

static TRACING_INIT: Once = Once::new();

pub fn init_tracing() {
    TRACING_INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_target(false)
            .with_ansi(false)
            .try_init();
    });
}
