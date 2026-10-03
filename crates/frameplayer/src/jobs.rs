//! Background work: anything that may block (network listings, opening
//! media, scans, update checks) runs on a thread and reports back here.

use crossbeam_channel::{Receiver, Sender, unbounded};

pub struct Jobs<T> {
    tx: Sender<T>,
    rx: Receiver<T>,
}

impl<T: Send + 'static> Jobs<T> {
    pub fn new() -> Jobs<T> {
        let (tx, rx) = unbounded();
        Jobs { tx, rx }
    }

    /// Runs `f` on a new thread; its result is delivered by [`Jobs::poll`].
    pub fn spawn(&self, name: &str, f: impl FnOnce() -> T + Send + 'static) {
        let tx = self.tx.clone();
        let r = std::thread::Builder::new()
            .name(format!("fp-job-{name}"))
            .spawn(move || {
                let _ = tx.send(f());
            });
        if let Err(e) = r {
            log::error!("cannot start job thread: {e}");
        }
    }

    /// Finished results, without blocking.
    pub fn poll(&self) -> Vec<T> {
        self.rx.try_iter().collect()
    }

    /// A sender for long-running workers that report more than once.
    pub fn sender(&self) -> Sender<T> {
        self.tx.clone()
    }
}

impl<T: Send + 'static> Default for Jobs<T> {
    fn default() -> Self {
        Jobs::new()
    }
}
