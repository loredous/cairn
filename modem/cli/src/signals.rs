//! SIGINT, SIGTERM and SIGHUP: set [`STOP`], release every registered PTT
//! at once from the signal thread, and exit if the main thread has not
//! finished within a few seconds (a second signal exits immediately). A
//! panic hook releases too, before the unwinding guards do it again.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use cairn_ptt::SharedPtt;

/// Set by the first signal; long-running loops poll it.
pub static STOP: AtomicBool = AtomicBool::new(false);

static PTTS: Mutex<Vec<SharedPtt>> = Mutex::new(Vec::new());

fn ptts() -> MutexGuard<'static, Vec<SharedPtt>> {
    PTTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Release this PTT on a signal or panic, too.
pub fn register_ptt(p: &SharedPtt) {
    ptts().push(p.clone());
}

fn release_all() {
    for p in ptts().iter() {
        p.release();
    }
}

/// Install the handlers.
pub fn install() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // try_lock: the panic may have happened while a PTT call held its lock.
        if let Ok(list) = PTTS.try_lock() {
            for p in list.iter() {
                p.try_release();
            }
        }
        prev(info);
    }));
    #[cfg(unix)]
    unix::install();
}

#[cfg(unix)]
mod unix {
    use super::*;
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;
    use std::time::Duration;

    pub fn install() {
        let mut signals = match Signals::new([SIGINT, SIGTERM, SIGHUP]) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("warning: cannot install signal handlers: {e}");
                return;
            }
        };
        std::thread::Builder::new()
            .name("cairn-signals".into())
            .spawn(move || {
                for sig in signals.forever() {
                    if STOP.swap(true, Ordering::SeqCst) {
                        release_all();
                        std::process::exit(128 + sig);
                    }
                    release_all();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(3));
                        eprintln!("not stopping cleanly; PTT released, exiting");
                        release_all();
                        std::process::exit(128 + sig);
                    });
                }
            })
            .expect("spawn signal thread");
    }
}
