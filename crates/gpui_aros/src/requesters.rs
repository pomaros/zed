// SPDX-License-Identifier: MIT
// Copyright (c) 2026 John Knipper

use std::io;
use std::thread::{self, JoinHandle};

#[derive(Default)]
pub(crate) struct RequestThreads {
    threads: Vec<JoinHandle<()>>,
    closed: bool,
}

impl RequestThreads {
    pub(crate) fn close_admission(&mut self) {
        self.closed = true;
    }

    pub(crate) fn spawn(&mut self, work: impl FnOnce() + Send + 'static) -> io::Result<()> {
        if self.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "application is shutting down"));
        }
        self.threads.try_reserve(1).map_err(io::Error::other)?;
        let thread = thread::Builder::new()
            .name("gpui-aros-requester".into())
            .stack_size(2 * 1024 * 1024)
            .spawn(work)?;
        self.threads.push(thread);
        Ok(())
    }

    /// Reap only completed threads: a requester waiting for input must not
    /// block painting. Keep the application alive until its worker has left
    /// native code and completed Rust thread cleanup before unloading it.
    pub(crate) fn reap_finished(&mut self) -> (bool, usize) {
        let mut panics = 0;
        let mut index = 0;
        while index < self.threads.len() {
            if self.threads[index].is_finished() {
                let thread = self.threads.swap_remove(index);
                if thread.join().is_err() {
                    panics += 1;
                }
            } else {
                index += 1;
            }
        }
        (self.threads.is_empty(), panics)
    }
}

#[cfg(unix)]
pub(crate) fn path_cstring(path: &std::path::Path) -> Result<std::ffi::CString, std::ffi::NulError> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
}

#[cfg(unix)]
pub(crate) fn path_from_bytes(bytes: &[u8]) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes.to_vec()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn waiting_requester_does_not_block_the_caller_or_allow_shutdown() {
        let (ready, started) = mpsc::channel();
        let (release, waiting) = mpsc::channel();
        let (result, completed) = mpsc::channel();
        let mut requests = RequestThreads::default();
        requests.spawn(move || {
            ready.send(()).expect("test caller exists");
            waiting.recv().expect("test releases the dialog");
            result.send("selected").expect("test receiver exists");
        }).expect("spawn requester");
        started.recv_timeout(Duration::from_secs(2)).expect("requester started");
        // Model foreground event turns while the native dialog waits.
        for _ in 0..100 {
            assert_eq!(requests.reap_finished(), (false, 0));
            assert!(completed.try_recv().is_err());
        }
        release.send(()).expect("requester exists");
        assert_eq!(completed.recv_timeout(Duration::from_secs(2)).expect("result"), "selected");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !requests.reap_finished().0 {
            assert!(Instant::now() < deadline, "requester must finish and join");
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(requests.reap_finished(), (true, 0));
    }

    #[test]
    fn shutdown_reentry_cannot_start_an_unjoined_requester() {
        let mut requests = RequestThreads::default();
        requests.close_admission();
        assert_eq!(requests.reap_finished(), (true, 0));
        // A quit observer can reenter Platform after the run loop exits.
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_called = called.clone();
        let error = requests.spawn(move || {
            worker_called.store(true, std::sync::atomic::Ordering::SeqCst);
        }).expect_err("shutdown admission is closed");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(requests.reap_finished(), (true, 0));
    }

    #[cfg(unix)]
    #[test]
    fn paths_preserve_utf8_and_non_utf8_dos_bytes() {
        for bytes in [b"RAM:dossier/\xc3\xa9.txt".as_slice(), b"WORK:legacy/\xe9.txt".as_slice()] {
            let path = path_from_bytes(bytes);
            assert_eq!(path_cstring(&path).expect("DOS path").as_bytes(), bytes);
        }
        assert!(path_cstring(&path_from_bytes(b"RAM:bad\0path")).is_err());
    }
}
