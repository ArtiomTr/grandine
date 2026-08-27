use core::{
    num::NonZeroUsize,
    sync::atomic::{AtomicUsize, Ordering},
};
use std::{
    collections::VecDeque,
    sync::Arc,
    thread::{Builder, available_parallelism},
};

use anyhow::Result;
use parking_lot::Mutex;
use std_ext::ArcExt as _;

type Work = Box<dyn FnOnce() + Send + 'static>;

/// Thread pool for archival work.
///
/// Every archival task owns a snapshot of the store for as long as it runs, so the worker count
/// bounds how many snapshots are alive at once. The pool starts with no workers, spawns one per
/// submission until it reaches the cap, and lets workers exit once the queue drains.
#[derive(Clone)]
pub struct ArchivalPool(Arc<Pool>);

impl Default for ArchivalPool {
    fn default() -> Self {
        let cores = available_parallelism().map_or(1, NonZeroUsize::get);

        Self::with_max_workers((cores / 2).max(1))
    }
}

impl ArchivalPool {
    fn with_max_workers(max_workers: usize) -> Self {
        Self(Arc::new(Pool {
            queue: Mutex::new(VecDeque::new()),
            workers: AtomicUsize::new(0),
            max_workers,
        }))
    }

    pub fn submit(&self, work: impl FnOnce() + Send + 'static) -> Result<()> {
        self.0.submit(Box::new(work))
    }

    #[cfg(test)]
    fn workers(&self) -> usize {
        self.0.workers.load(Ordering::SeqCst)
    }
}

struct Pool {
    queue: Mutex<VecDeque<Work>>,
    /// Only ever mutated while the queue lock is held, so retiring a worker and enqueueing work
    /// cannot interleave.
    workers: AtomicUsize,
    max_workers: usize,
}

impl Pool {
    fn submit(self: &Arc<Self>, work: Work) -> Result<()> {
        let mut queue = self.queue.lock();

        queue.push_back(work);

        if self.workers.load(Ordering::SeqCst) == self.max_workers {
            return Ok(());
        }

        self.workers.fetch_add(1, Ordering::SeqCst);

        drop(queue);

        let pool = self.clone_arc();

        let result = Builder::new()
            .name("archiver".to_owned())
            .spawn(move || pool.run());

        if let Err(error) = result {
            self.workers.fetch_sub(1, Ordering::SeqCst);

            return Err(error.into());
        }

        Ok(())
    }

    fn run(&self) {
        while let Some(work) = self.next_work() {
            work();
        }
    }

    /// Take the next task, or retire this worker when the queue has drained.
    ///
    /// Retiring and submitting both happen under the queue lock, so a task pushed by a submitter
    /// that saw this worker still alive is always picked up before the worker exits.
    fn next_work(&self) -> Option<Work> {
        let mut queue = self.queue.lock();

        let work = queue.pop_front();

        if work.is_none() {
            self.workers.fetch_sub(1, Ordering::SeqCst);
        }

        work
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Barrier, mpsc},
        thread,
    };

    use super::*;

    #[test]
    fn all_submitted_work_runs_and_the_worker_count_stays_within_the_cap() -> Result<()> {
        const MAX_WORKERS: usize = 3;
        const TASKS: usize = 64;

        let pool = ArchivalPool::with_max_workers(MAX_WORKERS);
        let (sender, receiver) = mpsc::channel();
        let running = Arc::new(AtomicUsize::new(0));

        for task in 0..TASKS {
            let sender = sender.clone();
            let running = running.clone_arc();
            let pool_handle = pool.clone();

            pool.submit(move || {
                assert!(running.fetch_add(1, Ordering::SeqCst) < MAX_WORKERS);
                assert!(pool_handle.workers() <= MAX_WORKERS);

                thread::yield_now();

                running.fetch_sub(1, Ordering::SeqCst);

                sender.send(task).expect("the receiver is still alive");
            })?;
        }

        drop(sender);

        let mut completed = receiver.iter().collect::<Vec<_>>();

        completed.sort_unstable();

        assert_eq!(completed, (0..TASKS).collect::<Vec<_>>());

        Ok(())
    }

    #[test]
    fn the_pool_downscales_after_the_queue_drains() -> Result<()> {
        const MAX_WORKERS: usize = 4;
        const TASKS: usize = 32;

        let pool = ArchivalPool::with_max_workers(MAX_WORKERS);
        let (sender, receiver) = mpsc::channel();

        for _ in 0..TASKS {
            let sender = sender.clone();

            pool.submit(move || sender.send(()).expect("the receiver is still alive"))?;
        }

        drop(sender);

        assert_eq!(receiver.iter().count(), TASKS);

        // The last task sends before its worker looks at the queue again, so workers may still be
        // retiring when `recv` returns.
        while pool.workers() > 0 {
            thread::yield_now();
        }

        // A pool that downscaled to nothing still accepts work.
        let (sender, receiver) = mpsc::channel();

        pool.submit(move || sender.send(()).expect("the receiver is still alive"))?;

        assert_eq!(receiver.recv()?, ());

        Ok(())
    }

    #[test]
    fn work_submitted_from_several_threads_is_not_dropped() -> Result<()> {
        const SUBMITTERS: usize = 8;
        const TASKS_PER_SUBMITTER: usize = 32;

        let pool = ArchivalPool::with_max_workers(3);
        let (sender, receiver) = mpsc::channel();
        let barrier = Barrier::new(SUBMITTERS);

        thread::scope(|scope| {
            for submitter in 0..SUBMITTERS {
                let pool = &pool;
                let barrier = &barrier;
                let sender = sender.clone();

                scope.spawn(move || {
                    barrier.wait();

                    for task in 0..TASKS_PER_SUBMITTER {
                        let sender = sender.clone();

                        pool.submit(move || {
                            sender
                                .send((submitter, task))
                                .expect("the receiver is still alive")
                        })
                        .expect("submitting must succeed");
                    }
                });
            }
        });

        drop(sender);

        let mut completed = receiver.iter().collect::<Vec<_>>();

        completed.sort_unstable();

        let expected = (0..SUBMITTERS)
            .flat_map(|submitter| (0..TASKS_PER_SUBMITTER).map(move |task| (submitter, task)))
            .collect::<Vec<_>>();

        assert_eq!(completed, expected);

        Ok(())
    }

    #[test]
    fn the_default_cap_is_half_the_cores_and_at_least_one() {
        let cores = available_parallelism().map_or(1, NonZeroUsize::get);

        assert_eq!(ArchivalPool::default().0.max_workers, (cores / 2).max(1));
        assert!(ArchivalPool::default().0.max_workers > 0);
    }
}
