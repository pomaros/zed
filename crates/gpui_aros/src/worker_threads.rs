// SPDX-License-Identifier: MIT
// Copyright (c) 2026 John Knipper

//! Join workers on one owned auxiliary while the owner services foreground.
//! Work-body completion is deliberately not used as evidence of TLS teardown.
use std::any::Any;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle, ThreadId};

type ThreadResult = Result<(), Box<dyn Any + Send + 'static>>;
struct Outcome {
    published: bool,
    all_joined: bool,
    results: Vec<ThreadResult>,
    helper_panic: Option<Box<dyn Any + Send + 'static>>,
}
struct Transfer {
    workers: Mutex<Vec<JoinHandle<()>>>,
    outcome: Mutex<Outcome>,
}
struct Joining {
    transfer: Arc<Transfer>,
    helper: Option<JoinHandle<()>>,
}
struct State {
    closed: bool,
    workers: Vec<JoinHandle<()>>,
    joining: Option<Joining>,
}
pub(super) struct WorkerThreads {
    owner: ThreadId,
    state: Mutex<State>,
    /// Native signal-only notifier. It must not run application callbacks,
    /// install application TLS or panic. It remains owned here while the
    /// auxiliary holds a clone, so its captured values are not dropped there.
    wake_owner: Arc<dyn Fn() + Send + Sync>,
}

