//! Coroutines the Lua owner thread drives between its jobs.
//!
//! A job that would hold the owner thread for long, a provider stream
//! or a tool call, runs its Lua function as a coroutine instead: [`drive`]
//! starts it and [`spawn`] hands the resulting future to the owner
//! loop (see [`crate::host`]), which polls it whenever it is woken and
//! runs other jobs in between. Host functions called inside such a
//! coroutine ([`is_driven`]) move their blocking work to a worker
//! thread ([`off_thread`]) and await it, so the coroutine yields
//! instead of holding the thread. Called anywhere else they block as
//! they always did, which keeps commands and hooks unchanged.

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::future::{Future, poll_fn};
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};
use std::thread;
use std::time::Duration;

use kage_core::sync::lock;
use mlua::{FromLuaMulti, Function, IntoLuaMulti, Lua};

use crate::watchdog;

/// A future the owner loop polls until it completes.
pub(crate) type Task = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Tasks waiting for the owner loop to adopt them, and the coroutines
/// being driven, by their thread pointer.
#[derive(Default)]
struct Registry {
    spawned: RefCell<Vec<Task>>,
    driven: RefCell<HashSet<usize>>,
}

/// Prepare `lua` to drive coroutines.
pub(crate) fn install(lua: &Lua) {
    lua.set_app_data(Registry::default());
}

/// Hand `task` to the owner loop. It is first polled once the current
/// job returns.
pub(crate) fn spawn(lua: &Lua, task: impl Future<Output = ()> + Send + 'static) {
    if let Some(registry) = lua.app_data_ref::<Registry>() {
        registry.spawned.borrow_mut().push(Box::pin(task));
    }
}

/// The tasks spawned since the last call, for the owner loop.
pub(crate) fn take_spawned(lua: &Lua) -> Vec<Task> {
    lua.app_data_ref::<Registry>()
        .map(|registry| std::mem::take(&mut *registry.spawned.borrow_mut()))
        .unwrap_or_default()
}

/// Whether the running Lua code is a coroutine [`drive`] started, where
/// host functions may yield.
pub(crate) fn is_driven(lua: &Lua) -> bool {
    let key = lua.current_thread().to_pointer() as usize;
    lua.app_data_ref::<Registry>()
        .is_some_and(|registry| registry.driven.borrow().contains(&key))
}

/// Run `func` with `args` as a driven coroutine. The future resolves to
/// what `func` returns.
pub(crate) fn drive<R: FromLuaMulti + Send + 'static>(
    lua: &Lua,
    func: Function,
    args: impl IntoLuaMulti,
) -> mlua::Result<impl Future<Output = mlua::Result<R>> + Send + 'static> {
    let thread = lua.create_thread(func)?;
    watchdog::install_on_thread(&thread).map_err(mlua::Error::external)?;
    let key = thread.to_pointer() as usize;
    if let Some(registry) = lua.app_data_ref::<Registry>() {
        registry.driven.borrow_mut().insert(key);
    }
    let guard = Driven {
        lua: lua.clone(),
        key,
    };
    let run = thread.into_async::<R>(args)?;
    Ok(async move {
        let _guard = guard;
        run.await
    })
}

/// Forgets a driven coroutine once its future is done or dropped.
struct Driven {
    lua: Lua,
    key: usize,
}

impl Drop for Driven {
    fn drop(&mut self) {
        if let Some(registry) = self.lua.app_data_ref::<Registry>() {
            registry.driven.borrow_mut().remove(&self.key);
        }
    }
}

/// A switch another thread flips to stop a task at its next poll.
#[derive(Default)]
pub(crate) struct Abort {
    aborted: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl Abort {
    /// Stop the task: it is woken and drops what it was awaiting.
    pub(crate) fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        if let Some(waker) = lock(&self.waker).take() {
            waker.wake();
        }
    }
}

/// What `future` resolves to, or `None` once `abort` is flipped. The
/// future is dropped then, so a driven coroutine ends at the point it
/// was waiting.
pub(crate) async fn abortable<F: Future>(future: F, abort: Arc<Abort>) -> Option<F::Output> {
    let mut future = pin!(future);
    poll_fn(|cx| {
        if abort.aborted.load(Ordering::SeqCst) {
            return Poll::Ready(None);
        }
        *lock(&abort.waker) = Some(cx.waker().clone());
        // Flipped between the check and storing the waker: the waker
        // stored is never woken, so look again.
        if abort.aborted.load(Ordering::SeqCst) {
            return Poll::Ready(None);
        }
        future.as_mut().poll(cx).map(Some)
    })
    .await
}

/// Run `work` on a thread of its own and await its result. `None` when
/// the work panicked.
pub(crate) fn off_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> impl Future<Output = Option<T>> + Send + 'static {
    let (tx, mut rx) = channel();
    let spawned = thread::Builder::new()
        .name("kage-lua-io".to_owned())
        .spawn(move || {
            tx.send(work());
        });
    async move {
        spawned.ok()?;
        rx.recv().await
    }
}

/// Wait `duration` without holding the owner thread.
pub(crate) async fn sleep(duration: Duration) {
    off_thread(move || thread::sleep(duration)).await;
}

/// A queue a worker thread fills and a driven coroutine drains.
pub(crate) fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Mutex::new(Shared {
        queue: VecDeque::new(),
        closed: false,
        abandoned: false,
        waker: None,
    }));
    (Sender(Arc::clone(&shared)), Receiver(shared))
}

struct Shared<T> {
    queue: VecDeque<T>,
    /// The sender is gone, so nothing more arrives.
    closed: bool,
    /// The receiver is gone, so nothing sent is read.
    abandoned: bool,
    waker: Option<Waker>,
}

/// The sending half of [`channel`]. Dropping it ends the stream.
pub(crate) struct Sender<T>(Arc<Mutex<Shared<T>>>);

/// The receiving half of [`channel`].
pub(crate) struct Receiver<T>(Arc<Mutex<Shared<T>>>);

impl<T> Sender<T> {
    /// Queue `value` and wake the receiver. `false` once the receiver
    /// is gone, so a worker can stop producing.
    pub(crate) fn send(&self, value: T) -> bool {
        let waker = {
            let mut shared = lock(&self.0);
            if shared.abandoned {
                return false;
            }
            shared.queue.push_back(value);
            shared.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        true
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut shared = lock(&self.0);
            shared.closed = true;
            shared.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut shared = lock(&self.0);
        shared.abandoned = true;
        shared.queue.clear();
    }
}

impl<T> Receiver<T> {
    /// The next value, or `None` once the sender is gone and the queue
    /// is empty.
    pub(crate) async fn recv(&mut self) -> Option<T> {
        poll_fn(|cx| {
            let mut shared = lock(&self.0);
            if let Some(value) = shared.queue.pop_front() {
                return Poll::Ready(Some(value));
            }
            if shared.closed {
                return Poll::Ready(None);
            }
            shared.waker = Some(cx.waker().clone());
            Poll::Pending
        })
        .await
    }
}
