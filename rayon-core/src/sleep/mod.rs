//! Code that decides when workers should go to sleep. See README.md
//! for an overview.

use crossbeam_utils::CachePadded;
use crate::latch::CoreLatch;
use crate::log::Logger;
use crate::log::Event::*;
use std::sync::atomic::Ordering;
use std::sync::{Condvar, Mutex};
use std::thread;
use std::usize;

mod counters;
use self::counters::{AtomicCounters, Counters, INVALID_SLEEPY_COUNTER, SleepyCounter, ZERO_SLEEPY_COUNTER};

pub(super) struct Sleep {
    logger: Logger,

    /// One "sleep state" per worker. Used to track if a worker is sleeping and to have
    /// them block.
    worker_sleep_states: Vec<CachePadded<WorkerSleepState>>,

    counters: AtomicCounters,
}

/// An instance of this struct is created when a thread becomes idle.
/// It is consumed when the thread finds work, and passed by `&mut`
/// reference for operations that preserve the idle state. (In other
/// words, producing one of these structs is evidence the thread is
/// idle.) It tracks state such as how long the thread has been idle.
pub(super) struct IdleState {
    /// What is worker index of the idle thread?
    worker_index: usize,

    /// How many rounds have we been circling without sleeping?
    rounds: u32,

    /// Once we become sleepy, what was the sleepy counter value?
    /// Set to `INVALID_SLEEPY_COUNTER` otherwise.
    sleepy_counter: SleepyCounter,
}

/// The "sleep state" for an individual worker.
#[derive(Default)]
struct WorkerSleepState {
    /// Set to true when the worker goes to sleep; set to false when
    /// the worker is notified or when it wakes.
    is_blocked: Mutex<bool>,

    condvar: Condvar,
}

const ROUNDS_UNTIL_SLEEPY: u32 = 32;
const ROUNDS_UNTIL_SLEEPING: u32 = ROUNDS_UNTIL_SLEEPY + 1;

impl Sleep {
    pub(super) fn new(logger: Logger, n_threads: usize) -> Sleep {
        Sleep {
            logger,
            worker_sleep_states: (0..n_threads).map(|_| Default::default()).collect(),
            counters: AtomicCounters::new(),
        }
    }

    #[inline]
    pub(super) fn start_looking(&self, worker_index: usize, latch: &CoreLatch) -> IdleState {
        self.logger.log(|| ThreadIdle {
            worker: worker_index,
            latch_addr: latch.addr(),
        });

        self.counters.add_idle_thread();

        IdleState {
            worker_index,
            rounds: 0,
            sleepy_counter: INVALID_SLEEPY_COUNTER,
        }
    }

    #[inline]
    pub(super) fn work_found(&self, idle_state: IdleState) {
        self.logger.log(|| ThreadFoundWork {
            worker: idle_state.worker_index,
            yields: idle_state.rounds,
        });

        // If we were the last idle thread and other threads are still sleeping,
        // then we should wake up another thread.
        let threads_to_wake = self.counters.sub_idle_thread();
        self.wake_any_threads(threads_to_wake as u32);
    }

    #[inline]
    pub(super) fn no_work_found(
        &self,
        idle_state: &mut IdleState,
        latch: &CoreLatch,
    ) {
        if idle_state.rounds < ROUNDS_UNTIL_SLEEPY {
            thread::yield_now();
            idle_state.rounds += 1;
        } else if idle_state.rounds == ROUNDS_UNTIL_SLEEPY {
            idle_state.sleepy_counter = self.announce_sleepy(idle_state.worker_index);
            idle_state.rounds += 1;
            thread::yield_now();
        } else if idle_state.rounds < ROUNDS_UNTIL_SLEEPING {
            idle_state.rounds += 1;
            thread::yield_now();
        } else {
            debug_assert_eq!(idle_state.rounds, ROUNDS_UNTIL_SLEEPING);
            self.sleep(idle_state, latch);
        }
    }

    #[cold]
    fn announce_sleepy(&self, worker_index: usize) -> SleepyCounter {
        loop {
            let counters = self.counters.load(Ordering::Relaxed);
            let sleepy_counter = counters.sleepy_counter();
            if self.counters.try_add_sleepy_thread(counters) {
                self.logger.log(|| ThreadSleepy { worker: worker_index, sleepy_counter: sleepy_counter.as_u16() });
                return sleepy_counter;
            }
        }
    }

