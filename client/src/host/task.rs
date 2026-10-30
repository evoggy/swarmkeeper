//! Tasks and time: tokio's here. A browser build runs the futures on the
//! page's event loop and takes the time from the browser instead.

pub use std::time::Instant;
pub use tokio::spawn;
pub use tokio::task::{JoinHandle, JoinSet};
pub use tokio::time::{interval, sleep, timeout};

/// A thread started with [`spawn_thread`].
pub type ThreadHandle = std::thread::JoinHandle<()>;

/// Run blocking work, such as a long computation, on a thread of its own.
pub fn spawn_thread<F>(f: F) -> std::thread::JoinHandle<()>
where
    F: FnOnce() + Send + 'static,
{
    std::thread::spawn(f)
}
