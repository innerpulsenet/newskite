//! Coalescing of the high-rate service events before they reach the UI thread.
//!
//! The event forwarder thread receives every service event as it happens. Two kinds are
//! frequent: `SegmentDone` (one per stored article, 100+ per second at speed) and the
//! 1 Hz `Throughput` sample. Sending each through a channel would let a stalled UI
//! thread (a hidden window, a long frame) pile them up without bound, and stamping a
//! throughput sample when the UI drains it would put a whole backlog at the same
//! instant. Instead the forwarder folds them into a [`Pending`] tally: segments are
//! summed per job, throughput samples are stamped when they arrive and capped at the
//! live window. Memory stays bounded however long the UI is away.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use super::app::LIVE_WINDOW_SECS;

/// Articles and bytes stored for one job since the last drain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SegmentTally {
    /// Articles (segments) fetched and decoded.
    pub articles: u64,
    /// Decoded bytes.
    pub bytes: u64,
}

/// Frequent events received but not yet shown.
#[derive(Debug, Default)]
pub struct Pending {
    segments: BTreeMap<i64, SegmentTally>,
    throughput: VecDeque<(i64, u64)>,
}

/// What [`Pending::take`] returns: per-job segment tallies and throughput samples.
pub type Drained = (Vec<(i64, SegmentTally)>, Vec<(i64, u64)>);

/// The tally shared by the forwarder thread and the UI thread.
pub type SharedPending = Arc<Mutex<Pending>>;

impl Pending {
    /// Counts one stored segment of `job_id`.
    pub fn add_segment(&mut self, job_id: i64, bytes: u64) {
        let t = self.segments.entry(job_id).or_default();
        t.articles = t.articles.saturating_add(1);
        t.bytes = t.bytes.saturating_add(bytes);
    }

    /// Keeps a throughput sample stamped `ts` (unix seconds, when it was received);
    /// only the newest [`LIVE_WINDOW_SECS`] are kept.
    pub fn add_throughput(&mut self, ts: i64, bytes_per_sec: u64) {
        self.throughput.push_back((ts, bytes_per_sec));
        while self.throughput.len() > LIVE_WINDOW_SECS {
            self.throughput.pop_front();
        }
    }

    /// Nothing is waiting.
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty() && self.throughput.is_empty()
    }

    /// Takes everything received so far: per-job segment tallies and the throughput
    /// samples, oldest first.
    pub fn take(&mut self) -> Drained {
        let segments = std::mem::take(&mut self.segments).into_iter().collect();
        let throughput = std::mem::take(&mut self.throughput).into_iter().collect();
        (segments, throughput)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_are_summed_per_job() {
        let mut p = Pending::default();
        for i in 0..10_000 {
            p.add_segment(if i % 2 == 0 { 7 } else { 9 }, 100);
        }
        let (segs, tp) = p.take();
        assert!(tp.is_empty());
        assert_eq!(
            segs,
            vec![
                (
                    7,
                    SegmentTally {
                        articles: 5_000,
                        bytes: 500_000
                    }
                ),
                (
                    9,
                    SegmentTally {
                        articles: 5_000,
                        bytes: 500_000
                    }
                ),
            ]
        );
        assert!(p.is_empty(), "take empties the tally");
    }

    #[test]
    fn throughput_keeps_arrival_times_and_stays_bounded() {
        let mut p = Pending::default();
        // A UI thread away for 700 s: the samples keep their own seconds.
        for i in 0..700 {
            p.add_throughput(1_000 + i, i as u64);
        }
        let (_, tp) = p.take();
        assert_eq!(tp.len(), LIVE_WINDOW_SECS);
        assert_eq!(tp.first(), Some(&(1_100, 100)), "oldest dropped first");
        assert_eq!(tp.last(), Some(&(1_699, 699)));
        assert!(
            tp.windows(2).all(|w| w[0].0 < w[1].0),
            "distinct, ordered times"
        );
    }
}
