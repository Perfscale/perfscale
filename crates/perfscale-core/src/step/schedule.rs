//! Load-schedule resolution for the native engine: fixed VUs, ramping VUs
//! (`stages:`), and arrival-rate (`arrival:`).
//!
//! Everything here is pure math — no tokio, no I/O — so interpolation,
//! validation, and the arrival-dispatch inversion are unit-testable without a
//! runtime. The async supervisors that *drive* these schedules live in
//! [`crate::step::runner`].

use super::{parse_duration_secs_strict, RunConfig};

/// One piecewise-linear segment: the value ramps linearly from `from` to `to`
/// between the previous segment's `end_secs` (0 for the first) and this
/// segment's `end_secs`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment<T> {
    /// Absolute end of the segment, seconds from run start.
    pub end_secs: f64,
    /// Value at the segment start.
    pub from: T,
    /// Value at the segment end.
    pub to: T,
}

/// The resolved load profile of a run — what a [`RunConfig`] means once
/// `stages:`/`arrival:` (or their absence) is accounted for and validated.
#[derive(Debug, Clone, PartialEq)]
pub enum Schedule {
    /// `vus` workers looping until `duration_secs` elapse — the classic mode.
    Fixed { vus: u32, duration_secs: u64 },
    /// Ramping VUs: a piecewise-linear target-VU curve; the runner's
    /// supervisor spawns/stops VU tasks to track it.
    RampingVus { segments: Vec<Segment<u32>> },
    /// Arrival-rate (open model): a piecewise-linear iterations/sec curve; a
    /// dispatcher hands iteration permits to a worker pool bounded by
    /// `max_vus`.
    ArrivalRate {
        segments: Vec<Segment<f64>>,
        max_vus: u32,
        pre_allocated_vus: u32,
    },
}

impl RunConfig {
    /// Resolve and validate the configured load profile.
    ///
    /// `stages` and `arrival` are mutually exclusive; when neither is set the
    /// fixed `vus`/`duration` profile applies. Errors (also surfaced by
    /// `perfscale lint`): both profiles set at once, empty `arrival.stages`,
    /// unparseable or zero stage durations, `arrival.max_vus` < 1, negative
    /// rates.
    pub fn resolve_schedule(&self) -> Result<Schedule, String> {
        if !self.stages.is_empty() && self.arrival.is_some() {
            return Err(
                "`stages` and `arrival` are mutually exclusive — pick one load profile".into(),
            );
        }
        if !self.stages.is_empty() {
            let mut segments = Vec::with_capacity(self.stages.len());
            let mut end_secs = 0.0;
            // Like k6's ramping-vus with the default startVUs: the run starts
            // at 0 and the first stage ramps up to its target.
            let mut from = 0u32;
            for (i, stage) in self.stages.iter().enumerate() {
                let secs = parse_duration_secs_strict(&stage.duration)
                    .map_err(|e| format!("stages[{i}]: {e}"))?;
                end_secs += secs as f64;
                segments.push(Segment {
                    end_secs,
                    from,
                    to: stage.target,
                });
                from = stage.target;
            }
            return Ok(Schedule::RampingVus { segments });
        }
        if let Some(arrival) = &self.arrival {
            if arrival.max_vus < 1 {
                return Err(
                    "arrival.max_vus must be at least 1 — it caps the worker pool that executes iterations"
                        .into(),
                );
            }
            if arrival.stages.is_empty() {
                return Err("arrival.stages must contain at least one stage".into());
            }
            let mut segments = Vec::with_capacity(arrival.stages.len());
            let mut end_secs = 0.0;
            let mut from = 0.0f64;
            for (i, stage) in arrival.stages.iter().enumerate() {
                let secs = parse_duration_secs_strict(&stage.duration)
                    .map_err(|e| format!("arrival.stages[{i}]: {e}"))?;
                if !stage.rate.is_finite() || stage.rate < 0.0 {
                    return Err(format!(
                        "arrival.stages[{i}].rate must be ≥ 0, got {}",
                        stage.rate
                    ));
                }
                end_secs += secs as f64;
                segments.push(Segment {
                    end_secs,
                    from,
                    to: stage.rate,
                });
                from = stage.rate;
            }
            return Ok(Schedule::ArrivalRate {
                segments,
                max_vus: arrival.max_vus,
                pre_allocated_vus: arrival
                    .pre_allocated_vus
                    .unwrap_or(1)
                    .clamp(1, arrival.max_vus),
            });
        }
        Ok(Schedule::Fixed {
            vus: self.vus.max(1),
            duration_secs: self.duration_secs(),
        })
    }
}

