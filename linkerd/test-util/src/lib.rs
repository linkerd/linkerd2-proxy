use std::future::Future;
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::time;

/// A trait that allows an executor to execute a future for up to a given
/// time limit, and then panics if the future has not finished.
///
/// This is intended for use in cases where the failure mode of some future
/// is to wait forever, rather than returning an error. When this happens,
/// it can make debugging test failures much more difficult, as killing
/// the tests when one has been waiting for over a minute prevents any
/// remaining tests from running, and doesn't print any output from the
/// killed test.
pub trait BlockOnFor {
    /// Runs the provided future for up to `timeout`, blocking the thread
    /// until the future completes.
    fn block_on_for<F>(&mut self, timeout: Duration, f: F) -> F::Output
    where
        F: Future;
}

impl BlockOnFor for Runtime {
    fn block_on_for<F>(&mut self, timeout: Duration, f: F) -> F::Output
    where
        F: Future,
    {
        match self.block_on(time::timeout(timeout, f)) {
            Ok(res) => res,
            Err(_) => {
                panic!(
                    "assertion failed: future did not finish within {:?}",
                    timeout
                );
            }
        }
    }
}
