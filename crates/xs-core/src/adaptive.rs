//! Adaptive bitrate control.
//!
//! Deliberately pure: it takes health samples and returns a bitrate, touching no
//! sockets or pipelines. That makes the interesting part -- the part that decides
//! whether your display gets blurry -- testable without a tablet plugged in.
//!
//! The algorithm is AIMD, the same shape TCP congestion control uses: back off
//! multiplicatively when things go wrong, recover additively when they are fine.
//! The asymmetry matters. Dropping quality must be fast enough to clear a stall
//! before the user notices, while raising it must be slow enough that we do not
//! oscillate between sharp and blurry, which is far more distracting than simply
//! sitting at a slightly lower bitrate.

use std::time::{Duration, Instant};

/// Bitrate range the controller may move within.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitrateBounds {
    pub min_kbps: u32,
    pub max_kbps: u32,
}

impl Default for BitrateBounds {
    fn default() -> Self {
        // 6 Mbit/s still looks fine for text at this size; 30 is past the point
        // of visible improvement over USB 2.0 and just wastes headroom.
        Self {
            min_kbps: 6_000,
            max_kbps: 30_000,
        }
    }
}

/// One observation of how the tablet is coping.
#[derive(Debug, Clone, Copy)]
pub struct HealthSample {
    /// Frames sitting in the tablet's decoder input queue.
    pub decode_queue_depth: u32,
    /// Frames dropped since the previous sample, host side and device side.
    pub dropped_delta: u64,
    /// Measured round trip over the control channel.
    pub rtt: Duration,
}

/// Queue depth at or above which we consider the tablet to be struggling.
const QUEUE_DEPTH_BAD: u32 = 3;
/// Queue depth at or below which things are considered healthy.
const QUEUE_DEPTH_GOOD: u32 = 1;
/// Round trip above this means the link is congested regardless of queue depth.
const RTT_BAD: Duration = Duration::from_millis(120);

/// Dropped frames per sampling window that are treated as noise rather than
/// congestion.
///
/// Zero was too strict in practice: the encoder's output queue is deliberately
/// shallow, so at ~55 fps an occasional single drop is normal even on a healthy
/// link. Treating every one as congestion meant the good-sample streak never
/// reached the threshold to probe upwards, and a real session sat pinned at the
/// bitrate floor for its whole lifetime. Anything above this is still congestion;
/// one or two per window is merely not evidence of health.
const DROPS_TOLERATED: u64 = 2;

/// Consecutive bad samples before backing off. At 2 Hz this is one second --
/// long enough to ignore a single hiccup, short enough to fix a real stall.
const BAD_SAMPLES_BEFORE_DECREASE: u32 = 2;
/// Consecutive good samples before probing upwards. Deliberately ~5 seconds.
const GOOD_SAMPLES_BEFORE_INCREASE: u32 = 10;

// The asymmetry between these two is the whole point of AIMD: backing off must be
// quicker than recovering, or the bitrate oscillates visibly. Enforced at compile
// time so a future tuning pass cannot quietly invert it.
const _: () = assert!(BAD_SAMPLES_BEFORE_DECREASE < GOOD_SAMPLES_BEFORE_INCREASE);

/// Multiplicative decrease factor.
const DECREASE_FACTOR: f64 = 0.75;
/// Additive increase step.
const INCREASE_STEP_KBPS: u32 = 1_000;

/// Never change bitrate more often than this, whatever the samples say.
const MIN_CHANGE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub struct AdaptiveController {
    bounds: BitrateBounds,
    current_kbps: u32,
    consecutive_bad: u32,
    consecutive_good: u32,
    last_change: Option<Instant>,
}

impl AdaptiveController {
    pub fn new(bounds: BitrateBounds, start_kbps: u32) -> Self {
        Self {
            bounds,
            current_kbps: start_kbps.clamp(bounds.min_kbps, bounds.max_kbps),
            consecutive_bad: 0,
            consecutive_good: 0,
            last_change: None,
        }
    }

