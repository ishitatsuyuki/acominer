use std::time::Duration;

// const ALPHA: f64 = 0.2;
// ALPHA.ln()
const LOG_ALPHA: f64 = -1.6094379124341003;
// (1. - ALPHA).ln()
const LOG_ONE_MINUS_ALPHA: f64 = -0.2231435513142097;
const EXPONENT: f64 = -8.0;
const CYCLE: &[f64] = &[1.1, 1.];
const SLOTS: usize = 2;

struct EwmaEstimator {
    current: f64,
    current_weight: f64,
}

fn exp_add(a: f64, b: f64) -> f64 {
    let max = a.max(b);
    let min = a.min(b);
    max + (1. + (min - max).exp()).ln()
}

impl EwmaEstimator {
    fn update(&mut self, v: f64) {
        let t = v.ln() * EXPONENT;
        if t.is_nan() { return; }
        self.current = exp_add(LOG_ONE_MINUS_ALPHA + self.current, LOG_ALPHA + t);
        self.current_weight = exp_add(LOG_ONE_MINUS_ALPHA + self.current_weight, LOG_ALPHA);
    }
    fn get(&self) -> f64 {
        if self.current_weight == f64::NEG_INFINITY {
            0.
        } else {
            ((self.current - self.current_weight) / EXPONENT).exp()
        }
    }
}

impl Default for EwmaEstimator {
    fn default() -> Self {
        EwmaEstimator {
            current: f64::NEG_INFINITY,
            current_weight: f64::NEG_INFINITY,
        }
    }
}

#[derive(Default)]
pub struct LatencyFleX {
    frame_begin: [Option<(u64, u64)>; SLOTS],
    last_frame_end: Option<(u64, u64)>,
    latency: EwmaEstimator,
    inv_throughput: EwmaEstimator,
    phase: usize,
}

impl LatencyFleX {
    pub fn get_wait_target(&mut self, frame_id: u64) -> u64 {
        if let Some((id, ts)) = self.last_frame_end {
            let gain = CYCLE[self.phase];
            self.phase = (self.phase + 1) % CYCLE.len();
            let invtpt = self.inv_throughput.get() / 0.98;
            let lat = self.latency.get();
            let target = ts + (((frame_id - id - 1) as f64 + gain.recip()) * invtpt - lat).round() as u64;
            target
        } else {
            0
        }
    }

    pub fn begin_frame(&mut self, frame_id: u64, ts: u64) {
        self.frame_begin[frame_id as usize % SLOTS] = Some((frame_id, ts));
    }

    pub fn end_frame(&mut self, frame_id: u64, ts: u64) {
        if let Some((id, begin)) = self.frame_begin[frame_id as usize % SLOTS].take() {
            if id != frame_id { return; }
            let lat = ts as i64 - begin as i64;
            if lat > 0 {
                self.latency.update(lat as f64);
            }
            if let Some((id, end)) = self.last_frame_end {
                let elapsed = frame_id.checked_sub(id).unwrap();
                let inv_throughput = (ts as i64 - end as i64) / elapsed as i64;
                if inv_throughput > 0 {
                    self.inv_throughput.update(inv_throughput as f64);
                }
            }
            self.last_frame_end = Some((frame_id, ts));
        }
    }

    pub fn latency(&self) -> Duration {
        Duration::from_nanos(self.latency.get() as u64)
    }

    pub fn inv_throughput(&self) -> Duration {
        Duration::from_nanos(self.inv_throughput.get() as u64)
    }
}