    #[cold]
    fn sleep(&self, idle_state: &mut IdleState, latch: &CoreLatch) {
        let worker_index = idle_state.worker_index;

        if !latch.get_sleepy() {
            self.logger.log(|| ThreadSleepInterruptedByLatch {
                worker: worker_index,
                latch_addr: latch.addr(),
            });

            return;
        }

        let sleep_state = &self.worker_sleep_states[worker_index];
        let mut is_blocked = sleep_state.is_blocked.lock().unwrap();
        debug_assert!(!*is_blocked);

        // Our latch was signalled. We should wake back up fully as we
        // wil have some stuff to do.
        if !latch.fall_asleep() {
            self.logger.log(|| ThreadSleepInterruptedByLatch {
                worker: worker_index,
                latch_addr: latch.addr(),
            });

            idle_state.wake_fully();
            return;
        }

        loop {
            let counters = self.counters.load(Ordering::SeqCst);
            if counters.jobs_counter() > idle_state.sleepy_counter {
                self.logger.log(|| ThreadSleepInterruptedByJob {
                    worker: worker_index,
                });

                // A new job was posted. We should return to just
                // before the SLEEPY state so we can do another search
                // and (if we fail to find work) go back to sleep.
                idle_state.wake_partly();
                latch.wake_up();
                return;
            }

            // Otherwise, let's move from IDLE to SLEEPING.
            if self.counters.try_add_sleeping_thread(counters) {
                break;
            }
        }

        // Successfully registered as asleep.

        self.logger.log(|| ThreadSleeping {
            worker: worker_index,
            latch_addr: latch.addr(),
        });

        // Flag ourselves as asleep and wait till we are notified.
        //
        // (Note that `is_blocked` is held under a mutex and the mutex
        // was acquired *before* we incremented the "sleepy
        // counter". This means that whomever is coming to wake us
        // will have to wait until we release the mutex in the call to
        // `wait`, so they will see this boolean as true.)
        *is_blocked = true;
        while *is_blocked {
            is_blocked = sleep_state.condvar.wait(is_blocked).unwrap();
        }

        // Update other state:
        idle_state.wake_fully();
        latch.wake_up();

        self.logger.log(|| ThreadAwoken {
            worker: worker_index,
            latch_addr: latch.addr(),
        });

    }

    /// Notify the given thread that it should wake up (if it is
    /// sleeping).  When this method is invoked, we typically know the
    /// thread is asleep, though in rare cases it could have been
    /// awoken by (e.g.) new work having been posted.
    pub(super) fn notify_worker_latch_is_set(&self, target_worker_index: usize) {
        self.wake_specific_thread(target_worker_index);
    }


    /// Signals that `num_jobs` new jobs were injected into the thread
    /// pool from outside. This function will ensure that there are
    /// threads available to process them, waking threads from sleep
    /// if necessary.
    ///
    /// # Parameters
    ///
    /// - `source_worker_index` -- index of the thread that did the
    ///   push, or `usize::MAX` if this came from outside the thread
    ///   pool -- it is used only for logging.
    /// - `num_jobs` -- lower bound on number of jobs available for stealing.
    ///   We'll try to get at least one thread per job.
    #[inline]
    pub(super) fn new_injected_jobs(
        &self,
        source_worker_index: usize,
        num_jobs: u32,
        queue_was_empty: bool,
    ) {
        self.new_jobs(source_worker_index, num_jobs, queue_was_empty)
    }

    /// Signals that `num_jobs` new jobs were pushed onto a thread's
    /// local deque. This function will try to ensure that there are
    /// threads available to process them, waking threads from sleep
    /// if necessary. However, this is not guaranteed: under certain
    /// race conditions, the function may fail to wake any new
    /// threads; in that case the existing thread should eventually
    /// pop the job.
    ///
    /// # Parameters
    ///
    /// - `source_worker_index` -- index of the thread that did the
    ///   push, or `usize::MAX` if this came from outside the thread
    ///   pool -- it is used only for logging.
    /// - `num_jobs` -- lower bound on number of jobs available for stealing.
    ///   We'll try to get at least one thread per job.
    #[inline]
    pub(super) fn new_internal_jobs(
        &self,
        source_worker_index: usize,
        num_jobs: u32,
        queue_was_empty: bool,
    ) {
        self.new_jobs(source_worker_index, num_jobs, queue_was_empty)
    }

