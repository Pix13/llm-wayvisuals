//! Rolling series of the selected model's `decode_tps`, for the waveform.

use crate::model::ModelSelect;
use crate::source::Feed;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub struct History {
    window: Duration,
    points: VecDeque<(Instant, Option<f64>)>,
    last_generation: Option<u64>,
}

impl History {
    pub fn new(window: Duration) -> Self {
        History {
            window,
            points: VecDeque::new(),
            last_generation: None,
        }
    }

    /// `None` = gap (no signal / no value).
    pub fn push(&mut self, at: Instant, value: Option<f64>) {
        let value = value.filter(|v| v.is_finite());
        self.points.push_back((at, value));
        self.trim(at);
    }

    fn trim(&mut self, now: Instant) {
        while let Some(&(t, _)) = self.points.front() {
            if now.saturating_duration_since(t) > self.window {
                self.points.pop_front();
            } else {
                break;
            }
        }
    }

    /// `(age, value)` oldest first; age is 0 (now) ..= 1 (window edge).
    pub fn samples(&self, now: Instant) -> Vec<(f32, Option<f32>)> {
        let w = self.window.as_secs_f32().max(f32::EPSILON);
        self.points
            .iter()
            .filter_map(|&(t, v)| {
                let age = now.saturating_duration_since(t).as_secs_f32() / w;
                (age <= 1.0).then_some((age, v.map(|v| v as f32)))
            })
            .collect()
    }

    /// Maximum value currently in the window.
    pub fn max(&self) -> Option<f64> {
        self.points
            .iter()
            .filter_map(|&(_, v)| v)
            .fold(None, |m, v| Some(m.map_or(v, |m: f64| m.max(v))))
    }

    /// Push the selected model's `decode_tps` once per new snapshot
    /// generation; push a gap while the feed is not live.
    pub fn observe(&mut self, feed: &Feed, sel: &ModelSelect, now: Instant) {
        if !feed.is_live() {
            if self.points.back().is_none_or(|&(_, v)| v.is_some()) {
                self.push(now, None);
            } else {
                self.trim(now);
            }
            return;
        }
        if self.last_generation == Some(feed.generation) {
            self.trim(now);
            return;
        }
        self.last_generation = Some(feed.generation);
        let v = feed
            .snapshot
            .as_ref()
            .and_then(|s| s.select(sel))
            .and_then(|m| m.decode_tps);
        self.push(now, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Snapshot;
    use crate::source::LinkStatus;
    use std::sync::Arc;

    fn feed(gen: u64, tps: Option<f64>, status: LinkStatus) -> Feed {
        let snap = Snapshot {
            models: vec![crate::model::Model {
                decode_tps: tps,
                ..Default::default()
            }],
            ..Default::default()
        };
        Feed {
            snapshot: Some(Arc::new(snap)),
            received: Some(Instant::now()),
            status,
            generation: gen,
        }
    }

    #[test]
    fn window_trimming_and_ages() {
        let t0 = Instant::now();
        let mut h = History::new(Duration::from_secs(10));
        for i in 0..=20u64 {
            h.push(t0 + Duration::from_secs(i), Some(i as f64));
        }
        let now = t0 + Duration::from_secs(20);
        let s = h.samples(now);
        assert_eq!(s.len(), 11); // t = 10..=20
        assert_eq!(s.first().unwrap().1, Some(10.0));
        assert!((s.first().unwrap().0 - 1.0).abs() < 1e-6);
        assert_eq!(s.last().unwrap(), &(0.0, Some(20.0)));
        assert_eq!(h.max(), Some(20.0));
        // later, old points age out of samples()
        let later = now + Duration::from_secs(5);
        assert_eq!(h.samples(later).len(), 6);
    }

    #[test]
    fn gaps_and_max() {
        let t0 = Instant::now();
        let mut h = History::new(Duration::from_secs(60));
        assert_eq!(h.max(), None);
        h.push(t0, Some(5.0));
        h.push(t0 + Duration::from_secs(1), None);
        h.push(t0 + Duration::from_secs(2), Some(f64::NAN));
        h.push(t0 + Duration::from_secs(3), Some(3.0));
        let s = h.samples(t0 + Duration::from_secs(3));
        assert_eq!(s.iter().map(|x| x.1).collect::<Vec<_>>(), vec![Some(5.0), None, None, Some(3.0)]);
        assert_eq!(h.max(), Some(5.0));
    }

    #[test]
    fn observe_once_per_generation() {
        let t0 = Instant::now();
        let mut h = History::new(Duration::from_secs(60));
        let sel = ModelSelect::Auto;
        let f1 = feed(1, Some(10.0), LinkStatus::Live);
        h.observe(&f1, &sel, t0);
        h.observe(&f1, &sel, t0 + Duration::from_millis(30));
        assert_eq!(h.samples(t0).len(), 1);
        h.observe(&feed(2, Some(20.0), LinkStatus::Live), &sel, t0 + Duration::from_secs(1));
        assert_eq!(h.samples(t0 + Duration::from_secs(1)).len(), 2);
        assert_eq!(h.max(), Some(20.0));
        // not live: a single gap, repeated calls do not pile up
        let stale = feed(2, Some(20.0), LinkStatus::Stale);
        h.observe(&stale, &sel, t0 + Duration::from_secs(4));
        h.observe(&stale, &sel, t0 + Duration::from_secs(5));
        let s = h.samples(t0 + Duration::from_secs(5));
        assert_eq!(s.len(), 3);
        assert_eq!(s[2].1, None);
        // missing value / unselected model -> gap
        h.observe(&feed(3, None, LinkStatus::Live), &sel, t0 + Duration::from_secs(6));
        assert_eq!(h.samples(t0 + Duration::from_secs(6)).len(), 4);
        h.observe(&feed(4, Some(1.0), LinkStatus::Live), &ModelSelect::Pid(99), t0 + Duration::from_secs(7));
        assert_eq!(h.samples(t0 + Duration::from_secs(7)).last().unwrap().1, None);
    }
}
