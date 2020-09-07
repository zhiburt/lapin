use lapin::{executor::Executor, ConnectionProperties, Result};
use std::{future::Future, pin::Pin};

// ConnectionProperties extension

pub trait LapinAsyncGlobalExecutorExt {
    fn with_async_global_executor(self) -> Self
    where
        Self: Sized;
}

impl LapinAsyncGlobalExecutorExt for ConnectionProperties {
    fn with_async_global_executor(self) -> Self {
        self.with_executor(AsyncGlobalExecutorExecutor)
    }
}

// Executor

#[derive(Debug)]
// this needs to be public until lapin passes the executor itself to reactors
pub struct AsyncGlobalExecutorExecutor;

impl Executor for AsyncGlobalExecutorExecutor {
    fn spawn(&self, f: Pin<Box<dyn Future<Output = ()> + Send>>) -> Result<()> {
        async_global_executor::spawn(f).detach();
        Ok(())
    }
}
