use std::any::Any;
use std::cell::RefCell;
use std::mem;
use std::panic::{self, AssertUnwindSafe};

use context::Context;
use context::stack::{Stack, ProtectedFixedSizeStack};
use futures::{Async, Future, Poll};
use futures::unsync::oneshot::{self, Receiver};
use tokio_core::reactor::Handle;

use errors::{Dropped, TaskFailed};
use switch::{Switch, WaitTask};

pub enum TaskResult<R> {
    Panicked(Box<Any + Send + 'static>),
    Finished(R),
}

/// A `Future` representing a completion of a coroutine.
pub struct CoroutineResult<R> {
    receiver: Receiver<TaskResult<R>>,
}

impl<R> Future for CoroutineResult<R> {
    type Item = R;
    type Error = TaskFailed;
    fn poll(&mut self) -> Poll<R, TaskFailed> {
        match self.receiver.poll() {
            Ok(Async::Ready(TaskResult::Panicked(reason))) => Err(TaskFailed::Panicked(reason)),
            Ok(Async::Ready(TaskResult::Finished(result))) => Ok(Async::Ready(result)),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(_) => Err(TaskFailed::Lost),
        }
    }
}

struct CoroutineContext {
    /// Use this to spawn waiting coroutines
    handle: Handle,
    /// The context that called us and we'll switch back to it when we wait for something.
    parent_context: Context,
    /// Our own stack. We keep ourselvel alive.
    stack: ProtectedFixedSizeStack,
}

thread_local! {
    static CONTEXTS: RefCell<Vec<CoroutineContext>> = RefCell::new(Vec::new());
}

/// A builder of coroutines.
///
/// This struct is the main entry point and a way to start coroutines of various kinds. It allows
/// both starting them with default parameters and configuring them with the builder pattern.
#[derive(Clone)]
pub struct Coroutine {
    handle: Handle,
    stack_size: usize,
    leak_on_panic: bool,
}

impl Coroutine {
    /// Starts building a coroutine.
    ///
    /// This constructor produces a new builder for coroutines. The builder can then be used to
    /// specify configuration of the coroutines.
    ///
    /// It is possible to spawn multiple coroutines from the same builder.
    ///
    /// # Parameters
    ///
    /// * `handle`: The coroutines need a reactor core to run on and schedule their control
    ///   switches. This is the handle to the reactor core to be used.
    ///
    /// # Examples
    ///
    /// ```
    /// # extern crate corona;
    /// # extern crate tokio_core;
    /// use corona::Coroutine;
    /// use tokio_core::reactor::Core;
    ///
    /// # fn main() {
    /// let core = Core::new().unwrap();
    /// let builder = Coroutine::new(core.handle());
    ///
    /// let coroutine = builder.spawn(|| { });
    /// # }
    ///
    /// ```
    pub fn new(handle: Handle) -> Self {
        Coroutine {
            handle,
            stack_size: Stack::default_size(),
            leak_on_panic: false,
        }
    }
    /// Spawns a coroutine directly.
    ///
    /// This constructor spawns a coroutine with default parameters without the inconvenience of
    /// handling a builder. It is equivalent to spawning it with an unconfigured builder.
    ///
    /// Unlike the [`spawn`](#method.spawn.html), this one can't fail, since the default parameters
    /// of the builder are expected to always work (if they don't, file a bug).
    ///
    /// # Examples
    ///
    /// ```
    /// # extern crate corona;
    /// # extern crate tokio_core;
    /// use corona::Coroutine;
    /// use tokio_core::reactor::Core;
    ///
    /// # fn main() {
    /// let core = Core::new().unwrap();
    ///
    /// let coroutine = Coroutine::with_defaults(core.handle(), || { });
    /// # }
    ///
    /// ```
    pub fn with_defaults<R, Task>(handle: Handle, task: Task) -> CoroutineResult<R>
    where
        R: 'static,
        Task: FnOnce() -> R + 'static,
    {
        Coroutine::new(handle).spawn(task)
    }
    pub fn spawn<R, Task>(&self, task: Task) -> CoroutineResult<R>
    where
        R: 'static,
        Task: FnOnce() -> R + 'static,
    {
        let (sender, receiver) = oneshot::channel();

        let handle = self.handle.clone();

        let perform = move |context, stack| {
            let my_context = CoroutineContext {
                handle,
                parent_context: context,
                stack,
            };
            CONTEXTS.with(|c| c.borrow_mut().push(my_context));
            let result = match panic::catch_unwind(AssertUnwindSafe(task)) {
                Ok(res) => TaskResult::Finished(res),
                Err(panic) => TaskResult::Panicked(panic),
            };
            // We are not interested in errors. They just mean the receiver is no longer
            // interested, which is fine by us.
            drop(sender.send(result));
            let my_context = CONTEXTS.with(|c| c.borrow_mut().pop().unwrap());
            (my_context.parent_context, my_context.stack)
        };
        Switch::run_new_coroutine(self.stack_size, Box::new(Some(perform)));

        CoroutineResult { receiver }
    }

