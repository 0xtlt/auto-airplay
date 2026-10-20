//! Shutdown requests recorded directly by the signal handler.
//!
//! Tokio forwards signals to `Signal::recv()` through its driver, so a signal
//! can be raised but not yet visible to an async listener. Before replacing
//! the process with `exec` (which would drop that pending notification), the
//! daemon checks this flag, which is set synchronously when the signal lands.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use signal_hook::consts::{SIGINT, SIGTERM};

#[derive(Debug, Clone)]
pub struct ShutdownFlag(Arc<AtomicBool>);

impl ShutdownFlag {
    /// Records SIGINT and SIGTERM from now on. This replaces the default
    /// "terminate" action, so a listener (e.g. Tokio's) must also handle them.
    pub fn register() -> io::Result<Self> {
        let flag = Arc::new(AtomicBool::new(false));
        for signal in [SIGINT, SIGTERM] {
            signal_hook::flag::register(signal, Arc::clone(&flag))?;
        }
        Ok(Self(flag))
    }

    pub fn is_requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}
