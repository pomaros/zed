//! Task dispatcher for the AROS backend.
//!
//! std threads are proven on hosted AROS, so this is a plain std-threaded
//! dispatcher: a background worker pool draining a priority queue, a main-thread
//! FIFO drained by the platform run loop, and a single long-lived timer thread
//! servicing all `dispatch_after` deadlines from a min-heap.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, VecDeque};
use std::ffi::c_int;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::thread::{self, ThreadId};

#[path = "worker_threads.rs"]
mod worker_threads;
use std::time::{Duration, Instant};
use worker_threads::WorkerThreads;

use gpui::{PlatformDispatcher, Priority, RunnableVariant};
use parking_lot::Mutex;
// std's Mutex/Condvar (NOT parking_lot's) for anything that BLOCKS: on this
// target parking_lot_core falls back to its generic ThreadParker, which
// busy-spins instead of parking -- fatal on a single-CPU cooperative guest
// (measured: 99% CPU at idle). std's condvar routes through aros_cond_wait ->
// pthread_cond_wait -> a real exec Wait().
use std::sync::{Condvar as StdCondvar, Mutex as StdMutex};

use crate::glue;

/// A blocking MPMC queue.
///
/// Deliberately NOT crossbeam_channel: crossbeam is lock-free and spins
/// (`Backoff::snooze()` -> `yield_now()`) before parking, which is designed for
/// real multi-core parallelism. Hosted AROS multiplexes EVERY task onto one
/// host CPU with cooperative, signal-based switching -- so a spinning receiver
/// cannot make progress, it can only burn the CPU that the task it is waiting
/// for needs to run. Task dumps during a freeze caught exactly that: a worker
/// pinned at 100% in `crossbeam_channel::flavors::list::Channel::recv` while
/// the UI sat in READY. A mutex+condvar blocks immediately and hands the CPU
/// back.
enum QueueWait<T> {
    Item(T),
    TimedOut,
    Closed,
}

// Stop admission without canceling async-task runnables while foreground
// consumers are still being polled. Dropping a Runnable wakes its Task awaiter
// with cancellation; the infallible Task future then panics when polled.
enum QueueState<T> {
    Open(VecDeque<T>),
    Stopped(VecDeque<T>),
    Released,
}

struct BlockingQueue<T> {
    items: StdMutex<QueueState<T>>,
    ready: StdCondvar,
}

impl<T> BlockingQueue<T> {
    fn new() -> Self {
        Self {
            items: StdMutex::new(QueueState::Open(VecDeque::new())),
            ready: StdCondvar::new(),
        }
    }

    /// Returns false if the queue is closed.
    fn push(&self, item: T) -> bool {
        let mut guard = self.items.lock().unwrap_or_else(|e| e.into_inner());
        match &mut *guard {
            QueueState::Open(q) => {
                q.push_back(item);
                drop(guard);
                self.ready.notify_one();
                true
            }
            QueueState::Stopped(retired) => {
                retired.push_back(item);
                false
            }
            QueueState::Released => {
                // Destruction may reenter dispatch. Never drop under lock.
                drop(guard);
                drop(item);
                false
            }
        }
    }

    /// Block until an item arrives; None once admission is stopped.
    fn pop(&self) -> Option<T> {
        let mut guard = self.items.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match &mut *guard {
                QueueState::Open(q) => match q.pop_front() {
                    Some(item) => return Some(item),
                    None => {
                        guard = self.ready.wait(guard).unwrap_or_else(|e| e.into_inner());
                    }
                },
                QueueState::Stopped(_) | QueueState::Released => return None,
            }
        }
    }

    /// Closed and timeout are distinct: a pending timer heap must never
    /// turn a closed queue into a spin until its far-future deadline.
    fn pop_timeout(&self, timeout: Duration) -> QueueWait<T> {
        let mut guard = self.items.lock().unwrap_or_else(|error| error.into_inner());
        let QueueState::Open(items) = &mut *guard else {
            return QueueWait::Closed;
        };
        if let Some(item) = items.pop_front() {
            return QueueWait::Item(item);
        }
        guard = self
            .ready
            .wait_timeout(guard, timeout)
            .unwrap_or_else(|error| error.into_inner())
            .0;
        match &mut *guard {
            QueueState::Stopped(_) | QueueState::Released => QueueWait::Closed,
            QueueState::Open(items) => match items.pop_front() {
                Some(item) => QueueWait::Item(item),
                None => QueueWait::TimedOut,
            },
        }
    }

    fn close(&self) {
        let mut guard = self.items.lock().unwrap_or_else(|error| error.into_inner());
        if let QueueState::Open(_) = &*guard {
            let QueueState::Open(items) = std::mem::replace(&mut *guard, QueueState::Released)
            else {
                unreachable!();
            };
            *guard = QueueState::Stopped(items);
        }
        drop(guard);
        self.ready.notify_all();
    }

    /// Only after the foreground pump is closed and all producers joined.
    fn release(&self) {
        let retired = std::mem::replace(
            &mut *self.items.lock().unwrap_or_else(|error| error.into_inner()),
            QueueState::Released,
        );
        drop(retired);
    }

    fn is_closed(&self) -> bool {
        !matches!(
            &*self.items.lock().unwrap_or_else(|error| error.into_inner()),
            QueueState::Open(_)
        )
    }
}

