//! Run every module in its own thread and restart any that stops.
//!
//! The Python version ran one process per module.  Here a module is a
//! thread; a panic or an error return is caught, logged, and the module is
//! started again -- after a 5-second guard if it died right after starting,
//! so a module that cannot start does not spin.  systemd still restarts the
//! whole program (`Restart=always`) if it exits.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::thread;
use std::time::{Duration, Instant};

pub type Task = Box<dyn Fn() -> Result<(), String> + Send + Sync + 'static>;

const RESTART_DELAY: Duration = Duration::from_secs(5);

/// Start `task` under supervision in a named thread; returns at once.
pub fn spawn(name: &str, task: Task) {
    let name = name.to_string();
    let tname: String = name.chars().take(15).collect(); // Linux limit is 15 bytes
    let builder = thread::Builder::new().name(tname);
    builder
        .spawn(move || {
            loop {
                log::info!("started module {name:?}");
                let started = Instant::now();
                let result = catch_unwind(AssertUnwindSafe(&*task));
                match result {
                    Ok(Ok(())) => log::error!("module {name:?} exited"),
                    Ok(Err(e)) => log::error!("module {name:?} failed: {e}"),
                    Err(p) => {
                        let msg = p
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_string()))
                            .unwrap_or_else(|| "panic".into());
                        log::error!("module {name:?} crashed: {msg}");
                    }
                }
                if started.elapsed() < RESTART_DELAY {
                    thread::sleep(RESTART_DELAY);
                }
            }
        })
        .expect("cannot spawn a thread");
}

/// Park the calling thread for ever (the supervisor threads do the work).
pub fn park_forever() -> ! {
    loop {
        thread::park();
    }
}
