use std::sync::OnceLock;

use tokio::runtime::Runtime;

/// The tokio runtime the core runs on. GTK owns the main thread, so core
/// work happens on these threads and results come back as relm4 messages.
pub fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("burrow-core")
            .enable_all()
            .build()
            .expect("failed to start the tokio runtime")
    })
}
