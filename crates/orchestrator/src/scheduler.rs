//! Bounded concurrency for agent turns.
//!
//! At most `max_concurrent` turns run at once across all threads, and each
//! thread runs one turn at a time. Everything else waits in FIFO order.

use std::collections::{HashSet, VecDeque};

#[derive(Debug, PartialEq, Eq)]
pub enum Scheduled<J> {
    /// Start this job now.
    Start(J),
    /// Queued; `position` is 1-based.
    Queued { position: usize },
}

#[derive(Debug)]
pub struct Scheduler<J> {
    max_concurrent: usize,
    running: HashSet<String>,
    queue: VecDeque<(String, J)>,
}

impl<J> Scheduler<J> {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            max_concurrent: max_concurrent.max(1),
            running: HashSet::new(),
            queue: VecDeque::new(),
        }
    }

    pub fn set_max_concurrent(&mut self, max: usize) {
        self.max_concurrent = max.max(1);
    }

    pub fn submit(&mut self, thread_id: &str, job: J) -> Scheduled<J> {
        if self.can_start(thread_id) {
            self.running.insert(thread_id.to_string());
            Scheduled::Start(job)
        } else {
            self.queue.push_back((thread_id.to_string(), job));
            Scheduled::Queued {
                position: self.queue.len(),
            }
        }
    }

    /// Marks `thread_id`'s turn finished and returns the next job that may
    /// start, if any. Call repeatedly until `None` after raising the limit.
    pub fn finished(&mut self, thread_id: &str) -> Option<(String, J)> {
        self.running.remove(thread_id);
        self.next_startable()
    }

    /// Takes the next queued job that may start now.
    pub fn next_startable(&mut self) -> Option<(String, J)> {
        if self.running.len() >= self.max_concurrent {
            return None;
        }
        let index = self
            .queue
            .iter()
            .position(|(thread, _)| !self.running.contains(thread))?;
        let (thread, job) = self.queue.remove(index)?;
        self.running.insert(thread.clone());
        Some((thread, job))
    }

    /// Drops the queued (not running) jobs of `thread_id`.
    pub fn cancel_queued(&mut self, thread_id: &str) -> Vec<J> {
        let mut cancelled = Vec::new();
        let mut kept = VecDeque::with_capacity(self.queue.len());
        for (thread, job) in self.queue.drain(..) {
            if thread == thread_id {
                cancelled.push(job);
            } else {
                kept.push_back((thread, job));
            }
        }
        self.queue = kept;
        cancelled
    }

    pub fn is_running(&self, thread_id: &str) -> bool {
        self.running.contains(thread_id)
    }

    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    pub fn queued_for(&self, thread_id: &str) -> usize {
        self.queue.iter().filter(|(t, _)| t == thread_id).count()
    }

    pub fn queued_count(&self) -> usize {
        self.queue.len()
    }

    fn can_start(&self, thread_id: &str) -> bool {
        self.running.len() < self.max_concurrent && !self.running.contains(thread_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_concurrency_and_runs_in_order() {
        let mut s = Scheduler::new(2);
        assert_eq!(s.submit("a", 1), Scheduled::Start(1));
        assert_eq!(s.submit("b", 2), Scheduled::Start(2));
        assert_eq!(s.submit("c", 3), Scheduled::Queued { position: 1 });
        assert_eq!(s.submit("d", 4), Scheduled::Queued { position: 2 });
        assert_eq!(s.running_count(), 2);

        assert_eq!(s.finished("a"), Some(("c".into(), 3)));
        assert_eq!(s.finished("b"), Some(("d".into(), 4)));
        assert_eq!(s.finished("c"), None);
    }

    #[test]
    fn one_turn_per_thread() {
        let mut s = Scheduler::new(5);
        assert_eq!(s.submit("a", 1), Scheduled::Start(1));
        assert_eq!(s.submit("a", 2), Scheduled::Queued { position: 1 });
        assert_eq!(s.submit("b", 3), Scheduled::Start(3));
        assert_eq!(s.queued_for("a"), 1);
        // Finishing b does not start a's second turn while a still runs.
        assert_eq!(s.finished("b"), None);
        assert_eq!(s.finished("a"), Some(("a".into(), 2)));
    }

    #[test]
    fn cancelling_drops_only_that_threads_queue() {
        let mut s = Scheduler::new(1);
        s.submit("a", 1);
        s.submit("b", 2);
        s.submit("a", 3);
        assert_eq!(s.cancel_queued("a"), vec![3]);
        assert_eq!(s.queued_count(), 1);
        assert_eq!(s.finished("a"), Some(("b".into(), 2)));
    }

    #[test]
    fn raising_the_limit_releases_waiting_jobs() {
        let mut s = Scheduler::new(1);
        s.submit("a", 1);
        s.submit("b", 2);
        s.submit("c", 3);
        s.set_max_concurrent(3);
        assert_eq!(s.next_startable(), Some(("b".into(), 2)));
        assert_eq!(s.next_startable(), Some(("c".into(), 3)));
        assert_eq!(s.next_startable(), None);
    }
}