    pub fn current_kbps(&self) -> u32 {
        self.current_kbps
    }

    pub fn bounds(&self) -> BitrateBounds {
        self.bounds
    }

    /// Replaces the bounds, e.g. because the user moved the quality slider.
    pub fn set_bounds(&mut self, bounds: BitrateBounds) {
        self.bounds = bounds;
        self.current_kbps = self.current_kbps.clamp(bounds.min_kbps, bounds.max_kbps);
    }

    /// Feeds in one sample. Returns the new bitrate only when it changed, so the
    /// caller can avoid touching the encoder unnecessarily.
    pub fn observe(&mut self, sample: HealthSample, now: Instant) -> Option<u32> {
        let unhealthy = sample.decode_queue_depth >= QUEUE_DEPTH_BAD
            || sample.dropped_delta > DROPS_TOLERATED
            || sample.rtt >= RTT_BAD;
        let healthy = sample.decode_queue_depth <= QUEUE_DEPTH_GOOD
            && sample.dropped_delta == 0
            && sample.rtt < RTT_BAD;

        if unhealthy {
            self.consecutive_bad += 1;
            self.consecutive_good = 0;
        } else if healthy {
            self.consecutive_good += 1;
            self.consecutive_bad = 0;
        } else {
            // In between: hold steady rather than drifting on ambiguous evidence.
            self.consecutive_bad = 0;
            self.consecutive_good = 0;
        }

        if !self.change_allowed(now) {
            return None;
        }

        if self.consecutive_bad >= BAD_SAMPLES_BEFORE_DECREASE {
            let target = (self.current_kbps as f64 * DECREASE_FACTOR) as u32;
            return self.apply(target, now, &mut Self::reset_bad);
        }

        if self.consecutive_good >= GOOD_SAMPLES_BEFORE_INCREASE {
            let target = self.current_kbps.saturating_add(INCREASE_STEP_KBPS);
            return self.apply(target, now, &mut Self::reset_good);
        }

        None
    }

    fn change_allowed(&self, now: Instant) -> bool {
        self.last_change
            .is_none_or(|t| now.duration_since(t) >= MIN_CHANGE_INTERVAL)
    }

    fn reset_bad(&mut self) {
        self.consecutive_bad = 0;
    }

    fn reset_good(&mut self) {
        self.consecutive_good = 0;
    }

