use {
    crate::{
        future::{CatchUnwind, FutureExt},
        task::AtomicWaker,
    },
    futures_core::{
        future::Future,
        task::{Context, Poll},
    },
    pin_utils::unsafe_pinned,
    std::{
        cell::UnsafeCell,
        fmt,
        panic::{self, AssertUnwindSafe},
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    },
};

#[derive(Debug)]
struct State<T> {
    completed: AtomicBool,
    recv_task: AtomicWaker,
    data: UnsafeCell<Option<thread::Result<T>>>,
}

// State is basically a mutex
unsafe impl<T: Send> Send for State<T> {}
unsafe impl<T: Send> Sync for State<T> {}
// The state does not ever project to the inner T
impl<T> Unpin for State<T> {}

/// The handle to a spawned future returned by
/// [`join_handle`](crate::future::FutureExt::join_handle).
// A `JoinHandle` doesn't *have* to be used - it won't abort the spawned future on drop.
#[derive(Debug)]
pub struct JoinHandle<T> {
    state: Arc<State<T>>,
}

impl<T> JoinHandle<T> {
    /// Poll whether the future completed, but don't receive result
    pub fn poll_complete(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if self.state.completed.load(Ordering::Relaxed) {
            return Poll::Ready(());
        }
        self.state.recv_task.register(cx.waker());
        if self.state.completed.load(Ordering::Relaxed) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    /// Try receiving result from spawned future.
    ///
    /// Returns `None` if the future didn't complete yet or the result was already extracted.
    pub fn try_recv(&mut self) -> Option<T> {
        if !self.state.completed.load(Ordering::Acquire) {
            return None;
        }

        let data = unsafe { &mut *self.state.data.get() }.take();
        match data? {
            Ok(output) => Some(output),
            Err(e) => panic::resume_unwind(e),
        }
    }
}


impl<T: Send + 'static> Future for JoinHandle<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        if !self.state.completed.load(Ordering::Relaxed) {
            self.state.recv_task.register(cx.waker());
        }
        if !self.state.completed.load(Ordering::Acquire) {
            return Poll::Pending;
        }

        let data = unsafe { &mut *self.state.data.get() }.take();
        match data {
            // also None if someone keeps polling after it was ready.
            None => panic!("spawned future didn't run to completion"),
            Some(Ok(output)) => Poll::Ready(output),
            Some(Err(e)) => panic::resume_unwind(e),
        }
    }
}

/// A future which sends its output to the corresponding `JoinHandle`.
/// Created by [`join_handle`](crate::future::FutureExt::join_handle).
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct JoinFuture<Fut: Future> {
    state: Arc<State<<Fut as Future>::Output>>,
    future: CatchUnwind<AssertUnwindSafe<Fut>>,
}

impl<Fut: Future> Drop for JoinFuture<Fut> {
    fn drop(&mut self) {
        if !self.state.completed.load(Ordering::Relaxed) {
            self.state.completed.store(true, Ordering::Release);
            self.state.recv_task.wake();
        }
    }
}

impl<Fut: Future + fmt::Debug> fmt::Debug for JoinFuture<Fut> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("JoinFuture")
            .field(&self.future)
            .finish()
    }
}

impl<Fut: Future + Unpin> Unpin for JoinFuture<Fut> {}

impl<Fut: Future> JoinFuture<Fut> {
    unsafe_pinned!(future: CatchUnwind<AssertUnwindSafe<Fut>>);
}

impl<Fut: Future> Future for JoinFuture<Fut> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if !self.state.completed.load(Ordering::Relaxed) {
            let output = ready!(self.as_mut().future().poll(cx));

            *(unsafe { &mut *self.state.data.get() }) = Some(output);
            self.state.completed.store(true, Ordering::Release);
            self.state.recv_task.wake();
        }

        Poll::Ready(())
    }
}

pub(super) fn join_handle<Fut: Future>(future: Fut) -> (JoinFuture<Fut>, JoinHandle<Fut::Output>) {
    let state = Arc::new(State {
        completed: AtomicBool::new(false),
        recv_task: AtomicWaker::new(),
        data: UnsafeCell::new(None),
    });

    // AssertUnwindSafe is used here because `Send + 'static` is basically
    // an alias for an implementation of the `UnwindSafe` trait but we can't
    // express that in the standard library right now.
    let wrapped = JoinFuture {
        future: AssertUnwindSafe(future).catch_unwind(),
        state: state.clone(),
    };

    (wrapped, JoinHandle { state })
}
