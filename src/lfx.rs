use std::ops::{Index, IndexMut};
use std::time::Duration;

const SLOTS: usize = 4;
const PHASES: u64 = 2;
const DOWN_FACTOR: f64 = 0.99;
const UP_FACTOR: f64 = 1.10;

struct EwmaEstimator {
    current: f64,
    current_weight: f64,
    alpha: f64,
}

impl EwmaEstimator {
    fn new(alpha: f64) -> EwmaEstimator {
        EwmaEstimator {
            current: 0.,
            current_weight: 0.,
            alpha,
        }
    }
    fn new_full_weight(alpha: f64) -> EwmaEstimator {
        EwmaEstimator {
            current: 0.,
            current_weight: 1.,
            alpha,
        }
    }

    fn update(&mut self, v: f64) {
        self.current = (1. - self.alpha) * self.current + self.alpha * v;
        self.current_weight = (1. - self.alpha) * self.current_weight + self.alpha;
    }
    fn get(&self) -> f64 {
        if self.current_weight == 0. {
            0.
        } else {
            self.current / self.current_weight
        }
    }
}

#[derive(Copy, Clone, Default, Debug)]
struct RingBuffer<T> {
    data: [T; SLOTS],
}

impl<T> Index<u64> for RingBuffer<T> {
    type Output = T;

    fn index(&self, index: u64) -> &Self::Output {
        return &self.data[index as usize % SLOTS];
    }
}

impl<T> IndexMut<u64> for RingBuffer<T> {
    fn index_mut(&mut self, index: u64) -> &mut Self::Output {
        return &mut self.data[index as usize % SLOTS];
    }
}

pub struct LatencyFleX {
    frame_begin: RingBuffer<Option<(u64, u64)>>,
    comp_applied: RingBuffer<i64>,
    frame_end_projection_base: Option<u64>,
    frame_end_projected_ts: RingBuffer<u64>,
    last_frame_begin_id: Option<u64>,
    last_frame_end: Option<(u64, u64)>,
    latency: EwmaEstimator,
    inv_throughput: EwmaEstimator,
    proj_correction: EwmaEstimator,
    last_prediction_error: i64,
}

impl LatencyFleX {
    pub fn new() -> LatencyFleX {
        LatencyFleX {
            frame_begin: Default::default(),
            comp_applied: Default::default(),
            frame_end_projection_base: None,
            frame_end_projected_ts: Default::default(),
            last_frame_begin_id: None,
            last_frame_end: None,
            latency: EwmaEstimator::new(0.3),
            inv_throughput: EwmaEstimator::new(0.3),
            proj_correction: EwmaEstimator::new_full_weight(0.5),
            last_prediction_error: 0,
        }
    }

    pub fn get_wait_target(&mut self, frame_id: u64) -> u64 {
        if let Some((end_id, end_ts)) = self.last_frame_end {
            let phase = frame_id % PHASES;
            let mut comp_to_apply = 0;
            if self.frame_end_projection_base.is_none() {
                self.frame_end_projection_base = Some(end_ts);
            } else {
                let base = self.frame_end_projection_base.unwrap();
                let prediction_error = end_ts as i64 - (base + self.frame_end_projected_ts[end_id]) as i64;
                let last_comp_applied = self.comp_applied[end_id];
                self.proj_correction.update((prediction_error.max(0) - (self.last_prediction_error - last_comp_applied).max(0)) as f64);
                self.last_prediction_error = prediction_error;
                comp_to_apply = self.proj_correction.get().round() as i64;
                self.comp_applied[frame_id] = comp_to_apply;
            }
            let invtpt = self.inv_throughput.get();
            let lat = self.latency.get();
            let last_begin_id = self.last_frame_begin_id.unwrap();
            let target = (self.frame_end_projection_base.unwrap() + self.frame_end_projected_ts[last_begin_id]) as i64
                + comp_to_apply + (((frame_id - last_begin_id) as f64 + if phase == 0 { 1. / UP_FACTOR } else { 1. } - 1.) * invtpt / DOWN_FACTOR - lat).round() as i64;
            let new_proj = self.frame_end_projected_ts[last_begin_id] as i64
                + comp_to_apply + ((frame_id - last_begin_id) as f64 * invtpt / DOWN_FACTOR).round() as i64;
            self.frame_end_projected_ts[frame_id] = new_proj as _;
            target as _
        } else {
            0
        }
    }

    pub fn begin_frame(&mut self, frame_id: u64, target: u64, ts: u64) {
        self.frame_begin[frame_id] = Some((frame_id, ts));
        self.last_frame_begin_id = Some(frame_id);
        if target != 0 {
            let forced_correction = ts as i64 - target as i64;
            self.frame_end_projected_ts[frame_id] = self.frame_end_projected_ts[frame_id].wrapping_add(forced_correction as _);
            self.comp_applied[frame_id] = self.comp_applied[frame_id].wrapping_add(forced_correction as _);
            self.last_prediction_error += forced_correction;
        }
    }

    pub fn end_frame(&mut self, frame_id: u64, ts: u64) {
        if let Some((id, begin)) = self.frame_begin[frame_id].take() {
            if id != frame_id { return; }
            let phase = frame_id % PHASES;
            let lat = ts as i64 - begin as i64;
            if phase == 1 && lat > 0 {
                self.latency.update(lat as f64);
            }
            if let Some((id, end)) = self.last_frame_end {
                let elapsed = frame_id.checked_sub(id).unwrap();
                let inv_throughput = (ts as i64 - end as i64) / elapsed as i64;
                if phase == 0 && inv_throughput > 0 {
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
