//! Startup ownership for threads created before the composition root is complete.
//! A failed registration closes and joins already started owners before returning
//! control to the native loader, which may immediately unload the library.

use std::thread::JoinHandle;

pub(crate) struct StartupTask<T> {
    runtime: tokio::runtime::Handle,
    task: Option<tokio::task::JoinHandle<T>>,
    close: Option<Box<dyn FnOnce()>>,
}

impl<T> StartupTask<T> {
    pub fn new(
        runtime: tokio::runtime::Handle,
        task: tokio::task::JoinHandle<T>,
        close: impl FnOnce() + 'static,
    ) -> Self {
        Self {
            runtime,
            task: Some(task),
            close: Some(Box::new(close)),
        }
    }

    pub fn into_task(mut self) -> tokio::task::JoinHandle<T> {
        self.close.take();
        self.task
            .take()
            .expect("startup task is transferred exactly once")
    }
}

impl<T> Drop for StartupTask<T> {
    fn drop(&mut self) {
        if let Some(close) = self.close.take() {
            close();
        }
        if let Some(task) = self.task.take() {
            let _ = self.runtime.block_on(task);
        }
    }
}

pub(crate) struct StartupThread<T> {
    thread: Option<JoinHandle<T>>,
    close: Option<Box<dyn FnOnce()>>,
}

impl<T> StartupThread<T> {
    pub fn new(thread: JoinHandle<T>, close: impl FnOnce() + 'static) -> Self {
        Self {
            thread: Some(thread),
            close: Some(Box::new(close)),
        }
    }

    /// Transfer the close/join obligation to the fully constructed module.
    pub fn into_thread(mut self) -> JoinHandle<T> {
        self.close.take();
        self.thread
            .take()
            .expect("startup thread is transferred exactly once")
    }
}

impl<T> Drop for StartupThread<T> {
    fn drop(&mut self) {
        if let Some(close) = self.close.take() {
            close();
        }
        if let Some(thread) = self.thread.take() {
            // Startup already failed; joining even a panicked owner proves it
            // cannot execute library code after the loader receives that error.
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;

    use super::*;

    #[test]
    fn failed_startup_waits_for_started_owner_cleanup_before_returning() {
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = Arc::clone(&finished);
        let (close, stop) = mpsc::channel();
        let thread = thread::spawn(move || {
            stop.recv().unwrap();
            worker_finished.store(true, Ordering::Release);
        });
        let guard = StartupThread::new(thread, move || {
            close.send(()).unwrap();
        });
        drop(guard);
        assert!(finished.load(Ordering::Acquire));
    }

    #[test]
    fn completed_startup_transfers_thread_without_closing_its_owner() {
        let closed = Arc::new(AtomicBool::new(false));
        let startup_closed = Arc::clone(&closed);
        let (close, stop) = mpsc::channel();
        let thread = thread::spawn(move || {
            stop.recv().unwrap();
            42
        });
        let guard = StartupThread::new(thread, move || {
            startup_closed.store(true, Ordering::Release);
        });
        let thread = guard.into_thread();
        assert!(!closed.load(Ordering::Acquire));
        close.send(()).unwrap();
        assert_eq!(thread.join().unwrap(), 42);
    }

    #[test]
    fn failed_startup_drains_nested_native_work_before_returning() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let completed = Arc::new(AtomicBool::new(false));
        let worker_completed = Arc::clone(&completed);
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = runtime.spawn(async move {
            stopped.await.unwrap();
            tokio::task::spawn_blocking(move || worker_completed.store(true, Ordering::Release))
                .await
                .unwrap();
        });
        let guard = StartupTask::new(runtime.handle().clone(), task, move || {
            let _ = stop.send(());
        });
        drop(guard);
        assert!(completed.load(Ordering::Acquire));
    }
}
