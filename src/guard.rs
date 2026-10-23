//! Ctrl+C handling.
//!
//! Every file operation (a move, a tag write, and the db row that records it)
//! runs inside a [`Critical`] section. Ctrl+C never interrupts one:
//!
//! * first press: ask the run to stop after the current file;
//! * further presses outside a critical section: exit immediately (nothing is
//!   being written, so that's safe);
//! * further presses inside a critical section: exit the moment it finishes.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use anyhow::Result;

static STOP: AtomicBool = AtomicBool::new(false);
static PRESSES: AtomicUsize = AtomicUsize::new(0);
static EXIT_AFTER_CRITICAL: AtomicBool = AtomicBool::new(false);
/// Held for the whole duration of a critical section. The signal handler only
/// ever exits the process while holding it, so it can't exit mid-write.
static CRITICAL: Mutex<()> = Mutex::new(());

pub const EXIT_INTERRUPTED: i32 = 130;

pub fn install() -> Result<()> {
    ctrlc::set_handler(|| {
        let presses = PRESSES.fetch_add(1, Ordering::SeqCst) + 1;
        STOP.store(true, Ordering::SeqCst);
        match CRITICAL.try_lock() {
            Ok(_guard) if presses > 1 => {
                crate::ui::line("interrupted again, quitting now (no file operation was in progress)");
                tracing::warn!("second Ctrl+C outside a file operation, exiting immediately");
                std::process::exit(EXIT_INTERRUPTED);
            }
            Ok(_) => {
                crate::ui::line("stopping after the current step... (press Ctrl+C again to quit right away)");
                tracing::warn!("Ctrl+C received, stopping after the current step");
            }
            Err(_) => {
                if presses > 1 {
                    EXIT_AFTER_CRITICAL.store(true, Ordering::SeqCst);
                    crate::ui::line("still finishing a file operation, will quit the moment it is done");
                } else {
                    crate::ui::line("finishing the current file operation before stopping, please wait...");
                }
                tracing::warn!(presses, "Ctrl+C received during a file operation, finishing it first");
            }
        }
    })?;
    Ok(())
}

/// Stop after the current step, as a first Ctrl+C would.
pub fn request_stop() {
    STOP.store(true, Ordering::SeqCst);
}

pub fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// A section of work that must never be interrupted halfway.
pub struct Critical {
    guard: Option<MutexGuard<'static, ()>>,
}

impl Critical {
    pub fn enter() -> Self {
        let guard = CRITICAL.lock().unwrap_or_else(|e| e.into_inner());
        Critical { guard: Some(guard) }
    }
}

impl Drop for Critical {
    fn drop(&mut self) {
        drop(self.guard.take());
        if EXIT_AFTER_CRITICAL.load(Ordering::SeqCst) {
            // Re-take the lock so no other critical section can start, then go.
            let _hold = CRITICAL.lock().unwrap_or_else(|e| e.into_inner());
            tracing::warn!("file operation finished, exiting on repeated Ctrl+C");
            crate::ui::line("file operation finished, quitting");
            std::process::exit(EXIT_INTERRUPTED);
        }
    }
}
