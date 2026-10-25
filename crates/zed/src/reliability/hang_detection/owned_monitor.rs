// SPDX-License-Identifier: MIT
// Copyright (c) 2026 John Knipper

//! An owned monitor whose idle waits can be interrupted by shutdown.
//!
//! The sampling callback must not wait for foreground work. A sample already
//! running finishes before join; this does not cancel arbitrary blocking I/O.
use std::io;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub(super) struct OwnedMonitor {
    stop: Option<Sender<()>>,
    worker: Option<JoinHandle<()>>,
    // Shutdown and destruction of worker panic data stay on the creator.
    not_send: PhantomData<Rc<()>>,
}

impl OwnedMonitor {
    pub(super) fn spawn(
        startup_delay: Duration,
        interval: Duration,
        mut sample: impl FnMut() + Send + 'static,
    ) -> io::Result<Self> {
        let (stop, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("HangLogging".into())
            .spawn(move || {
                match receiver.recv_timeout(startup_delay) {
                    Err(RecvTimeoutError::Timeout) => {}
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                }
                loop {
                    match receiver.recv_timeout(interval) {
                        Err(RecvTimeoutError::Timeout) => sample(),
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            })?;
        Ok(Self {
            stop: Some(stop),
            worker: Some(worker),
            not_send: PhantomData,
        })
    }

    pub(super) fn shutdown(&mut self) -> thread::Result<()> {
        if let Some(stop) = self.stop.take() {
            // A message wakes an idle receive. No timer tick or deadline must
            // expire for shutdown to proceed.
            let _ = stop.send(());
        }
        match self.worker.take() {
            Some(worker) => worker.join(),
            None => Ok(()),
        }
    }
}

impl Drop for OwnedMonitor {
    fn drop(&mut self) {
        // Also handles normal App teardown if the quit observer was not run.
        // The join result (including arbitrary panic payload Drop) stays here.
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };
    struct Dropped(Arc<AtomicUsize>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[test]
    fn stop_interrupts_startup_wait_without_deadline_expiry() {
        let drops = Arc::new(AtomicUsize::new(0));
        let witness = Dropped(drops.clone());
        let mut monitor = OwnedMonitor::spawn(
            Duration::from_secs(3600),
            Duration::from_secs(3600),
            move || {
                let _ = &witness;
                panic!("startup stop must not sample")
            },
        )
        .unwrap();
        monitor.shutdown().unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        monitor.shutdown().unwrap();
    }
    #[test]
    fn drop_without_quit_observer_stops_and_joins() {
        let drops = Arc::new(AtomicUsize::new(0));
        let witness = Dropped(drops.clone());
        drop(
            OwnedMonitor::spawn(
                Duration::from_secs(3600),
                Duration::from_secs(3600),
                move || {
                    let _ = &witness;
                },
            )
            .unwrap(),
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn stopped_monitor_does_not_sample_again() {
        let (sent, ready) = mpsc::channel();
        let samples = Arc::new(AtomicUsize::new(0));
        let count = samples.clone();
        let mut monitor =
            OwnedMonitor::spawn(Duration::ZERO, Duration::from_millis(1), move || {
                count.fetch_add(1, Ordering::SeqCst);
                sent.send(()).unwrap();
            })
            .unwrap();
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        monitor.shutdown().unwrap();
        let stopped = samples.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(10));
        assert_eq!(samples.load(Ordering::SeqCst), stopped);
    }
    #[test]
    fn in_flight_sample_finishes_before_shutdown_returns() {
        let (sent, ready) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let done = Arc::new(AtomicUsize::new(0));
        let seen = done.clone();
        let mut monitor =
            OwnedMonitor::spawn(Duration::ZERO, Duration::from_millis(1), move || {
                sent.send(()).unwrap();
                gate.recv().unwrap();
                done.store(1, Ordering::SeqCst);
            })
            .unwrap();
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            release.send(()).unwrap();
        });
        monitor.shutdown().unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        releaser.join().unwrap();
    }
    #[test]
    fn panic_payload_is_returned_and_dropped_on_owner() {
        use std::{any::Any, sync::Mutex};
        struct PanicData {
            owner: thread::ThreadId,
            drops: Arc<Mutex<Vec<thread::ThreadId>>>,
        }
        impl Drop for PanicData {
            fn drop(&mut self) {
                assert_eq!(thread::current().id(), self.owner);
                self.drops.lock().unwrap().push(thread::current().id());
            }
        }
        let drops = Arc::new(Mutex::new(Vec::new()));
        let owner = thread::current().id();
        let worker_drops = drops.clone();
        let (sent, ready) = mpsc::channel();
        let mut monitor =
            OwnedMonitor::spawn(Duration::ZERO, Duration::from_millis(1), move || {
                sent.send(()).unwrap();
                std::panic::panic_any(PanicData {
                    owner,
                    drops: worker_drops.clone(),
                });
            })
            .unwrap();
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        let panic: Box<dyn Any + Send> = monitor.shutdown().unwrap_err();
        assert!(drops.lock().unwrap().is_empty());
        drop(panic);
        assert_eq!(drops.lock().unwrap().len(), 1);
        monitor.shutdown().unwrap();
    }
}