const MIN_THREADS: usize = 2;

/// Exec priority for background workers and the timer thread: SAME as the UI
/// task (0), relying on exec's round-robin time-slicing between equal-priority
/// READY tasks for fairness.
///
/// History: this was -1 ("UI always preempts background work") while hosted
/// darwin's preemption was broken — back then a CPU-bound equal-pri worker
/// starved the UI outright because nothing ever forced a task switch. With
/// tick forwarding fixed (aros-upstream 8f1acacc) the demotion inverted the
/// bug: exec's strict-priority scheduler gives pri -1 ZERO cpu while a pri-0
/// task stays busy, and a continuously-animating UI (any indeterminate
/// progress spinner repaints every frame) is exactly that — workers starved
/// forever, folder-size results never applied, navigation enumeration never
/// ran, tasks never finished, so the spinner never stopped: a livelock
/// (observed on-device 2026-07-21: UI RUN at pri 0 in render, all workers
/// READY at -1, one mid-send on the folder-sizes channel).
const WORKER_PRI: c_int = 0;

/// One pending `dispatch_after` runnable, ordered by deadline for the timer
/// thread's min-heap. `seq` breaks ties (and keeps the ordering total without
/// having to order `RunnableVariant`); `BinaryHeap` is a max-heap, so both
/// comparisons are reversed to pop the *earliest* deadline first.
struct TimerEntry {
    deadline: Instant,
    seq: u64,
    runnable: RunnableVariant,
}

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.seq == other.seq
    }
}
impl Eq for TimerEntry {}
impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.deadline
            .cmp(&other.deadline)
            .then(self.seq.cmp(&other.seq))
            .reverse()
    }
}

pub(crate) struct ArosDispatcher {
    main_thread_id: ThreadId,
    main_queue: Arc<Mutex<VecDeque<RunnableVariant>>>,
    background_queue: Arc<BlockingQueue<RunnableVariant>>,
    timer_queue: Arc<BlockingQueue<(Instant, RunnableVariant)>>,
    workers: WorkerThreads,
    foreground_closed: AtomicBool,
}

