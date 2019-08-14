//! # Futures Executor
//!
//! A [futures::WaitableManager] is a future executor, which is more or less a
//! userspace scheduler, taking a list of [futures::Task] and running them to
//! completion. Occasionally, a [futures::Task] will need to wait on a resource,
//! usually backed by a [types::Handle]. When this happens, it can use its
//! [core::task::Waker] to tell the executor to wake it up once a specified
//! handled is notified.
//!
//! A [futures::WorkQueue] is a handle to the [futures::WaitableManager]. Work
//! is submitted to the [futures::WaitableManager] by pushing
//! [futures::WorkItem]s on the [futures::WorkQueue].
//!
//! The implementation is very liberally taken from the blog post [Building an
//! Embedded Futures Executor]
//! and adapted to work with the current Futures API and to work with our
//! Operating System.
//!
//! [Building an Embedded Futures Executor]: https://web.archive.org/web/20190812080700/https://josh.robsonchase.com/embedded-executor/

use core::task::{Context, Waker, Poll};
use core::cell::Cell;
use core::future::Future;
use core::pin::Pin;
use alloc::sync::Arc;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use futures::task::ArcWake;
use futures::future::{FutureObj, LocalFutureObj};
use spin::Mutex;

use crate::types::HandleRef;
use crate::syscalls;

/// A Task represents a future spawned on the [WaitableManager].
#[derive(Debug)]
struct Task<'a> {
    /// The future backing this task. When the task is woken up, this future
    /// gets polled.
    future: LocalFutureObj<'a, ()>,
    /// The waker used to wake this task up from sleep, rescheduling it to be polled.
    // Invariant: waker should always be Some after the task has been spawned.
    waker: Option<Waker>,
    /// List of handles that this task is currently waiting on.
    waiting_on: Vec<u32>,
}

/// Task currently executing on this thread, or None.
#[thread_local]
static CURRENT_TASK: Cell<Option<generational_arena::Index>> = Cell::new(None);

/// A WorkQueue represents a handle to a [WaitableManager] on which you can spawn
/// new Futures with [WorkQueue::spawn()] or put the current future to sleep until
/// a handle is signaled through [WorkQueue::wait_for()].
///
/// This handle may be cloned - it will still point to the same [WaitableManager].
/// It may be shared with other threads, sent to other event loops, etc... in order
/// to implement message passing.
///
/// Internally, a WorkQueue is an (Arc'd) deque of [WorkItem], which the event loop
/// will pop from in order to drive the scheduler.
#[derive(Debug, Clone, Default)]
pub struct WorkQueue<'a>(Arc<Mutex<VecDeque<WorkItem<'a>>>>);

/// A WorkItem is an element of work that will be executed by a
/// [WaitableManager]'s run function. By pushing a new WorkItem on a
/// [WorkQueue], the user can drive the event loop.
///
/// The user can only access two of the three possible WorkItems: Spawn and
/// WaitHandle. Poll is used internally when an awaited handle is signaled.
#[derive(Debug)]
enum WorkItem<'a> {
    /// Causes the [Task] specified by the index to be woken up and polled.
    Poll(generational_arena::Index),
    /// Creates a new [Task] backed by the given future on the event loop.
    Spawn(FutureObj<'a, ()>),
    /// Registers the [Task] backed by the given [Waker] to be woken up when the
    /// passed handle is signaled - which is detected by adding it to the list
    /// of handles the event loop calls [syscalls::wait_synchronization()] on
    /// when no other task needs to run.
    WaitHandle(HandleRef<'static>, Waker, generational_arena::Index),
}

impl<'a> WorkQueue<'a> {
    /// Registers the task represented by the given [Context] to be polled when
    /// the given handle is signaled.
    // TODO: How to know which handle was signaled ?_?.
    pub(crate) fn wait_for(&self, handle: HandleRef<'_>, ctx: &mut Context) {
        let id = CURRENT_TASK.get();

        if let Some(id) = id {
            self.0.lock().push_back(WorkItem::WaitHandle(handle.staticify(), ctx.waker().clone(), id))
        } else {
            panic!("Tried to use wait_async outside of a spawned future.
            Please only use wait_async from futures spawned on a WaitableManager.");
        }
    }

    /// Spawn a top-level future on the event loop. The future will be polled once
    /// on spawn. Once the future is spawned, it will be owned by the [WaitableManager].
    pub fn spawn(&self, future: FutureObj<'a, ()>) {
        self.0.lock().push_back(WorkItem::Spawn(future));
    }
}

/// A waker backed by a WorkQueue and an index in the [WaitableManager]'s registry of
/// tasks. Waking up will add a Poll work item on the [WorkQueue], causing the
/// WaitableManager to poll the item selected by the index.
#[derive(Debug, Clone)]
pub struct QueueWaker<'a> {
    /// The WorkQueue this waker operates on.
    queue: WorkQueue<'a>,
    /// An index to the future to poll on wake in the [WaitableManager::registry].
    id: generational_arena::Index,
}

impl<'a> ArcWake for QueueWaker<'a> {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.queue.0.lock().push_back(WorkItem::Poll(arc_self.id))
    }
}