    pub fn wait<I, E, Fut>(mut fut: Fut) -> Result<Result<I, E>, Dropped>
    where
        Fut: Future<Item = I, Error = E>,
    {
        // XXX Describe the magic here
        let my_context = CONTEXTS.with(|c| {
            c.borrow_mut().pop().expect("Can't wait outside of a coroutine")
        });
        let mut result: Option<Result<I, E>> = None;
        let (reply_instruction, context) = {
            let res_ref = &mut result as *mut _ as usize;
            let mut poll = move || {
                let res = match fut.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(ok)) => Ok(ok),
                    Err(err) => Err(err),
                };
                let result = res_ref as *mut Option<Result<I, E>>;
                unsafe { *result = Some(res) };
                Ok(Async::Ready(()))
            };
            let p: &mut FnMut() -> Poll<(), ()> = &mut poll;
            let handle = my_context.handle.clone();
            let mut task = WaitTask {
                poll: Some(unsafe { mem::transmute::<_, &'static mut _>(p) }),
                context: None,
                handle,
            };
            let instruction = Switch::WaitFuture { task };
            instruction.exchange(my_context.parent_context)
        };
        let new_context = CoroutineContext {
            parent_context: context,
            stack: my_context.stack,
            handle: my_context.handle,
        };
        CONTEXTS.with(|c| c.borrow_mut().push(new_context));
        match reply_instruction {
            Switch::Resume => (),
            Switch::Cleanup => return Err(Dropped),
            _ => unreachable!("Invalid instruction on wakeup"),
        }
        Ok(result.unwrap())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::rc::Rc;
    use std::time::Duration;

    use futures::future;
    use tokio_core::reactor::{Core, Timeout};

    use super::*;

    /// Test spawning and execution of tasks.
    #[test]
    fn spawn_some() {
        let mut core = Core::new().unwrap();
        let s1 = Rc::new(AtomicBool::new(false));
        let s2 = Rc::new(AtomicBool::new(false));
        let s1c = s1.clone();
        let s2c = s2.clone();
        let handle = core.handle();

        let builder = Coroutine::new(handle);
        // builder.stack_size(40960); XXX
        let builder_inner = builder.clone();

        let result = builder.spawn(move || {
            let result = builder_inner.spawn(move || {
                s2c.store(true, Ordering::Relaxed);
                42
            });
            s1c.store(true, Ordering::Relaxed);
            result
        });

        // Both coroutines run to finish
        assert!(s1.load(Ordering::Relaxed), "The outer closure didn't run");
        assert!(s2.load(Ordering::Relaxed), "The inner closure didn't run");
        // The result gets propagated through.
        let extract = result.and_then(|r| r);
        assert_eq!(42, core.run(extract).unwrap());
    }

    /// Wait for a future to complete.
    #[test]
    fn future_wait() {
        let mut core = Core::new().unwrap();
        let handle = core.handle();
        let (sender, receiver) = oneshot::channel();
        let all_done = Coroutine::with_defaults(core.handle(), move || {
            let msg = Coroutine::wait(receiver).unwrap().unwrap();
            msg
        });
        Coroutine::with_defaults(core.handle(), move || {
            let timeout = Timeout::new(Duration::from_millis(50), &handle).unwrap();
            Coroutine::wait(timeout).unwrap().unwrap();
            sender.send(42).unwrap();
        });
        assert_eq!(42, core.run(all_done).unwrap());
    }

    /// The panic doesn't kill the main thread, but is reported.
    #[test]
    fn panics() {
        let mut core = Core::new().unwrap();
        let handle = core.handle();
        match core.run(Coroutine::with_defaults(handle, || panic!("Test"))) {
            Err(TaskFailed::Panicked(_)) => (),
            _ => panic!("Panic not reported properly"),
        }
        let handle = core.handle();
        assert_eq!(42, core.run(Coroutine::with_defaults(handle, || 42)).unwrap());
    }

    /// It's impossible to wait on a future outside of a coroutine
    #[test]
    #[should_panic]
    fn panic_without_coroutine() {
        drop(Coroutine::wait(future::ok::<_, ()>(42)));
    }
}