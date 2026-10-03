//! The I/O side of the app: a multi-threaded tokio runtime running every
//! service (library, sources, haptics, remote APIs, updater). The render
//! thread talks to it only through channels ([`services::Services`]).

pub mod services;

/// Build the I/O runtime. Worker count is modest: the Snapdragon's big
/// cores are better spent on decode and rendering.
pub fn build_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .max_blocking_threads(16)
        .thread_name("fp-io")
        .enable_all()
        .build()
}