/// The event loop manager. Waits on the waitable objects added to it.
#[derive(Debug)]
pub struct WaitableManager<'a> {
    /// Queue of things to do in the next "tick" of the event loop.
    work_queue: WorkQueue<'a>,
    /// List of futures that are currently running on this executor.
    registry: generational_arena::Arena<Task<'a>>
}

impl<'a> Default for WaitableManager<'a> {
    fn default() -> Self {
        WaitableManager::new()
    }
}

impl<'a> WaitableManager<'a> {
    /// Creates an empty event loop.
    pub fn new() -> WaitableManager<'a> {
        WaitableManager {
            work_queue: WorkQueue(Arc::new(Mutex::new(VecDeque::new()))),
            registry: generational_arena::Arena::new(),
        }
    }

    /// Returns a handle to the underlying WorkQueue backing this
    /// event loop. Can (and probably should be) passed to futures spawned on
    /// the event loop so they can wait on handles and spawn new futures
    /// themselves.
    pub fn work_queue(&self) -> WorkQueue<'a> {
        self.work_queue.clone()
    }

    /// Runs the event loop, popping items from the underlying [WorkQueue] and
    /// executing them. When there isn't any more work to do, we call
    /// [syscalls::wait_synchronization()] on all the handles that were
    /// registered through [WorkQueue#WaitHandle]. All the tasks that were
    /// waiting on the handle that got woken up will be polled again, resuming
    /// the event loop.
    ///
    /// Returns when all the futures spawned on the loop have returned a value.
    pub fn run(&mut self) {
        let mut waitables = Vec::new();
        let mut handle_to_waker: Vec<Vec<(generational_arena::Index, Waker)>> = Vec::new();
        loop {
            loop {
                let item = self.work_queue.0.lock().pop_front();
                let item = if let Some(item) = item { item } else { break };
                match item {
                    WorkItem::Poll(id) => {
                        if let Some(Task { future, waker, .. }) = self.registry.get_mut(id) {
                            let future = Pin::new(future);

                            let waker = waker
                                .as_ref()
                                .expect("waker not set, task spawned incorrectly");

                            CURRENT_TASK.set(Some(id));

                            if let Poll::Ready(_) = future.poll(&mut Context::from_waker(waker)) {
                                // Ideally, this would not be necessary because
                                // dropping a future should cause the futures
                                // associated with those to get cancelled.
                                let task = self.registry.remove(id).unwrap();
                                for hnd in task.waiting_on {
                                    if let Some(idx) = waitables.iter().position(|v: &HandleRef| v.inner.get() == hnd) {
                                        handle_to_waker[idx].retain(|(idx, _)| *idx != id);
                                        if handle_to_waker.is_empty() {
                                            waitables.remove(idx);
                                        }
                                    } else {
                                        // Shrug again
                                    }
                                }
                            }

                            CURRENT_TASK.set(None);
                        }
                    },
                    WorkItem::Spawn(future) => {
                        let id = self.registry.insert(Task {
                            future: future.into(),
                            waker: None,
                            waiting_on: Vec::new()
                        });
                        self.registry.get_mut(id).unwrap().waker = Some(Arc::new(QueueWaker {
                            queue: self.work_queue.clone(),
                            id,
                        }).into_waker());
                        self.work_queue.0.lock().push_back(WorkItem::Poll(id));
                    },
                    WorkItem::WaitHandle(hnd, waker, id) => {
                        if let Some(task) = self.registry.get_mut(id) {
                            if let Some(idx) = waitables.iter().position(|v| *v == hnd) {
                                handle_to_waker[idx].push((id, waker));
                            } else {
                                waitables.push(hnd);
                                handle_to_waker.push(vec![(id, waker)]);
                            }
                            task.waiting_on.push(hnd.inner.get());
                        } else {
                            // ¯\_(ツ)_/¯
                        }
                    }
                }
            }

            if self.registry.is_empty() {
                break;
            }

            assert!(!waitables.is_empty(), "WaitableManager entered invalid state: No waitables to wait on.");
            debug!("Calling WaitSynchronization with {:?}", waitables);
            let idx = syscalls::wait_synchronization(&*waitables, None).unwrap();
            debug!("Handle idx {} got signaled", idx);
            for (_, item) in handle_to_waker.remove(idx) {
                item.wake()
            }

            waitables.remove(idx);
        }
    }
}