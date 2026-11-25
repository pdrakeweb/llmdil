//! One manager per data directory.
//!
//! The running manager holds a named pipe whose name is derived from its
//! data directory, so isolated copies with another `%APPDATA%` run side by
//! side. A second launch on the same directory connects to the pipe, which
//! makes the first one show its window, and exits before it touches a
//! file. The name is released by the OS when the holder exits, however it
//! exits. Windows only; elsewhere every launch is the first.

use std::path::Path;

/// What [`acquire`] found.
pub enum Launch {
    /// This process owns the data directory. Keep the guard, and give it
    /// to [`InstanceGuard::serve`] once the window exists.
    First(InstanceGuard),
    /// Another manager owns it (and has been asked to show its window).
    AlreadyRunning,
}

#[cfg(windows)]
pub use windows::{acquire, InstanceGuard};

/// Nothing to hold on this platform.
#[cfg(not(windows))]
pub struct InstanceGuard;

#[cfg(not(windows))]
impl InstanceGuard {
    pub async fn serve(self, _accept: impl Fn() -> bool, _show: impl Fn()) {}
}

#[cfg(not(windows))]
pub async fn acquire(_data_dir: &Path) -> Launch {
    Launch::First(InstanceGuard)
}

#[cfg(windows)]
mod windows {
    use std::time::{Duration, Instant};

    use sha2::{Digest, Sha256};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

    use super::{Launch, Path};

    /// How long a second launch waits for the first to answer, or (a
    /// relaunch) to finish exiting and release the name.
    const WAIT: Duration = Duration::from_secs(10);
    /// How long the first one lets a client read its answer.
    const ANSWER_READ: Duration = Duration::from_secs(2);
    const SHOWN: u8 = 1;

    /// The held pipe name.
    pub struct InstanceGuard {
        name: String,
        server: NamedPipeServer,
    }

    /// The pipe name of `data_dir`, independent of case, `/` versus `\`
    /// and a trailing separator.
    pub(super) fn pipe_name(data_dir: &Path) -> String {
        let path = std::path::absolute(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
        let path = path.to_string_lossy().to_lowercase();
        let digest = Sha256::digest(path.trim_end_matches('\\').as_bytes());
        format!(r"\\.\pipe\llmdil-manager-{digest:x}")
    }

    /// Claim `data_dir` for this process, or have the manager that already
    /// holds it show its window.
    pub async fn acquire(data_dir: &Path) -> Launch {
        acquire_within(data_dir, WAIT).await
    }

    pub(super) async fn acquire_within(data_dir: &Path, wait: Duration) -> Launch {
        let name = pipe_name(data_dir);
        let deadline = Instant::now() + wait;
        loop {
            // Atomic: creating the first instance fails while any process
            // holds the name.
            if let Ok(server) = ServerOptions::new().first_pipe_instance(true).create(&name) {
                return Launch::First(InstanceGuard { name, server });
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if matches!(
                tokio::time::timeout(left, ask_to_show(&name)).await,
                Ok(true)
            ) || Instant::now() >= deadline
            {
                return Launch::AlreadyRunning;
            }
            // The holder is exiting (or between two pipe instances).
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Connect to the holder; true once it answered. Read access is
    /// enough, so a holder running elevated answers too.
    async fn ask_to_show(name: &str) -> bool {
        match ClientOptions::new().write(false).open(name) {
            Ok(mut pipe) => matches!(pipe.read_u8().await, Ok(SHOWN)),
            Err(_) => false,
        }
    }

    impl InstanceGuard {
        /// Answer second launches for the life of the process: while
        /// `accept` (not exiting) holds, `show` runs and the launch is told
        /// it may exit. The name stays held even when a connection fails.
        pub async fn serve(self, accept: impl Fn() -> bool, show: impl Fn()) {
            let Self { name, mut server } = self;
            loop {
                let connected = server.connect().await.is_ok();
                // The next instance exists before this one is dropped, so
                // the name is never free.
                let next = loop {
                    match ServerOptions::new().create(&name) {
                        Ok(next) => break next,
                        Err(e) => {
                            tracing::warn!("single instance: {e}");
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                };
                let mut client = std::mem::replace(&mut server, next);
                if connected && accept() {
                    show();
                    tokio::spawn(async move {
                        let _ = client.write_u8(SHOWN).await;
                        // Until the client closes its end.
                        let _ = tokio::time::timeout(ANSWER_READ, client.read(&mut [0u8; 1])).await;
                    });
                }
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::windows::{acquire_within, pipe_name};
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn the_name_identifies_the_data_directory() {
        let name = pipe_name(Path::new(r"C:\Data\App"));
        assert_eq!(name, pipe_name(Path::new("c:/data/app/")));
        assert_ne!(name, pipe_name(Path::new(r"C:\Data\App2")));
        assert!(!name.contains("Data"), "no path in a machine-wide name");
    }

    #[tokio::test]
    async fn a_second_launch_shows_the_first_and_is_turned_away() {
        let dir = crate::test_support::unique_test_dir("llm-mgr-single");
        let Launch::First(guard) = acquire(&dir).await else {
            panic!("nothing holds a fresh directory");
        };
        let shown = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&shown);
        tokio::spawn(guard.serve(
            || true,
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
            },
        ));
        for n in 1..=3 {
            let started = Instant::now();
            assert!(matches!(acquire(&dir).await, Launch::AlreadyRunning));
            assert!(started.elapsed() < Duration::from_secs(5));
            assert_eq!(shown.load(Ordering::SeqCst), n);
        }
        // Another data directory is unaffected, and nothing was written.
        let other = crate::test_support::unique_test_dir("llm-mgr-single");
        assert!(matches!(acquire(&other).await, Launch::First(_)));
        assert!(!dir.exists() && !other.exists());
    }

    /// A relaunch while the old process is still exiting gets the
    /// directory once it is gone, and is never told to exit.
    #[tokio::test]
    async fn a_relaunch_waits_for_an_exiting_holder() {
        let dir = crate::test_support::unique_test_dir("llm-mgr-single");
        let Launch::First(guard) = acquire(&dir).await else {
            panic!("nothing holds a fresh directory");
        };
        let shown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shown);
        let holder =
            tokio::spawn(guard.serve(|| false, move || flag.store(true, Ordering::SeqCst)));
        let relaunch = {
            let dir = dir.clone();
            tokio::spawn(async move { acquire(&dir).await })
        };
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!relaunch.is_finished());
        holder.abort();
        assert!(matches!(relaunch.await.unwrap(), Launch::First(_)));
        assert!(!shown.load(Ordering::SeqCst));
    }

    /// A holder that never answers still keeps a second manager out.
    #[tokio::test]
    async fn a_hung_holder_is_never_joined() {
        let dir = crate::test_support::unique_test_dir("llm-mgr-single");
        let _guard = acquire(&dir).await;
        let second = acquire_within(&dir, Duration::from_millis(300)).await;
        assert!(matches!(second, Launch::AlreadyRunning));
    }
}