impl Schedule {
    /// Total run length in seconds — for staged profiles the sum of the stage
    /// durations.
    pub fn total_secs(&self) -> f64 {
        match self {
            Schedule::Fixed { duration_secs, .. } => *duration_secs as f64,
            Schedule::RampingVus { segments } => segments.last().map(|s| s.end_secs).unwrap_or(0.0),
            Schedule::ArrivalRate { segments, .. } => {
                segments.last().map(|s| s.end_secs).unwrap_or(0.0)
            }
        }
    }

    /// Target VU count at `elapsed` seconds from run start (piecewise-linear,
    /// rounded to the nearest whole VU). For fixed runs the constant `vus`;
    /// for arrival-rate runs the pool cap (the dispatcher, not a VU target,
    /// drives that profile).
    pub fn target_vus_at(&self, elapsed_secs: f64) -> u32 {
        match self {
            Schedule::Fixed { vus, .. } => *vus,
            Schedule::RampingVus { segments } => lerp_segments(segments, elapsed_secs)
                .round()
                .clamp(0.0, u32::MAX as f64)
                as u32,
            Schedule::ArrivalRate { max_vus, .. } => *max_vus,
        }
    }

    /// Arrival rate (iterations/sec) at `elapsed` seconds — 0 for non-arrival
    /// profiles.
    pub fn rate_at(&self, elapsed_secs: f64) -> f64 {
        match self {
            Schedule::ArrivalRate { segments, .. } => {
                lerp_segments(segments, elapsed_secs).max(0.0)
            }
            _ => 0.0,
        }
    }
}

/// Piecewise-linear interpolation over `segments` at `elapsed` seconds from
/// run start. Before the first segment the value is its `from`; past the last
/// segment's end it stays at the final `to`.
pub fn lerp_segments<T: Into<f64> + Copy>(segments: &[Segment<T>], elapsed_secs: f64) -> f64 {
    let mut start_secs = 0.0;
    for (i, seg) in segments.iter().enumerate() {
        if elapsed_secs < seg.end_secs || i + 1 == segments.len() {
            let span = seg.end_secs - start_secs;
            if span <= 0.0 {
                return seg.to.into();
            }
            let t = ((elapsed_secs - start_secs) / span).clamp(0.0, 1.0);
            let (a, b) = (seg.from.into(), seg.to.into());
            return a + (b - a) * t;
        }
        start_secs = seg.end_secs;
    }
    0.0
}

/// Incremental arrival-dispatch cursor: yields the absolute offsets (seconds
/// from run start) at which iterations should start, one at a time — no
/// precomputed schedule table.
///
/// Within a segment (local time `t`, rate ramping `r0 → r1` over `d` seconds,
/// `k = (r1 − r0)/d`) the cumulative iteration count is
/// `C(t) = r0·t + k·t²/2`; the n-th dispatch sits at `C⁻¹(n)` — the quadratic
/// root, or a plain division when the rate is constant (`k ≈ 0`).
#[derive(Debug, Default, Clone)]
pub struct DispatchCursor {
    segment: usize,
    done_in_segment: u64,
}

