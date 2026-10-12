//! The delay of the live frames in the worker, for each stage.

use std::time::Duration;

use crate::capture::LiveTimes;

/// The number of live frames in one log line.
const FRAMES: usize = 300;

/// The delays of the stages of the live frames. Logs the median, the 99th
/// percentile and the largest delay of each stage every [`FRAMES`] frames, at
/// the debug level.
#[derive(Debug, Default)]
pub(crate) struct Delays {
    /// The delays from the capture time until the thread dequeues the camera
    /// frame.
    dequeue: Vec<Duration>,
    /// The delays from the dequeue time until the requantization ends.
    requantize: Vec<Duration>,
    /// The delays from the end of the requantization until the worker queues
    /// the live frame.
    queue: Vec<Duration>,
    /// The delays from the capture time until the worker queues the live
    /// frame.
    total: Vec<Duration>,
}

impl Delays {
    /// Records the delays of a live frame with the times `times`. `queued` is
    /// the time at which the worker queued the frame.
    pub(crate) fn record(&mut self, times: LiveTimes, queued: Duration) {
        self.dequeue
            .push(times.dequeued.saturating_sub(times.captured));
        self.requantize
            .push(times.requantized.saturating_sub(times.dequeued));
        self.queue.push(queued.saturating_sub(times.requantized));
        self.total.push(queued.saturating_sub(times.captured));
        if self.total.len() < FRAMES {
            return;
        }
        tracing::debug!(
            "live delay in ms over {FRAMES} frames, median/p99/max: dequeue {}, requantize {}, queue {}, total {}",
            summary(&mut self.dequeue),
            summary(&mut self.requantize),
            summary(&mut self.queue),
            summary(&mut self.total),
        );
        for samples in [
            &mut self.dequeue,
            &mut self.requantize,
            &mut self.queue,
            &mut self.total,
        ] {
            samples.clear();
        }
    }
}

/// Returns the median, the 99th percentile and the largest value of `samples`
/// in milliseconds. Sorts `samples`.
fn summary(samples: &mut [Duration]) -> String {
    samples.sort_unstable();
    let at = |percent: usize| {
        let index = (samples.len() * percent / 100).min(samples.len().saturating_sub(1));
        samples
            .get(index)
            .map_or(0.0, |delay| delay.as_secs_f64() * 1000.0)
    };
    format!("{:.1}/{:.1}/{:.1}", at(50), at(99), at(100))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn millis(values: &[u64]) -> Vec<Duration> {
        values.iter().copied().map(Duration::from_millis).collect()
    }

    /// Returns the samples 1 ms, 2 ms and so on up to 100 ms.
    fn samples_1_to_100_ms() -> Vec<Duration> {
        (1..=100).map(Duration::from_millis).collect()
    }

    /// Returns the times of a live frame with a delay of 1 ms at each stage.
    fn times() -> LiveTimes {
        LiveTimes {
            captured: Duration::from_millis(1),
            dequeued: Duration::from_millis(2),
            requantized: Duration::from_millis(3),
        }
    }

    /// Records `count` live frames.
    fn record_frames(delays: &mut Delays, count: usize) {
        for _ in 0..count {
            delays.record(times(), Duration::from_millis(4));
        }
    }

    #[test]
    fn summary_of_1_to_100_ms_is_51_100_100() {
        let mut samples = samples_1_to_100_ms();

        assert_eq!(summary(&mut samples), "51.0/100.0/100.0");
    }

    #[test]
    fn summary_sorts_the_samples() {
        let mut samples = millis(&[30, 10, 20]);

        assert_eq!(summary(&mut samples), "20.0/30.0/30.0");
    }

    #[test]
    fn summary_of_one_sample_repeats_it() {
        let mut samples = millis(&[7]);

        assert_eq!(summary(&mut samples), "7.0/7.0/7.0");
    }

    #[test]
    fn summary_without_samples_is_zero() {
        assert_eq!(summary(&mut []), "0.0/0.0/0.0");
    }

    #[test]
    fn record_keeps_the_samples_before_300_frames() {
        let mut delays = Delays::default();

        record_frames(&mut delays, FRAMES - 1);

        assert_eq!(delays.total.len(), FRAMES - 1);
    }

    #[test]
    fn record_clears_the_samples_after_300_frames() {
        let mut delays = Delays::default();

        record_frames(&mut delays, FRAMES);

        assert_eq!(delays.total.len(), 0);
    }
}
