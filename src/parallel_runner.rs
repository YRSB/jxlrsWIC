//! Multi-threaded `JxlParallelRunner` for `jxl-rs`.
//!
//! `jxl-rs` ships a single-threaded fallback: passing `None` to `JxlDecoder::process` uses a
//! `SequentialRunner` reporting `num_threads() == 1`, which makes the crate's own
//! `run_ordered` take a plain `for` loop. The parallelism hook is a trait, so supplying a
//! worker pool is the whole change.
//!
//! Threads are scoped to one decode rather than pooled. Explorer asks WIC for thumbnails one
//! small image at a time, and thread creation is noise next to decoding a megapixel frame, so
//! a persistent pool would add locking to every call for no measurable gain.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use jxl::api::{JxlParallelRunner, JxlParallelRunnerFun};

/// Environment variable that overrides the worker count.
///
/// Exists because the decoder is loaded into third-party hosts: an application that already
/// fans out over images may want the WIC decoder to stay narrow rather than compete for cores.
pub(crate) const THREADS_ENV: &str = "JXLRS_WIC_THREADS";

/// Hard ceiling on workers, so a stray environment variable cannot spawn an absurd number.
const MAX_THREADS: usize = 64;

/// Splits a `jxl-rs` work list across OS threads.
pub(crate) struct ThreadRunner {
    threads: usize,
}

impl ThreadRunner {
    /// Builds a runner sized from [`THREADS_ENV`], falling back to the machine's parallelism.
    pub(crate) fn new() -> Self {
        Self::with_threads(configured_threads())
    }

    /// Builds a runner with an explicit worker count, clamped into `1..=MAX_THREADS`.
    pub(crate) fn with_threads(threads: usize) -> Self {
        Self {
            threads: threads.clamp(1, MAX_THREADS),
        }
    }
}

/// Resolves the worker count for this process.
pub(crate) fn configured_threads() -> usize {
    if let Some(requested) = std::env::var(THREADS_ENV).ok().and_then(|raw| {
        raw.trim()
            .parse::<usize>()
            .ok()
            .filter(|threads| *threads > 0)
    }) {
        return requested.min(MAX_THREADS);
    }
    std::thread::available_parallelism()
        .map_or(1, |parallelism| parallelism.get())
        .min(MAX_THREADS)
}

impl JxlParallelRunner for ThreadRunner {
    fn run(&mut self, num: usize, fun: &JxlParallelRunnerFun<'_>) -> jxl::error::Result<()> {
        let workers = self.threads.min(num).max(1);
        if workers == 1 {
            for index in 0..num {
                fun(index)?;
            }
            return Ok(());
        }

        // Work stealing rather than a static split: `jxl-rs` hands out pass-group indices whose
        // cost varies widely with image content, so a fixed partition would leave workers idle
        // while one of them finishes the expensive groups.
        let next = AtomicUsize::new(0);
        let failure: Mutex<Option<jxl::error::Error>> = Mutex::new(None);

        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= num {
                        return;
                    }
                    if let Err(err) = fun(index) {
                        // Keep the first failure; later ones are almost certainly consequences
                        // of the same corrupt bitstream and would only add noise.
                        let mut slot = failure
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if slot.is_none() {
                            *slot = Some(err);
                        }
                        return;
                    }
                });
            }
        });

        match failure
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn num_threads(&self) -> usize {
        self.threads
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failure the tests can raise without depending on a specific `jxl-rs` error variant,
    /// whose set is `#[non_exhaustive]`.
    fn test_failure() -> jxl::error::Error {
        // `InvalidRawQuantTable` is a unit variant with no payload and no bitstream input, so
        // constructing it cannot itself fail.
        jxl::error::Error::InvalidRawQuantTable
    }

    #[test]
    fn runs_every_index_exactly_once() {
        let mut runner = ThreadRunner::with_threads(4);
        let seen = Mutex::new(vec![0u8; 100]);
        runner
            .run(100, &|index| {
                seen.lock().unwrap()[index] += 1;
                Ok(())
            })
            .expect("run");
        let seen = seen.into_inner().unwrap();
        assert_eq!(seen.len(), 100);
        assert!(seen.iter().all(|count| *count == 1), "seen={seen:?}");
    }

    #[test]
    fn single_worker_runs_in_order() {
        let mut runner = ThreadRunner::with_threads(1);
        let order = Mutex::new(Vec::new());
        runner
            .run(16, &|index| {
                order.lock().unwrap().push(index);
                Ok(())
            })
            .expect("run");
        let order = order.into_inner().unwrap();
        assert_eq!(order, (0..16).collect::<Vec<_>>());
    }

    #[test]
    fn propagates_the_first_failure() {
        let mut runner = ThreadRunner::with_threads(4);
        let attempts = AtomicUsize::new(0);
        let result = runner.run(32, &|index| {
            attempts.fetch_add(1, Ordering::Relaxed);
            if index == 7 {
                return Err(test_failure());
            }
            Ok(())
        });
        assert!(result.is_err(), "a worker failure must reach the caller");
        assert!(attempts.load(Ordering::Relaxed) <= 32);
    }

    #[test]
    fn clamps_invalid_worker_counts() {
        assert_eq!(ThreadRunner::with_threads(0).num_threads(), 1);
        assert_eq!(
            ThreadRunner::with_threads(usize::MAX).num_threads(),
            MAX_THREADS
        );
    }

    #[test]
    fn empty_work_list_is_a_no_op() {
        let mut runner = ThreadRunner::with_threads(8);
        runner.run(0, &|_| Ok(())).expect("run");
    }
}
