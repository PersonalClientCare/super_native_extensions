use std::{
    sync::mpsc::{self, RecvTimeoutError},
    time::Duration,
};

/// Runs `check` on a fresh thread and fails the test unless it returns true within `limit`.
pub(crate) fn run_on_thread(limit: Duration, check: impl FnOnce() -> bool + Send + 'static) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(check());
    });
    match rx.recv_timeout(limit) {
        Ok(true) => {}
        Ok(false) => panic!("check failed on the test thread"),
        Err(RecvTimeoutError::Timeout) => panic!("test body did not return in time"),
        Err(RecvTimeoutError::Disconnected) => {
            panic!("test thread panicked before reporting")
        }
    }
}
