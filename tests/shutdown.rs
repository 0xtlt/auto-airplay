#![cfg(unix)]

use auto_airplay::shutdown::ShutdownFlag;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::low_level::raise;

// Both signals are covered in one test: this binary is its own process, and
// the registered handlers stay installed for its whole lifetime.
#[test]
fn records_sigterm_and_sigint_synchronously() {
    let first = ShutdownFlag::register().unwrap();
    assert!(!first.is_requested());

    raise(SIGTERM).unwrap();
    assert!(first.is_requested(), "SIGTERM must be visible right away");

    let second = ShutdownFlag::register().unwrap();
    assert!(!second.is_requested());
    raise(SIGINT).unwrap();
    assert!(second.is_requested(), "SIGINT must be visible right away");
}