impl Drop for ArosDispatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl ArosDispatcher {
    /// Must be constructed on the main thread (records its `ThreadId`).
    pub(crate) fn new() -> Self {
        // gpui's PriorityQueue re-export is not available for this target, so a
        // plain blocking MPMC queue backs the worker pool. Priority is not
        // honored; the run loop's frame cadence keeps latency bounded
        // (correctness first).
        let background_queue: Arc<BlockingQueue<RunnableVariant>> = Arc::new(BlockingQueue::new());
        // Every AROS task -- including these workers -- is multiplexed onto a
        // SINGLE host thread by the hosted kernel, so `available_parallelism()`
        // (the host's core count) buys exactly
        // zero parallelism here. It only multiplies context switches (each one
        // a raise()/sigsuspend() signal round-trip) and contention on the
        // shared, not-always-thread-safe C runtime (posixc's fd table, the
        // timer device). Cap it: two workers keep I/O overlapping with the UI
        // without spawning a herd that just fights over one CPU.
        let thread_count = if cfg!(target_os = "aros") {
            MIN_THREADS
        } else {
            thread::available_parallelism().map_or(MIN_THREADS, |n| n.get().max(MIN_THREADS))
        };

        let workers = WorkerThreads::new(|| {
            // Completion wakes the owner even if no foreground work remains.
            unsafe { glue::gpa_wake_main() };
        });
        for i in 0..thread_count {
            let queue = background_queue.clone();
            workers
                .spawn(format!("gpui-aros-worker-{i}"), move || {
                    // Same priority as the UI task (see WORKER_PRI doc):
                    // exec round-robins equal-pri tasks per quantum now
                    // that preemption ticks actually arrive.
                    // SAFETY: FFI; acts on the calling task only.
                    unsafe { glue::gpa_lower_task_pri(WORKER_PRI) };
                    while let Some(runnable) = queue.pop() {
                        runnable.run();
                    }
                })
                .expect("failed to spawn gpui_aros background worker")
        }

        let main_queue = Arc::new(Mutex::new(VecDeque::new()));

        // One long-lived timer thread services every `dispatch_after` from a
        // deadline-ordered heap, instead of spawning a fresh sleeper task per
        // call — gpui fires timers constantly (animation, debounce, `Timer`),
        // and on hosted AROS each thread is an exec Task from a finite pool, so
        // thread-per-timer churned tasks and risked a spawn abort on the hot
        // path (fatal in AROS's single address space).
        let timer_queue: Arc<BlockingQueue<(Instant, RunnableVariant)>> =
            Arc::new(BlockingQueue::new());
        {
            let main_queue = main_queue.clone();
            let timer_queue = timer_queue.clone();
            workers
                .spawn("gpui-aros-timer".to_owned(), move || {
                    // Same rationale as the workers (see WORKER_PRI doc).
                    // SAFETY: FFI; acts on the calling task only.
                    unsafe { glue::gpa_lower_task_pri(WORKER_PRI) };
                    timer_loop(timer_queue, main_queue)
                })
                .expect("failed to spawn gpui_aros timer thread")
        };

        Self {
            main_thread_id: thread::current().id(),
            main_queue,
            background_queue,
            timer_queue,
            workers,
            foreground_closed: AtomicBool::new(false),
        }
    }

    /// Called after quit observers, while foreground callbacks can still
    /// run on the owner. Never wait for a live worker that needs that owner.
    pub(crate) fn shutdown(&self) {
        if !self.workers.is_owner() && !self.workers.is_empty() {
            log::error!("gpui_aros: active dispatcher shutdown outside owner thread");
            std::process::abort();
        }
        self.workers.close().unwrap_or_else(|error| {
            log::error!("gpui_aros: cannot start owned shutdown joiner: {error}");
            std::process::abort();
        });
        self.background_queue.close();
        self.timer_queue.close();
        loop {
            while let Some(runnable) = { self.main_queue.lock().pop_front() } {
                runnable.run();
            }
            let (remaining, panics) = self.workers.join_completed().unwrap_or_else(|error| {
                log::error!("gpui_aros: unsafe dispatcher shutdown: {error}");
                std::process::abort();
            });
            if panics != 0 {
                log::error!("gpui_aros: {panics} dispatcher workers panicked");
            }
            if remaining == 0 {
                break;
            }
            // Existing native wake signal, no polling/Delay/timer deadline.
            // The owned joiner or a foreground dispatch wakes us.
            if unsafe { glue::gpa_wait_dispatcher_shutdown() } != 0 {
                log::error!("gpui_aros: shutdown has no valid owner wake signal");
                std::process::abort();
            }
        }
        let discarded = {
            let mut queue = self.main_queue.lock();
            self.foreground_closed.store(true, AtomicOrdering::Release);
            std::mem::take(&mut *queue)
        };
        drop(discarded);
        // No foreground awaiter can run after this point. Cancellation wakes
        // now discard its continuation instead of polling a canceled Task.
        self.background_queue.release();
        self.timer_queue.release();
    }

    /// Shared handle to the main-thread queue, drained by the run loop.
    pub(crate) fn main_queue(&self) -> Arc<Mutex<VecDeque<RunnableVariant>>> {
        self.main_queue.clone()
    }
}

impl PlatformDispatcher for ArosDispatcher {
    fn is_main_thread(&self) -> bool {
        thread::current().id() == self.main_thread_id
    }

    fn dispatch(&self, runnable: RunnableVariant, _priority: Priority) {
        if !self.background_queue.push(runnable) {
            log::error!("gpui_aros: background dispatch failed (workers gone)");
        }
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, _priority: Priority) {
        let mut queue = self.main_queue.lock();
        if self.foreground_closed.load(AtomicOrdering::Acquire) {
            drop(queue);
            log::error!("gpui_aros: foreground dispatch after shutdown");
            return;
        }
        queue.push_back(runnable);
        drop(queue);
        // Nudge the run loop out of its park so the work starts within the
        // poll granularity (~2 ms) instead of the frame budget. Safe from
        // any thread (hosted-AROS threads are exec tasks); no-op pre-init.
        unsafe { glue::gpa_wake_main() };
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        let deadline = Instant::now() + duration;
        // Hand the deadline to the shared timer thread. Failure only happens if
        // that thread is gone (shutdown) — drop the runnable rather than panic,
        // matching `dispatch`.
        if !self.timer_queue.push((deadline, runnable)) {
            log::error!("gpui_aros: timer dispatch failed (timer thread gone)");
        }
    }

