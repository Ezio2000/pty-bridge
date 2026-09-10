//! Claude observation policy. No terminal text parsing or process-health inference.
use pty_core::{SessionState, Snapshot};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

pub const OUTPUT_SILENCE: Duration = Duration::from_secs(30);
pub const EMPTY_SILENCE: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub interaction_id: u64,
    pub output_seq: u64,
    pub start_cursor: u64,
    pub end_cursor: u64,
}

#[derive(Default)]
pub struct Observation {
    ranges: Vec<(u64, u64)>,
    notified: Option<u64>,
}

impl Observation {
    pub fn notice_delivered(&mut self, interaction_id: u64) {
        self.notified = Some(self.notified.unwrap_or(0).max(interaction_id));
    }
    pub fn read(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        self.ranges.push((start, end));
        self.ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (start, end) in self.ranges.drain(..) {
            if let Some(last) = merged.last_mut()
                && start <= last.1
            {
                last.1 = last.1.max(end);
                continue;
            }
            merged.push((start, end));
        }
        self.ranges = merged;
    }

    pub fn candidate(&mut self, snapshot: &Snapshot, now: Instant) -> Option<Candidate> {
        let activity = &snapshot.activity;
        // Old, dropped ranges never count as observed and need not accumulate forever.
        self.ranges
            .retain(|&(_, end)| end > snapshot.retained_start);
        if snapshot.state != SessionState::Running
            || snapshot.ending
            || activity.pending_inputs != 0
            || self.notified == Some(activity.interaction_id)
        {
            return None;
        }
        let has_output = activity.output_seq > activity.interaction_output_seq;
        let silence = if has_output {
            now.saturating_duration_since(activity.last_output_at?) >= OUTPUT_SILENCE
        } else {
            now.saturating_duration_since(activity.interaction_at) >= EMPTY_SILENCE
        };
        if !silence {
            return None;
        }
        let start = activity.interaction_cursor;
        let end = snapshot.retained_end;
        // A protocol-only output has no visible bytes to inspect, so it remains eligible.
        if has_output && start < end && self.ranges.iter().any(|&(a, b)| a <= start && b >= end) {
            return None;
        }
        Some(Candidate {
            interaction_id: activity.interaction_id,
            output_seq: activity.output_seq,
            start_cursor: start,
            end_cursor: end,
        })
    }

    pub fn claim(&mut self, snapshot: &Snapshot, candidate: &Candidate, now: Instant) -> bool {
        if self.candidate(snapshot, now).as_ref() != Some(candidate) {
            return false;
        }
        self.notice_delivered(candidate.interaction_id);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pty_core::Activity;
    fn snapshot(now: Instant) -> Snapshot {
        Snapshot {
            state: SessionState::Running,
            termination: None,
            rows: 24,
            cols: 80,
            created_at_ms: 0,
            last_output_at_ms: None,
            last_input_at_ms: None,
            retained_start: 0,
            retained_end: 0,
            ending: false,
            activity: Activity {
                interaction_id: 1,
                interaction_at: now,
                interaction_output_seq: 0,
                interaction_cursor: 0,
                output_seq: 0,
                last_output_at: None,
                pending_inputs: 0,
            },
        }
    }
    fn output(s: &mut Snapshot, now: Instant, end: u64) {
        s.activity.output_seq += 1;
        s.activity.last_output_at = Some(now);
        s.retained_end = end;
    }
    #[test]
    fn empty_read_does_not_reset_ninety_second_deadline() {
        let now = Instant::now();
        let s = snapshot(now);
        let mut o = Observation::default();
        o.read(0, 0);
        assert!(o.candidate(&s, now + Duration::from_secs(89)).is_none());
        let c = o.candidate(&s, now + EMPTY_SILENCE).unwrap();
        assert!(o.claim(&s, &c, now + EMPTY_SILENCE));
        assert!(o.candidate(&s, now + Duration::from_secs(999)).is_none());
    }
    #[test]
    fn precise_ranges_partial_reads_and_gaps() {
        let now = Instant::now();
        let mut s = snapshot(now);
        output(&mut s, now, 10);
        let mut o = Observation::default();
        assert!(o.candidate(&s, now + Duration::from_secs(29)).is_none());
        o.read(5, 10);
        assert!(o.candidate(&s, now + OUTPUT_SILENCE).is_some());
        o.read(0, 3);
        assert!(o.candidate(&s, now + OUTPUT_SILENCE).is_some());
        o.read(3, 5);
        assert!(o.candidate(&s, now + OUTPUT_SILENCE).is_none());
        output(&mut s, now + OUTPUT_SILENCE, 15);
        assert!(o.candidate(&s, now + 2 * OUTPUT_SILENCE).is_some());
    }
    #[test]
    fn stale_candidates_are_revalidated_and_output_does_not_rearm() {
        let now = Instant::now();
        let mut s = snapshot(now);
        output(&mut s, now, 5);
        let mut o = Observation::default();
        let c = o.candidate(&s, now + OUTPUT_SILENCE).unwrap();
        output(&mut s, now + OUTPUT_SILENCE, 10);
        assert!(!o.claim(&s, &c, now + OUTPUT_SILENCE));
        let c = o.candidate(&s, now + 2 * OUTPUT_SILENCE).unwrap();
        assert!(o.claim(&s, &c, now + 2 * OUTPUT_SILENCE));
        output(&mut s, now + 2 * OUTPUT_SILENCE, 15);
        assert!(o.candidate(&s, now + 3 * OUTPUT_SILENCE).is_none());
        s.activity.interaction_id += 1;
        s.activity.interaction_at = now + 3 * OUTPUT_SILENCE;
        s.activity.interaction_output_seq = s.activity.output_seq;
        s.activity.interaction_cursor = s.retained_end;
        assert!(
            o.candidate(&s, now + 3 * OUTPUT_SILENCE + EMPTY_SILENCE)
                .is_some()
        );
    }
    #[test]
    fn reading_exit_and_inflight_write_cancel_queued_notice() {
        let now = Instant::now();
        let mut s = snapshot(now);
        output(&mut s, now, 5);
        let mut o = Observation::default();
        let c = o.candidate(&s, now + OUTPUT_SILENCE).unwrap();
        s.activity.pending_inputs = 1;
        assert!(!o.claim(&s, &c, now + OUTPUT_SILENCE));
        s.activity.pending_inputs = 0;
        o.read(0, 5);
        assert!(!o.claim(&s, &c, now + OUTPUT_SILENCE));
        let mut o = Observation::default();
        s.ending = true;
        assert!(o.candidate(&s, now + OUTPUT_SILENCE).is_none());
        s.ending = false;
        s.state = SessionState::Finished;
        assert!(o.candidate(&s, now + OUTPUT_SILENCE).is_none());
    }
    #[test]
    fn dropped_bytes_are_not_read_and_fast_echo_belongs_to_input() {
        let now = Instant::now();
        let mut s = snapshot(now);
        output(&mut s, now, 10);
        s.retained_start = 5;
        s.activity.interaction_at = now + Duration::from_millis(1);
        let mut o = Observation::default();
        o.read(5, 10);
        assert!(o.candidate(&s, now + OUTPUT_SILENCE).is_some());
    }
}