    /// Common helper for `new_injected_jobs` and `new_internal_jobs`.
    #[inline]
    fn new_jobs(
        &self,
        source_worker_index: usize,
        num_jobs: u32,
        queue_was_empty: bool,
    ) {
        let mut counters = self.counters.load(Ordering::SeqCst);

        // If we find that the jobs counter is out of date, we have to fix that.
        if counters.jobs_counter() != counters.sleepy_counter() {
            self.sync_jobs_counter(&mut counters);
        }

        let num_awake_but_idle = counters.awake_but_idle_threads();
        let num_sleepers = counters.sleeping_threads();

        self.logger.log(|| JobThreadCounts {
            worker: source_worker_index,
            num_idle: num_awake_but_idle as u16,
            num_sleepers: num_sleepers as u16,
        });

        if num_sleepers == 0 {
            // nobody to wake
            return;
        }

        // Promote from u16 to u32 so we can interoperate with
        // num_jobs more easily.
        let num_awake_but_idle = num_awake_but_idle as u32;
        let num_sleepers = num_sleepers as u32;

        // If the queue is non-empty, then we always wake up a worker
        // -- clearly the existing idle jobs aren't enough. Otherwise,
        // check to see if we have enough idle workers.
        if !queue_was_empty {
            let num_to_wake = std::cmp::min(num_jobs, num_sleepers);
            self.wake_any_threads(num_to_wake);
        } else if num_awake_but_idle < num_jobs {
            let num_to_wake = std::cmp::min(num_jobs - num_awake_but_idle, num_sleepers);
            self.wake_any_threads(num_to_wake);
        }
    }

    /// Invoked when we find that the "jobs counter" is not equal to
    /// the "sleepy counter".  This means that there may be threads
    /// actively going to sleep. In that case, we have to synchronize
    /// the jobs counter with the sleepy counter using a
    /// compare-exchange. If it happens that this fails, then the
    /// sleepy thread may have actually gone to sleep, so we re-load
    /// the counters word.
    #[cold]
    fn sync_jobs_counter(&self, counters: &mut Counters) {
        loop {
            if counters.jobs_counter() == counters.sleepy_counter() {
                return;
            }

            if self.counters.try_replicate_sleepy_counter(*counters) {
                return;
            }

            *counters = self.counters.load(Ordering::SeqCst);
        }
    }

    #[cold]
    fn wake_any_threads(
        &self,
        mut num_to_wake: u32,
    ) {
        if num_to_wake > 0 {
            for i in 0..self.worker_sleep_states.len() {
                if self.wake_specific_thread(i) {
                    num_to_wake -= 1;
                    if num_to_wake == 0 {
                        return;
                    }
                }
            }
        }
    }

    fn wake_specific_thread(&self, index: usize) -> bool {
        let sleep_state = &self.worker_sleep_states[index];

        let mut is_blocked = sleep_state.is_blocked.lock().unwrap();
        if *is_blocked {
            *is_blocked = false;
            sleep_state.condvar.notify_one();

            // When the thread went to sleep, it will have incremented
            // this value. When we wake it, its our job to decrement
            // it. We could have the thread do it, but that would
            // introduce a delay between when the thread was
            // *notified* and when this counter was decremented. That
            // might mislead people with new work into thinking that
            // there are sleeping threads that they should try to
            // wake, when in fact there is nothing left for them to
            // do.
            self.counters.sub_sleeping_thread();

            self.logger.log(|| ThreadNotify {
                worker: index,
            });

            true
        } else {
            false
        }
    }
}

impl IdleState {
    fn wake_fully(&mut self) {
        self.rounds = 0;
        self.sleepy_counter = INVALID_SLEEPY_COUNTER;
    }

    fn wake_partly(&mut self) {
        self.rounds = ROUNDS_UNTIL_SLEEPY;
        self.sleepy_counter = INVALID_SLEEPY_COUNTER;
    }
}