    fn spawn_realtime(&self, f: Box<dyn FnOnce() + Send>) {
        if let Err(error) = self.workers.spawn("gpui-aros-realtime".into(), f) {
            log::error!("gpui_aros: realtime dispatch rejected: {error}");
        }
    }
}

/// The single timer thread: keep a min-heap of pending deadlines, park until
/// the nearest one (or until a new deadline arrives), then push every matured
/// runnable onto the main queue and wake the run loop. Exits when the sender is
/// dropped (dispatcher teardown).
fn timer_loop(
    queue: Arc<BlockingQueue<(Instant, RunnableVariant)>>,
    main_queue: Arc<Mutex<VecDeque<RunnableVariant>>>,
) {
    let mut heap: BinaryHeap<TimerEntry> = BinaryHeap::new();
    let mut seq: u64 = 0;
    loop {
        if queue.is_closed() {
            break;
        }
        // Fire everything already due.
        let now = Instant::now();
        let mut woke = false;
        while heap.peek().is_some_and(|e| e.deadline <= now) {
            let entry = heap.pop().expect("peeked");
            main_queue.lock().push_back(entry.runnable);
            woke = true;
        }
        if woke {
            // SAFETY: see dispatch_on_main_thread; safe from any thread.
            unsafe { glue::gpa_wake_main() };
        }

        // Park until the next deadline, or indefinitely if none pending.
        // A zero/near-zero timeout would busy-loop, so only wait when the
        // deadline is still ahead; otherwise loop straight back and fire it.
        let received = match heap.peek() {
            Some(next) => {
                let timeout = next.deadline.saturating_duration_since(Instant::now());
                if timeout.is_zero() {
                    continue;
                }
                match queue.pop_timeout(timeout) {
                    QueueWait::Item(item) => Some(item),
                    QueueWait::TimedOut => continue,
                    QueueWait::Closed => break,
                }
            }
            // Nothing pending: block. `None` means the queue closed (shutdown).
            None => match queue.pop() {
                Some(item) => Some(item),
                None => break,
            },
        };
        if let Some((deadline, runnable)) = received {
            heap.push(TimerEntry {
                deadline,
                seq,
                runnable,
            });
            seq = seq.wrapping_add(1);
        }
    }
    // The timer thread must not cancel Tasks still awaited by foreground
    // work. Retain timers without firing them early; the owner releases them
    // after joining us and closing the foreground pump.
    for entry in heap {
        queue.push((entry.deadline, entry.runnable));
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn closed_timer_queue_does_not_wait_for_far_future_deadline() {
        let queue = Arc::new(BlockingQueue::<u8>::new());
        let other = queue.clone();
        let (started, ready) = mpsc::channel();
        let (finished, completion) = mpsc::channel();
        let worker = thread::spawn(move || {
            started.send(()).unwrap();
            assert!(matches!(
                other.pop_timeout(Duration::from_secs(3600)),
                QueueWait::Closed
            ));
            finished.send(()).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        queue.close();
        completion.recv_timeout(Duration::from_secs(1)).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn releasing_retired_values_can_reenter_without_queue_lock() {
        struct Reenter(Arc<BlockingQueue<Reenter>>);
        impl Drop for Reenter {
            fn drop(&mut self) {
                assert!(self.0.is_closed());
            }
        }
        let queue = Arc::new(BlockingQueue::new());
        assert!(queue.push(Reenter(queue.clone())));
        queue.close();
        assert!(queue.is_closed());
        queue.release();
    }

    #[test]
    fn stopped_queue_retains_backlog_and_rejections_until_release() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Count(Arc<AtomicUsize>);
        impl Drop for Count {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let queue = BlockingQueue::new();
        assert!(queue.push(Count(drops.clone())));
        queue.close();
        assert!(!queue.push(Count(drops.clone())));
        assert!(queue.pop().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        queue.close(); // stopping twice does not cancel retained work
        queue.release();
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        assert!(!queue.push(Count(drops.clone())));
        assert_eq!(drops.load(Ordering::SeqCst), 3);
        queue.release(); // release is also idempotent
    }
}