impl DispatchCursor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Offset of the next dispatch from run start, or `None` once every
    /// segment's iteration budget is exhausted. A segment delivers
    /// `floor`-ish `C(span)` dispatches — fractional remainders do not roll
    /// over into the next segment.
    pub fn next(&mut self, segments: &[Segment<f64>]) -> Option<f64> {
        loop {
            let seg = segments.get(self.segment)?;
            let start_secs = if self.segment == 0 {
                0.0
            } else {
                segments[self.segment - 1].end_secs
            };
            let span = seg.end_secs - start_secs;
            if span <= 0.0 {
                self.segment += 1;
                self.done_in_segment = 0;
                continue;
            }
            let r0 = seg.from;
            let k = (seg.to - seg.from) / span;
            // Total iterations this segment delivers: C(span) = (r0 + r1)/2 · d.
            let budget = r0 * span + k * span * span / 2.0;
            let n = self.done_in_segment + 1;
            if n as f64 > budget + 1e-9 {
                self.segment += 1;
                self.done_in_segment = 0;
                continue;
            }
            // Invert C(t) = r0·t + k·t²/2 = n. `k ≈ 0` implies r0 > 0 here,
            // because a zero-rate segment has budget 0 and was skipped above.
            let t = if k.abs() < 1e-12 {
                n as f64 / r0
            } else {
                let disc = (r0 * r0 + 2.0 * k * n as f64).max(0.0);
                (-r0 + disc.sqrt()) / k
            };
            self.done_in_segment = n;
            return Some(start_secs + t.clamp(0.0, span));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step::{ArrivalConfig, RateStage, VuStage};

    fn staged_config(stages: &[(&str, u32)]) -> RunConfig {
        RunConfig {
            stages: stages
                .iter()
                .map(|(d, t)| VuStage {
                    duration: d.to_string(),
                    target: *t,
                })
                .collect(),
            ..Default::default()
        }
    }

    fn arrival_config(max_vus: u32, stages: &[(&str, f64)]) -> RunConfig {
        RunConfig {
            arrival: Some(Box::new(ArrivalConfig {
                max_vus,
                pre_allocated_vus: None,
                stages: stages
                    .iter()
                    .map(|(d, r)| RateStage {
                        duration: d.to_string(),
                        rate: *r,
                    })
                    .collect(),
            })),
            ..Default::default()
        }
    }

    // -----------------------------------------------------------------
    // Resolution & validation
    // -----------------------------------------------------------------

    #[test]
    fn fixed_is_the_default_profile() {
        let cfg = RunConfig {
            vus: 3,
            duration: "30s".into(),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_schedule().unwrap(),
            Schedule::Fixed {
                vus: 3,
                duration_secs: 30
            }
        );
    }

    #[test]
    fn stages_resolve_to_ramping_segments() {
        let cfg = staged_config(&[("10s", 10), ("20s", 10), ("10s", 0)]);
        let schedule = cfg.resolve_schedule().unwrap();
        assert_eq!(
            schedule,
            Schedule::RampingVus {
                segments: vec![
                    Segment {
                        end_secs: 10.0,
                        from: 0,
                        to: 10
                    },
                    Segment {
                        end_secs: 30.0,
                        from: 10,
                        to: 10
                    },
                    Segment {
                        end_secs: 40.0,
                        from: 10,
                        to: 0
                    },
                ]
            }
        );
        assert_eq!(schedule.total_secs(), 40.0);
    }

    #[test]
    fn stages_and_arrival_together_is_an_error() {
        let mut cfg = staged_config(&[("10s", 5)]);
        cfg.arrival = Some(Box::new(ArrivalConfig {
            max_vus: 5,
            pre_allocated_vus: None,
            stages: vec![RateStage {
                duration: "10s".into(),
                rate: 1.0,
            }],
        }));
        let err = cfg.resolve_schedule().unwrap_err();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn stage_with_zero_or_garbage_duration_is_an_error() {
        for bad in ["0s", "soon", ""] {
            let cfg = staged_config(&[(bad, 5)]);
            let err = cfg.resolve_schedule().unwrap_err();
            assert!(err.contains("stages[0]"), "{err}");
        }
    }

    #[test]
    fn arrival_requires_max_vus_and_stages() {
        let err = arrival_config(0, &[("10s", 5.0)])
            .resolve_schedule()
            .unwrap_err();
        assert!(err.contains("max_vus"), "{err}");

        let err = arrival_config(5, &[]).resolve_schedule().unwrap_err();
        assert!(err.contains("at least one stage"), "{err}");
    }

    #[test]
    fn arrival_rejects_negative_and_nan_rates() {
        for bad in [-1.0, f64::NAN, f64::INFINITY] {
            let err = arrival_config(5, &[("10s", bad)])
                .resolve_schedule()
                .unwrap_err();
            assert!(err.contains("rate"), "{err}");
        }
    }

    #[test]
    fn arrival_resolves_with_defaults() {
        let schedule = arrival_config(10, &[("30s", 5.0)])
            .resolve_schedule()
            .unwrap();
        match schedule {
            Schedule::ArrivalRate {
                segments,
                max_vus,
                pre_allocated_vus,
            } => {
                assert_eq!(max_vus, 10);
                assert_eq!(pre_allocated_vus, 1, "default is one pre-allocated VU");
                assert_eq!(segments.len(), 1);
                assert_eq!(segments[0].to, 5.0);
            }
            other => panic!("expected arrival schedule, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Interpolation
    // -----------------------------------------------------------------

    #[test]
    fn target_vus_interpolates_piecewise_linearly() {
        // ramp 0→10 over 10s, hold 10 for 20s, ramp down to 0 over 10s.
        let schedule = staged_config(&[("10s", 10), ("20s", 10), ("10s", 0)])
            .resolve_schedule()
            .unwrap();
        assert_eq!(schedule.target_vus_at(0.0), 0, "ramp starts at 0");
        assert_eq!(schedule.target_vus_at(5.0), 5, "mid-segment");
        assert_eq!(schedule.target_vus_at(10.0), 10, "segment boundary");
        assert_eq!(schedule.target_vus_at(20.0), 10, "hold");
        assert_eq!(schedule.target_vus_at(35.0), 5, "ramp-down mid-segment");
        assert_eq!(schedule.target_vus_at(40.0), 0, "ramp-down to zero");
        assert_eq!(
            schedule.target_vus_at(41.0),
            0,
            "past the end stays at last target"
        );
    }

    #[test]
    fn target_vus_rounds_to_nearest() {
        // 0→1 over 10s: crosses 0.5 at t=5 → rounds up from there.
        let schedule = staged_config(&[("10s", 1)]).resolve_schedule().unwrap();
        assert_eq!(schedule.target_vus_at(4.9), 0);
        assert_eq!(schedule.target_vus_at(5.1), 1);
    }

    #[test]
    fn rate_at_interpolates_arrival_segments() {
        let schedule = arrival_config(10, &[("10s", 10.0), ("10s", 20.0)])
            .resolve_schedule()
            .unwrap();
        assert_eq!(schedule.rate_at(0.0), 0.0);
        assert_eq!(schedule.rate_at(5.0), 5.0);
        assert_eq!(schedule.rate_at(10.0), 10.0, "segment boundary");
        assert_eq!(schedule.rate_at(15.0), 15.0);
        assert_eq!(schedule.rate_at(25.0), 20.0, "past the end holds");
    }

    #[test]
    fn fractional_rates_interpolate() {
        let schedule = arrival_config(2, &[("10s", 0.5)])
            .resolve_schedule()
            .unwrap();
        assert_eq!(schedule.rate_at(10.0), 0.5);
    }

    // -----------------------------------------------------------------
    // Arrival dispatch (rate-integral inversion)
    // -----------------------------------------------------------------

    fn dispatch_all(segments: &[Segment<f64>]) -> Vec<f64> {
        let mut cursor = DispatchCursor::new();
        let mut out = Vec::new();
        while let Some(t) = cursor.next(segments) {
            out.push(t);
        }
        out
    }

    #[test]
    fn constant_rate_dispatches_at_even_intervals() {
        // Constant 5 it/s over 2s → 10 dispatches, 200ms apart.
        let segments = [Segment {
            end_secs: 2.0,
            from: 5.0,
            to: 5.0,
        }];
        let times = dispatch_all(&segments);
        assert_eq!(times.len(), 10, "{times:?}");
        for (i, t) in times.iter().enumerate() {
            let expected = 0.2 * (i + 1) as f64;
            assert!(
                (t - expected).abs() < 1e-9,
                "dispatch {i}: {t} != {expected}"
            );
        }
    }

    #[test]
    fn ramping_rate_integrates_to_total_iterations() {
        // Ramp 0→10 it/s over 10s → ∫ = ½·10·10 = 50 iterations, the n-th at
        // t = √(2n) seconds.
        let segments = [Segment {
            end_secs: 10.0,
            from: 0.0,
            to: 10.0,
        }];
        let times = dispatch_all(&segments);
        assert_eq!(times.len(), 50, "{times:?}");
        assert!((times[0] - 2.0_f64.sqrt()).abs() < 1e-9);
        assert!((times[49] - 10.0).abs() < 1e-9);
        // Dispatches get denser as the rate ramps up.
        assert!(times[40] - times[39] < times[1] - times[0]);
    }

    #[test]
    fn ramping_down_still_dispatches_full_budget() {
        // Ramp 10→0 over 10s → also 50 iterations.
        let segments = [Segment {
            end_secs: 10.0,
            from: 10.0,
            to: 0.0,
        }];
        assert_eq!(dispatch_all(&segments).len(), 50);
    }

    #[test]
    fn zero_rate_segment_dispatches_nothing() {
        let segments = [
            Segment {
                end_secs: 5.0,
                from: 0.0,
                to: 0.0,
            },
            Segment {
                end_secs: 6.0,
                from: 1.0,
                to: 1.0,
            },
        ];
        let times = dispatch_all(&segments);
        assert_eq!(times.len(), 1);
        assert!((times[0] - 6.0).abs() < 1e-9, "lands in the second segment");
    }

    #[test]
    fn fractional_budget_is_not_carried_across_segments() {
        // 0.4 iterations per segment — never enough for a dispatch.
        let segments = [
            Segment {
                end_secs: 1.0,
                from: 0.4,
                to: 0.4,
            },
            Segment {
                end_secs: 2.0,
                from: 0.4,
                to: 0.4,
            },
        ];
        assert!(dispatch_all(&segments).is_empty());
    }

    #[test]
    fn dispatch_offsets_are_absolute_across_segments() {
        // 1 it/s for 1s, then 2 it/s for 1s: dispatches at 1.0, 1.5, 2.0.
        let segments = [
            Segment {
                end_secs: 1.0,
                from: 1.0,
                to: 1.0,
            },
            Segment {
                end_secs: 2.0,
                from: 2.0,
                to: 2.0,
            },
        ];
        let times = dispatch_all(&segments);
        assert_eq!(times.len(), 3, "{times:?}");
        assert!((times[0] - 1.0).abs() < 1e-9);
        assert!((times[1] - 1.5).abs() < 1e-9);
        assert!((times[2] - 2.0).abs() < 1e-9);
    }

    #[test]
    fn total_secs_sums_stage_durations() {
        let cfg = staged_config(&[("30s", 5), ("1m", 10), ("30s", 0)]);
        assert_eq!(cfg.resolve_schedule().unwrap().total_secs(), 120.0);
        let cfg = arrival_config(10, &[("30s", 5.0), ("1m30s", 20.0)]);
        assert_eq!(cfg.resolve_schedule().unwrap().total_secs(), 120.0);
    }
}