    fn apply(
        &mut self,
        target: u32,
        now: Instant,
        reset: &mut dyn FnMut(&mut Self),
    ) -> Option<u32> {
        let clamped = target.clamp(self.bounds.min_kbps, self.bounds.max_kbps);
        reset(self);
        if clamped == self.current_kbps {
            // Already at the rail; keep the counter reset so we do not spin.
            return None;
        }
        self.current_kbps = clamped;
        self.last_change = Some(now);
        Some(clamped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> HealthSample {
        HealthSample {
            decode_queue_depth: 0,
            dropped_delta: 0,
            rtt: Duration::from_millis(20),
        }
    }

    fn bad() -> HealthSample {
        HealthSample {
            decode_queue_depth: 5,
            dropped_delta: 3,
            rtt: Duration::from_millis(200),
        }
    }

    fn controller() -> AdaptiveController {
        AdaptiveController::new(BitrateBounds::default(), 15_000)
    }

    #[test]
    fn backs_off_after_sustained_trouble() {
        let mut c = controller();
        let t0 = Instant::now();
        // First bad sample alone must not move anything -- one hiccup is not a trend.
        assert_eq!(c.observe(bad(), t0), None);
        let changed = c.observe(bad(), t0 + Duration::from_secs(2));
        assert_eq!(changed, Some(11_250)); // 15000 * 0.75
    }

    #[test]
    fn a_single_bad_sample_is_ignored() {
        let mut c = controller();
        let t0 = Instant::now();
        assert_eq!(c.observe(bad(), t0), None);
        // Recovery resets the streak, so the next bad sample starts from scratch.
        assert_eq!(c.observe(good(), t0 + Duration::from_secs(2)), None);
        assert_eq!(c.observe(bad(), t0 + Duration::from_secs(3)), None);
        assert_eq!(c.current_kbps(), 15_000);
    }

    #[test]
    fn recovers_slowly_and_only_after_many_good_samples() {
        let mut c = AdaptiveController::new(BitrateBounds::default(), 10_000);
        let t0 = Instant::now();
        for i in 0..(GOOD_SAMPLES_BEFORE_INCREASE - 1) {
            assert_eq!(
                c.observe(good(), t0 + Duration::from_secs(2 + i as u64)),
                None,
                "should not raise bitrate after only {} good samples",
                i + 1
            );
        }
        let t = t0 + Duration::from_secs(60);
        assert_eq!(c.observe(good(), t), Some(11_000));
    }

    #[test]
    fn never_leaves_the_configured_bounds() {
        let bounds = BitrateBounds {
            min_kbps: 5_000,
            max_kbps: 20_000,
        };
        let mut c = AdaptiveController::new(bounds, 6_000);
        let mut t = Instant::now();
        // Hammer it with bad samples; it must stop at the floor, not below.
        for _ in 0..200 {
            t += Duration::from_secs(2);
            c.observe(bad(), t);
        }
        assert_eq!(c.current_kbps(), bounds.min_kbps);

        for _ in 0..2000 {
            t += Duration::from_secs(2);
            c.observe(good(), t);
        }
        assert_eq!(c.current_kbps(), bounds.max_kbps);
    }

    #[test]
    fn respects_the_minimum_change_interval() {
        let mut c = controller();
        let t0 = Instant::now();
        c.observe(bad(), t0);
        assert!(c.observe(bad(), t0 + Duration::from_secs(2)).is_some());
        // Immediately after a change, further bad news must not compound.
        c.observe(bad(), t0 + Duration::from_secs(2));
        assert_eq!(c.observe(bad(), t0 + Duration::from_millis(2100)), None);
    }

    #[test]
    fn an_occasional_dropped_frame_is_not_treated_as_congestion() {
        // Regression: with a zero tolerance, a single drop per window kept
        // resetting the good streak, and a real 40s session never rose off the
        // bitrate floor.
        let mut c = AdaptiveController::new(BitrateBounds::default(), 12_000);
        let noisy = HealthSample {
            decode_queue_depth: 0,
            dropped_delta: DROPS_TOLERATED,
            rtt: Duration::from_millis(20),
        };
        let t0 = Instant::now();
        for i in 0..20 {
            let t = t0 + Duration::from_secs(2 + i);
            assert!(
                c.observe(noisy, t).is_none_or(|kbps| kbps >= 12_000),
                "must not back off on tolerated drops"
            );
        }
        assert_eq!(c.current_kbps(), 12_000, "should have held steady");
    }

    #[test]
    fn sustained_heavy_drops_still_back_off() {
        let mut c = AdaptiveController::new(BitrateBounds::default(), 12_000);
        let heavy = HealthSample {
            decode_queue_depth: 0,
            dropped_delta: DROPS_TOLERATED + 1,
            rtt: Duration::from_millis(20),
        };
        let t0 = Instant::now();
        c.observe(heavy, t0);
        assert!(c.observe(heavy, t0 + Duration::from_secs(2)).is_some());
    }

    #[test]
    fn high_rtt_alone_is_enough_to_back_off() {
        let mut c = controller();
        let laggy = HealthSample {
            decode_queue_depth: 0,
            dropped_delta: 0,
            rtt: Duration::from_millis(300),
        };
        let t0 = Instant::now();
        c.observe(laggy, t0);
        assert!(c.observe(laggy, t0 + Duration::from_secs(2)).is_some());
    }

    #[test]
    fn tightening_bounds_pulls_current_bitrate_into_range() {
        let mut c = AdaptiveController::new(BitrateBounds::default(), 25_000);
        c.set_bounds(BitrateBounds {
            min_kbps: 4_000,
            max_kbps: 8_000,
        });
        assert_eq!(c.current_kbps(), 8_000);
    }
}
