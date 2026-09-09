//! Pure admission and queueing logic. No I/O; fed with [`Snapshot`]s by the
//! socket layer and unit-tested against a decision table.
//!
//! Rules:
//! - A job is **accepted** immediately when `running < max_jobs`,
//!   `mem_available > min_mem_available`, and nothing is already queued
//!   (FIFO fairness: capacity that frees up goes to the queue first).
//! - Otherwise it is **queued** unless `queue.len() >= queue_limit`, in which
//!   case it is **rejected** as saturated.
//! - The queue is ordered by `Priority::rank()` descending, then arrival.
//! - Jobserver tokens are *not* an admission input; they bound parallelism
//!   inside jobs.

use rbs_proto::{JobId, Priority};

use crate::sysinfo::Snapshot;

/// Admission limits, derived from the `[server]` config section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_jobs: u32,
    pub min_mem_available_bytes: u64,
    pub queue_limit: u32,
}

impl From<&rbs_config::Server> for Limits {
    fn from(s: &rbs_config::Server) -> Self {
        Limits {
            max_jobs: s.max_jobs,
            min_mem_available_bytes: u64::from(s.min_mem_available_gib) << 30,
            queue_limit: s.queue_limit,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Run now; the scheduler has already counted the job as running.
    Accept,
    /// Parked in the queue at 1-based `position`.
    Queue { position: usize },
    /// Queue full.
    Reject(RejectCause),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectCause {
    Saturated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    id: JobId,
    priority: Priority,
    seq: u64,
}

#[derive(Debug)]
pub struct Scheduler {
    limits: Limits,
    running: usize,
    queue: Vec<Entry>,
    seq: u64,
}

impl Scheduler {
    pub fn new(limits: Limits) -> Scheduler {
        Scheduler {
            limits,
            running: 0,
            queue: Vec::new(),
            seq: 0,
        }
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub fn running(&self) -> usize {
        self.running
    }

    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Whether a freshly submitted job would run immediately.
    pub fn accepting(&self, snap: &Snapshot) -> bool {
        self.queue.is_empty() && self.has_capacity(snap)
    }

    fn has_capacity(&self, snap: &Snapshot) -> bool {
        self.running < self.limits.max_jobs as usize
            && snap.mem_available_bytes > self.limits.min_mem_available_bytes
    }

    /// Decide what to do with a newly submitted job.
    pub fn admit(&mut self, id: JobId, priority: Priority, snap: &Snapshot) -> Decision {
        if self.accepting(snap) {
            self.running += 1;
            return Decision::Accept;
        }
        if self.queue.len() >= self.limits.queue_limit as usize {
            return Decision::Reject(RejectCause::Saturated);
        }
        self.seq += 1;
        let entry = Entry {
            id,
            priority,
            seq: self.seq,
        };
        let idx = self
            .queue
            .iter()
            .position(|e| e.priority.rank() < priority.rank())
            .unwrap_or(self.queue.len());
        self.queue.insert(idx, entry);
        Decision::Queue { position: idx + 1 }
    }

    /// A running job ended; release the slot. Returns jobs that may start now.
    pub fn finish(&mut self, snap: &Snapshot) -> Vec<JobId> {
        self.running = self.running.saturating_sub(1);
        self.poll(snap)
    }

    /// Re-evaluate the queue against a fresh snapshot (memory may have freed up).
    /// Each returned job is counted as running from now on.
    pub fn poll(&mut self, snap: &Snapshot) -> Vec<JobId> {
        let mut released = Vec::new();
        while !self.queue.is_empty() && self.has_capacity(snap) {
            let e = self.queue.remove(0);
            self.running += 1;
            released.push(e.id);
        }
        released
    }

    /// Remove a queued job (cancel/disconnect). Returns `true` if it was queued.
    pub fn remove_queued(&mut self, id: JobId) -> bool {
        match self.queue.iter().position(|e| e.id == id) {
            Some(i) => {
                self.queue.remove(i);
                true
            }
            None => false,
        }
    }

    /// 1-based queue position of `id`, if queued.
    pub fn position(&self, id: JobId) -> Option<usize> {
        self.queue.iter().position(|e| e.id == id).map(|i| i + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn limits() -> Limits {
        Limits {
            max_jobs: 2,
            min_mem_available_bytes: 8 * GIB,
            queue_limit: 3,
        }
    }

    fn snap(mem_gib: u64) -> Snapshot {
        Snapshot {
            mem_available_bytes: mem_gib * GIB,
            load1: 0.0,
        }
    }

    #[test]
    fn limits_from_config_scale_gib() {
        let s = rbs_config::Server {
            max_jobs: 3,
            min_mem_available_gib: 2,
            queue_limit: 7,
            ..rbs_config::Server::default()
        };
        assert_eq!(
            Limits::from(&s),
            Limits {
                max_jobs: 3,
                min_mem_available_bytes: 2 * GIB,
                queue_limit: 7
            }
        );
    }

    #[test]
    fn decision_table() {
        // (running, queued, mem_gib, expect)
        struct Case {
            running: usize,
            queued: usize,
            mem_gib: u64,
            expect: Decision,
        }
        let cases = [
            Case {
                running: 0,
                queued: 0,
                mem_gib: 32,
                expect: Decision::Accept,
            },
            Case {
                running: 1,
                queued: 0,
                mem_gib: 32,
                expect: Decision::Accept,
            },
            Case {
                running: 2,
                queued: 0,
                mem_gib: 32,
                expect: Decision::Queue { position: 1 },
            },
            Case {
                running: 0,
                queued: 0,
                mem_gib: 8, // not strictly greater than the minimum
                expect: Decision::Queue { position: 1 },
            },
            Case {
                running: 0,
                queued: 0,
                mem_gib: 4,
                expect: Decision::Queue { position: 1 },
            },
            Case {
                running: 2,
                queued: 2,
                mem_gib: 32,
                expect: Decision::Queue { position: 3 },
            },
            Case {
                running: 2,
                queued: 3,
                mem_gib: 32,
                expect: Decision::Reject(RejectCause::Saturated),
            },
        ];
        for (i, c) in cases.iter().enumerate() {
            let mut s = Scheduler::new(limits());
            for _ in 0..c.running {
                assert_eq!(
                    s.admit(JobId(100), Priority::Agent, &snap(32)),
                    Decision::Accept
                );
            }
            // Park `queued` jobs by starving memory.
            for q in 0..c.queued {
                assert!(matches!(
                    s.admit(JobId(200 + q as u64), Priority::Agent, &snap(0)),
                    Decision::Queue { .. }
                ));
            }
            assert_eq!(s.running(), c.running);
            assert_eq!(s.queued(), c.queued);
            let got = s.admit(JobId(1), Priority::Agent, &snap(c.mem_gib));
            assert_eq!(got, c.expect, "case {i}");
        }
    }

    #[test]
    fn queue_nonempty_blocks_direct_accept_even_with_capacity() {
        let mut s = Scheduler::new(limits());
        assert!(matches!(
            s.admit(JobId(1), Priority::Agent, &snap(0)),
            Decision::Queue { position: 1 }
        ));
        assert!(!s.accepting(&snap(32)));
        assert_eq!(
            s.admit(JobId(2), Priority::Agent, &snap(32)),
            Decision::Queue { position: 2 }
        );
        // Memory recovers: poll releases in order, both fit (max_jobs = 2).
        assert_eq!(s.poll(&snap(32)), vec![JobId(1), JobId(2)]);
        assert_eq!(s.running(), 2);
        assert_eq!(s.queued(), 0);
    }

    #[test]
    fn priority_then_fifo_ordering() {
        let mut s = Scheduler::new(Limits {
            max_jobs: 1,
            min_mem_available_bytes: 0,
            queue_limit: 10,
        });
        assert_eq!(
            s.admit(JobId(0), Priority::Agent, &snap(1)),
            Decision::Accept
        );
        assert_eq!(
            s.admit(JobId(1), Priority::Background, &snap(1)),
            Decision::Queue { position: 1 }
        );
        assert_eq!(
            s.admit(JobId(2), Priority::Agent, &snap(1)),
            Decision::Queue { position: 1 }
        );
        assert_eq!(
            s.admit(JobId(3), Priority::Agent, &snap(1)),
            Decision::Queue { position: 2 }
        );
        assert_eq!(
            s.admit(JobId(4), Priority::Interactive, &snap(1)),
            Decision::Queue { position: 1 }
        );
        assert_eq!(s.position(JobId(1)), Some(4));
        assert_eq!(s.position(JobId(2)), Some(2));
        assert_eq!(s.position(JobId(0)), None);

        assert_eq!(s.finish(&snap(1)), vec![JobId(4)]);
        assert_eq!(s.finish(&snap(1)), vec![JobId(2)]);
        assert_eq!(s.finish(&snap(1)), vec![JobId(3)]);
        assert_eq!(s.finish(&snap(1)), vec![JobId(1)]);
        assert_eq!(s.finish(&snap(1)), Vec::<JobId>::new());
        assert_eq!(s.running(), 0);
    }

    #[test]
    fn finish_does_not_release_when_memory_is_low() {
        let mut s = Scheduler::new(limits());
        assert_eq!(
            s.admit(JobId(0), Priority::Agent, &snap(32)),
            Decision::Accept
        );
        assert!(matches!(
            s.admit(JobId(1), Priority::Agent, &snap(1)),
            Decision::Queue { .. }
        ));
        assert_eq!(s.finish(&snap(1)), Vec::<JobId>::new());
        assert_eq!(s.running(), 0);
        assert_eq!(s.queued(), 1);
        assert_eq!(s.poll(&snap(32)), vec![JobId(1)]);
    }

    #[test]
    fn remove_queued_and_underflow_safety() {
        let mut s = Scheduler::new(limits());
        assert!(matches!(
            s.admit(JobId(1), Priority::Agent, &snap(0)),
            Decision::Queue { .. }
        ));
        assert!(s.remove_queued(JobId(1)));
        assert!(!s.remove_queued(JobId(1)));
        assert_eq!(s.queued(), 0);
        assert!(s.accepting(&snap(32)));
        // finish with nothing running must not underflow.
        assert_eq!(s.finish(&snap(32)), Vec::<JobId>::new());
        assert_eq!(s.running(), 0);
    }

    #[test]
    fn zero_queue_limit_rejects_immediately() {
        let mut s = Scheduler::new(Limits {
            max_jobs: 0,
            min_mem_available_bytes: 0,
            queue_limit: 0,
        });
        assert_eq!(
            s.admit(JobId(1), Priority::Interactive, &snap(64)),
            Decision::Reject(RejectCause::Saturated)
        );
    }
}