impl WorkerThreads {
    pub(super) fn new(wake_owner: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            owner: thread::current().id(),
            state: Mutex::new(State {
                closed: false,
                workers: Vec::new(),
                joining: None,
            }),
            wake_owner: Arc::new(wake_owner),
        }
    }
    pub(super) fn spawn(
        &self,
        name: String,
        work: impl FnOnce() + Send + 'static,
    ) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "dispatcher admission closed",
            ));
        }
        let handle = thread::Builder::new().name(name).spawn(work)?;
        state.workers.push(handle);
        Ok(())
    }
    pub(super) fn is_owner(&self) -> bool {
        thread::current().id() == self.owner
    }
    pub(super) fn is_empty(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.workers.is_empty() && state.joining.is_none()
    }
    pub(super) fn close(&self) -> io::Result<()> {
        self.close_with(|work| {
            thread::Builder::new()
                .name("gpui-aros-joiner".into())
                .spawn(work)
        })
    }
    fn close_with(
        &self,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<JoinHandle<()>>,
    ) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !self.is_owner() && (!state.workers.is_empty() || state.joining.is_some()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "dispatcher shutdown requires owner",
            ));
        }
        state.closed = true;
        if state.joining.is_none() && !state.workers.is_empty() {
            let count = state.workers.len();
            let workers = std::mem::take(&mut state.workers);
            state.joining = Some(Joining {
                transfer: Arc::new(Transfer {
                    workers: Mutex::new(workers),
                    outcome: Mutex::new(Outcome {
                        published: false,
                        all_joined: false,
                        results: Vec::with_capacity(count),
                        helper_panic: None,
                    }),
                }),
                helper: None,
            });
        }
        if let Some(joining) = state.joining.as_mut() {
            if joining.helper.is_none() {
                let transfer = joining.transfer.clone();
                let wake_owner = self.wake_owner.clone();
                // The transfer remains owned in State even if spawn fails and
                // drops this closure. No captured JoinHandle can be detached.
                joining.helper = Some(spawn(Box::new(move || {
                    let joined = catch_unwind(AssertUnwindSafe(|| {
                        loop {
                            let worker = transfer
                                .workers
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .pop();
                            let Some(worker) = worker else {
                                break;
                            };
                            // A worker panic is data. Retain its payload for the
                            // owner, never run its arbitrary Drop on this helper.
                            let result = worker.join();
                            transfer
                                .outcome
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .results
                                .push(result);
                        }
                    }));
                    {
                        let mut outcome = transfer
                            .outcome
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        match joined {
                            Ok(()) => outcome.all_joined = true,
                            Err(panic) => outcome.helper_panic = Some(panic),
                        }
                        outcome.published = true;
                    }
                    // No application code or application-owned destructor runs
                    // here or during this helper's TLS teardown. All worker TLS
                    // destructors have completed before all_joined is published.
                    wake_owner();
                }))?);
            }
        }
        Ok(())
    }
    /// (unfinished join protocol, joined worker panics). The owner only joins
    /// the signal-only helper after it has joined all application workers.
    /// It must keep pumping foreground while unfinished is nonzero.
    pub(super) fn join_completed(&self) -> io::Result<(usize, usize)> {
        let joining = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.workers.is_empty() && state.joining.is_none() {
                return Ok((0, 0));
            }
            if !self.is_owner() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "dispatcher joins require owner",
                ));
            }
            let Some(joining) = state.joining.as_ref() else {
                return Ok((1, 0));
            };
            if joining.helper.is_none() {
                return Err(io::Error::other("dispatcher joiner has not started"));
            }
            if !joining
                .transfer
                .outcome
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .published
            {
                return Ok((1, 0));
            }
            state.joining.take().expect("observed join transfer")
        };
        let Joining { transfer, helper } = joining;
        // Only std join/publication/native signal operations ran on this
        // auxiliary. It has no user TLS or callback dependency on this owner.
        let helper_result = helper.expect("registered joiner").join();
        let mut outcome = transfer
            .outcome
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if helper_result.is_err() || !outcome.all_joined || outcome.helper_panic.is_some() {
            // Preserve remaining handles rather than detach them on failure.
            // Normal shutdown cannot return success from this invariant error.
            drop(outcome);
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .joining = Some(Joining {
                transfer,
                helper: None,
            });
            return Err(io::Error::other(
                "dispatcher joiner failed before complete teardown",
            ));
        }
        let panics = outcome
            .results
            .iter()
            .filter(|result| result.is_err())
            .count();
        let results = std::mem::take(&mut outcome.results);
        drop(outcome);
        // Payload destruction occurs on the owner, after the helper is joined.
        drop(results);
        Ok((0, panics))
    }
}
impl Drop for WorkerThreads {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|error| error.into_inner());
        if !state.workers.is_empty() || state.joining.is_some() {
            eprintln!("gpui_aros: dispatcher dropped before workers and joiner joined");
            std::process::abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;
    fn finish(workers: &WorkerThreads, wakes: &mpsc::Receiver<()>) -> usize {
        wakes.recv_timeout(Duration::from_secs(1)).unwrap();
        let (remaining, panics) = workers.join_completed().unwrap();
        assert_eq!(remaining, 0);
        assert!(workers.is_empty());
        panics
    }
    #[test]
    fn owner_services_foreground_before_join_and_refuses_late_spawn() {
        let (wake, wakes) = mpsc::channel();
        let workers = WorkerThreads::new(move || wake.send(()).unwrap());
        let (request, requests) = mpsc::channel();
        let (reply, replies) = mpsc::channel();
        workers
            .spawn("worker".into(), move || {
                request.send(()).unwrap();
                replies.recv().unwrap();
            })
            .unwrap();
        requests.recv_timeout(Duration::from_secs(1)).unwrap();
        workers.close().unwrap();
        assert_eq!(workers.join_completed().unwrap(), (1, 0));
        assert_eq!(
            workers.spawn("late".into(), || {}).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        reply.send(()).unwrap();
        assert_eq!(finish(&workers, &wakes), 0);
        assert_eq!(workers.join_completed().unwrap(), (0, 0));
    }
    #[test]
    fn self_join_refused_without_losing_handle() {
        let (wake, wakes) = mpsc::channel();
        let workers = Arc::new(WorkerThreads::new(move || wake.send(()).unwrap()));
        let other = workers.clone();
        workers
            .spawn("self-check".into(), move || {
                assert_eq!(
                    other.join_completed().unwrap_err().kind(),
                    io::ErrorKind::PermissionDenied
                );
            })
            .unwrap();
        workers.close().unwrap();
        assert_eq!(finish(&workers, &wakes), 0);
    }
    #[test]
    fn panicking_work_still_notifies_and_is_joined() {
        let (wake, wakes) = mpsc::channel();
        let workers = WorkerThreads::new(move || wake.send(()).unwrap());
        workers.spawn("panic".into(), || panic!("fixture")).unwrap();
        workers.close().unwrap();
        assert_eq!(finish(&workers, &wakes), 1);
    }
    #[test]
    fn failed_helper_spawn_keeps_all_handles_for_retry() {
        let (wake, wakes) = mpsc::channel();
        let workers = WorkerThreads::new(move || wake.send(()).unwrap());
        workers.spawn("worker".into(), || {}).unwrap();
        assert!(
            workers
                .close_with(|_| Err(io::Error::other("spawn fixture")))
                .is_err()
        );
        assert!(!workers.is_empty());
        assert_eq!(
            workers
                .state
                .lock()
                .unwrap()
                .joining
                .as_ref()
                .unwrap()
                .transfer
                .workers
                .lock()
                .unwrap()
                .len(),
            1
        );
        workers.close().unwrap();
        assert_eq!(finish(&workers, &wakes), 0);
    }
    #[test]
    fn panic_payload_drops_on_owner_and_can_reenter_closed_registry() {
        struct Payload {
            workers: Arc<WorkerThreads>,
            dropped: mpsc::Sender<ThreadId>,
        }
        impl Drop for Payload {
            fn drop(&mut self) {
                assert_eq!(
                    self.workers
                        .spawn("late-payload".into(), || {})
                        .unwrap_err()
                        .kind(),
                    io::ErrorKind::BrokenPipe
                );
                self.dropped.send(thread::current().id()).unwrap();
            }
        }
        let (wake, wakes) = mpsc::channel();
        let workers = Arc::new(WorkerThreads::new(move || wake.send(()).unwrap()));
        let (dropped, notification) = mpsc::channel();
        let payload = Payload {
            workers: workers.clone(),
            dropped,
        };
        workers
            .spawn("panic-payload".into(), move || {
                std::panic::panic_any(payload)
            })
            .unwrap();
        workers.close().unwrap();
        assert_eq!(finish(&workers, &wakes), 1);
        assert_eq!(
            notification.recv_timeout(Duration::from_secs(1)).unwrap(),
            thread::current().id()
        );
    }
}
