use crate::{thread::ThreadHandle, Result};
use async_task::Task;
use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use std::{
    cell::RefCell, fmt, future::Future, ops::Deref, pin::Pin, sync::Arc,
    thread::Builder as ThreadBuilder,
};

thread_local!(static LAPIN_EXECUTOR_THREAD: RefCell<bool> = RefCell::new(false));

pub(crate) fn within_executor() -> bool {
    LAPIN_EXECUTOR_THREAD.with(|executor_thread| *executor_thread.borrow())
}

pub trait Executor: std::fmt::Debug + Send + Sync {
    fn spawn(&self, f: Pin<Box<dyn Future<Output = ()> + Send>>) -> Result<()>;
}

impl Executor for Arc<dyn Executor> {
    fn spawn(&self, f: Pin<Box<dyn Future<Output = ()> + Send>>) -> Result<()> {
        self.deref().spawn(f)
    }
}

#[derive(Clone)]
pub struct DefaultExecutor {
    sender: Sender<Option<Task<()>>>,
    receiver: Receiver<Option<Task<()>>>,
    threads: Arc<Mutex<Vec<ThreadHandle>>>,
}

impl DefaultExecutor {
    pub fn new(max_threads: usize) -> Result<Self> {
        let (sender, receiver) = crossbeam_channel::unbounded::<Option<Task<()>>>();
        let threads = Arc::new(Mutex::new(
            (1..=max_threads)
                .map(|id| {
                    let receiver = receiver.clone();
                    Ok(ThreadHandle::new(
                        ThreadBuilder::new()
                            .name(format!("executor {}", id))
                            .spawn(move || {
                                LAPIN_EXECUTOR_THREAD
                                    .with(|executor_thread| *executor_thread.borrow_mut() = true);
                                while let Ok(Some(task)) = receiver.recv() {
                                    task.run();
                                }
                                Ok(())
                            })?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?,
        ));
        Ok(Self {
            sender,
            receiver,
            threads,
        })
    }

    pub(crate) fn default() -> Result<Arc<dyn Executor>> {
        Ok(Arc::new(Self::new(1)?))
    }
}

impl fmt::Debug for DefaultExecutor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DefaultExecutor").finish()
    }
}

impl Executor for DefaultExecutor {
    fn spawn(&self, f: Pin<Box<dyn Future<Output = ()> + Send>>) -> Result<()> {
        let sender = self.sender.clone();
        let schedule = move |task| sender.send(Some(task)).expect("executor failed");
        let (task, _) = async_task::spawn(f, schedule, ());
        task.schedule();
        Ok(())
    }
}

impl Drop for DefaultExecutor {
    fn drop(&mut self) {
        if let Some(threads) = self.threads.try_lock() {
            for _ in threads.iter() {
                let _ = self.sender.send(None);
            }
            for thread in threads.iter() {
                if !thread.is_current() {
                    let _ = thread.wait("executor");
                }
            }
        }
    }
}
