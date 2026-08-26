use core::sync::atomic::{AtomicUsize, Ordering};
use std::{sync::Arc, thread::Builder};

use anyhow::Result;
use logging::debug_with_peers;
use std_ext::ArcExt as _;

/// Every archival thread owns a snapshot of the store for as long as it runs, so their combined
/// memory cost is bounded only by how many of them run at once. Work that finds no permit runs on
/// the calling thread instead of being dropped, so the real bound is one snapshot above this.
const MAX_CONCURRENT_ARCHIVAL_THREADS: usize = 4;

#[derive(Clone, Default)]
pub struct ArchivalPermits(Arc<AtomicUsize>);

impl ArchivalPermits {
    pub fn try_acquire(&self) -> Option<ArchivalPermit> {
        self.0
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |held| {
                (held < MAX_CONCURRENT_ARCHIVAL_THREADS).then(|| held.saturating_add(1))
            })
            .ok()
            .map(|_| ArchivalPermit(self.0.clone_arc()))
    }

    /// Run `work` on a thread named `name`, or, when no permit is available, on the calling
    /// thread. Running inline blocks the caller, but it keeps the number of store snapshots held
    /// at once bounded, which unconditional spawning would not.
    ///
    /// Both callers are the fork choice mutator, so falling back stalls block, attestation and
    /// tick processing for the length of one archival pass. That is deliberate: back pressure on
    /// the mutator is preferable to letting archival memory grow without a bound.
    pub fn spawn_or_run(&self, name: &str, work: impl FnOnce() + Send + 'static) -> Result<()> {
        match self.try_acquire() {
            Some(permit) => {
                Builder::new().name(name.to_owned()).spawn(move || {
                    work();
                    drop(permit);
                })?;
            }
            None => {
                debug_with_peers!(
                    "archival thread limit reached; running {name} on the calling thread"
                );

                work();
            }
        }

        Ok(())
    }

    #[cfg(test)]
    fn held(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

pub struct ArchivalPermit(Arc<AtomicUsize>);

impl Drop for ArchivalPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
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
    fn acquiring_is_bounded_and_permits_are_released_on_drop() {
        let permits = ArchivalPermits::default();

        let held = core::iter::repeat_with(|| permits.try_acquire())
            .take(MAX_CONCURRENT_ARCHIVAL_THREADS)
            .map(|permit| permit.expect("permits below the cap must be granted"))
            .collect::<Vec<_>>();

        assert_eq!(permits.held(), MAX_CONCURRENT_ARCHIVAL_THREADS);
        assert!(permits.try_acquire().is_none());

        drop(held);

        assert_eq!(permits.held(), 0);
        assert!(permits.try_acquire().is_some());
    }

    #[test]
    fn a_rejected_attempt_does_not_consume_a_permit() {
        let permits = ArchivalPermits::default();

        let held = core::iter::repeat_with(|| permits.try_acquire())
            .take(MAX_CONCURRENT_ARCHIVAL_THREADS)
            .map(|permit| permit.expect("permits below the cap must be granted"))
            .collect::<Vec<_>>();

        for _ in 0..3 {
            assert!(permits.try_acquire().is_none());
            assert_eq!(permits.held(), MAX_CONCURRENT_ARCHIVAL_THREADS);
        }

        drop(held);

        // A rejected attempt that had incremented the count would leave the cap
        // permanently short here.
        assert_eq!(permits.held(), 0);
    }

    #[test]
    fn concurrent_acquisition_grants_exactly_the_cap() {
        const CONTENDERS: usize = 32;

        let permits = ArchivalPermits::default();
        let barrier = Barrier::new(CONTENDERS);
        let granted = AtomicUsize::new(0);

        // Every contender holds its permit until all of them have made their
        // attempt, so the outcome does not depend on how the threads interleave.
        thread::scope(|scope| {
            for _ in 0..CONTENDERS {
                scope.spawn(|| {
                    let permit = permits.try_acquire();

                    if permit.is_some() {
                        granted.fetch_add(1, Ordering::SeqCst);
                    }

                    barrier.wait();

                    drop(permit);
                });
            }
        });

        assert_eq!(
            granted.load(Ordering::SeqCst),
            MAX_CONCURRENT_ARCHIVAL_THREADS
        );
        assert_eq!(permits.held(), 0);
    }

    #[test]
    fn spawn_or_run_runs_inline_only_when_the_cap_is_reached() -> Result<()> {
        let permits = ArchivalPermits::default();

        let (sender, receiver) = mpsc::channel();

        permits.spawn_or_run("test-archiver", {
            let sender = sender.clone();
            move || {
                sender
                    .send(thread::current().id())
                    .expect("the receiver is still alive")
            }
        })?;

        assert_ne!(receiver.recv()?, thread::current().id());

        // The spawned thread sends before it drops its permit, so the permit may still be held
        // when `recv` returns.
        while permits.held() > 0 {
            thread::yield_now();
        }

        let held = core::iter::repeat_with(|| permits.try_acquire())
            .take(MAX_CONCURRENT_ARCHIVAL_THREADS)
            .map(|permit| permit.expect("permits below the cap must be granted"))
            .collect::<Vec<_>>();

        permits.spawn_or_run("test-archiver", move || {
            sender
                .send(thread::current().id())
                .expect("the receiver is still alive")
        })?;

        assert_eq!(receiver.recv()?, thread::current().id());

        drop(held);

        Ok(())
    }

    #[test]
    fn permits_are_shared_between_clones() {
        let permits = ArchivalPermits::default();
        let clone = permits.clone();

        let permit = clone
            .try_acquire()
            .expect("the first permit must be granted");

        assert_eq!(permits.held(), 1);

        drop(permit);

        assert_eq!(permits.held(), 0);
    }
}
