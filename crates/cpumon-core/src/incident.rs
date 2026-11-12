//! Detection of high-usage incidents.
//!
//! An incident starts after `sustain` consecutive high samples and ends after
//! `sustain` consecutive normal ones, so short blips neither start nor split incidents.
//! A sample is high when any trigger fires:
//!
//! - **cpu**: total CPU at or above its threshold;
//! - **gpu**: the busiest GPU engine at or above its threshold;
//! - **core**: any single process using at least a given share of one core. This
//!   catches a runaway thread, which barely moves the machine total on a many-core
//!   CPU (one pegged core on a 16-thread machine is about 6%).

use std::fmt;

/// When a sample counts as high.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// Total CPU percent.
    pub cpu: f64,
    /// Busiest GPU engine percent.
    pub gpu: f64,
    /// Percent of one core used by a single process.
    pub proc_core: f64,
    /// Consecutive samples needed to start or end an incident (at least 1).
    pub sustain: u32,
}

/// The measurements the triggers look at.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Observation {
    /// Total CPU percent.
    pub cpu: f64,
    /// Busiest GPU engine percent.
    pub gpu: f64,
    /// Highest percent of one core used by any single process.
    pub max_proc_core: f64,
}

/// Which triggers fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Triggers {
    pub cpu: bool,
    pub gpu: bool,
    pub core: bool,
}

impl Triggers {
    /// Evaluates the triggers for one observation.
    pub fn evaluate(thresholds: &Thresholds, obs: &Observation) -> Self {
        Self {
            cpu: obs.cpu >= thresholds.cpu,
            gpu: obs.gpu >= thresholds.gpu,
            core: obs.max_proc_core >= thresholds.proc_core,
        }
    }

    /// Whether any trigger fired.
    pub fn any(self) -> bool {
        self.cpu || self.gpu || self.core
    }

    /// Triggers that fired in either.
    pub fn union(self, other: Self) -> Self {
        Self {
            cpu: self.cpu || other.cpu,
            gpu: self.gpu || other.gpu,
            core: self.core || other.core,
        }
    }
}

impl fmt::Display for Triggers {
    /// Formats as e.g. `cpu+core`, or `none`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = [(self.cpu, "cpu"), (self.gpu, "gpu"), (self.core, "core")];
        let mut first = true;
        for (_, name) in names.iter().filter(|(fired, _)| *fired) {
            if !first {
                f.write_str("+")?;
            }
            f.write_str(name)?;
            first = false;
        }
        if first {
            f.write_str("none")?;
        }
        Ok(())
    }
}

/// An incident in progress or just ended.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Incident {
    /// Every trigger that fired during the incident.
    pub triggers: Triggers,
    /// Samples recorded, including the trailing normal ones before it ended.
    pub samples: u32,
    pub peak_cpu: f64,
    pub peak_gpu: f64,
    pub peak_proc_core: f64,
}

impl Incident {
    fn record(&mut self, triggers: Triggers, obs: &Observation) {
        self.triggers = self.triggers.union(triggers);
        self.samples += 1;
        self.peak_cpu = self.peak_cpu.max(obs.cpu);
        self.peak_gpu = self.peak_gpu.max(obs.gpu);
        self.peak_proc_core = self.peak_proc_core.max(obs.max_proc_core);
    }
}

/// What a sample did to the incident state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Transition {
    /// No incident.
    Idle,
    /// An incident started with this sample.
    Started,
    /// An incident is in progress; this sample belongs to it.
    Ongoing,
    /// The incident ended with this sample, which belongs to it.
    Ended(Incident),
}

impl Transition {
    /// Whether this sample is part of an incident (and its offenders should be recorded).
    pub fn in_incident(&self) -> bool {
        !matches!(self, Self::Idle)
    }
}

/// Tracks incidents across samples.
#[derive(Debug)]
pub struct IncidentTracker {
    thresholds: Thresholds,
    high_streak: u32,
    normal_streak: u32,
    current: Option<Incident>,
}

impl IncidentTracker {
    pub fn new(thresholds: Thresholds) -> Self {
        Self {
            thresholds: Thresholds {
                sustain: thresholds.sustain.max(1),
                ..thresholds
            },
            high_streak: 0,
            normal_streak: 0,
            current: None,
        }
    }

    pub fn thresholds(&self) -> &Thresholds {
        &self.thresholds
    }

    /// The incident in progress, if any.
    pub fn current(&self) -> Option<&Incident> {
        self.current.as_ref()
    }

    /// Ends the incident in progress, if any (e.g. on shutdown).
    pub fn finish(&mut self) -> Option<Incident> {
        self.high_streak = 0;
        self.normal_streak = 0;
        self.current.take()
    }

    /// Feeds one sample. Returns the transition and the triggers that fired.
    pub fn observe(&mut self, obs: &Observation) -> (Transition, Triggers) {
        let triggers = Triggers::evaluate(&self.thresholds, obs);
        let high = triggers.any();
        let sustain = self.thresholds.sustain;

        let transition = match &mut self.current {
            None => {
                self.high_streak = if high { self.high_streak + 1 } else { 0 };
                if self.high_streak >= sustain {
                    let mut incident = Incident::default();
                    incident.record(triggers, obs);
                    self.current = Some(incident);
                    self.high_streak = 0;
                    self.normal_streak = 0;
                    Transition::Started
                } else {
                    Transition::Idle
                }
            }
            Some(incident) => {
                incident.record(triggers, obs);
                self.normal_streak = if high { 0 } else { self.normal_streak + 1 };
                if self.normal_streak >= sustain {
                    let ended = *incident;
                    self.current = None;
                    self.normal_streak = 0;
                    Transition::Ended(ended)
                } else {
                    Transition::Ongoing
                }
            }
        };
        (transition, triggers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THRESHOLDS: Thresholds = Thresholds {
        cpu: 80.0,
        gpu: 80.0,
        proc_core: 90.0,
        sustain: 2,
    };

    const NORMAL: Observation = Observation {
        cpu: 10.0,
        gpu: 5.0,
        max_proc_core: 20.0,
    };
    const HIGH_CPU: Observation = Observation {
        cpu: 95.0,
        ..NORMAL
    };
    const HIGH_GPU: Observation = Observation {
        gpu: 100.0,
        ..NORMAL
    };
    const PEGGED_CORE: Observation = Observation {
        max_proc_core: 99.5,
        ..NORMAL
    };

    fn run(tracker: &mut IncidentTracker, obs: &[Observation]) -> Vec<Transition> {
        obs.iter().map(|o| tracker.observe(o).0).collect()
    }

    #[test]
    fn evaluates_each_trigger() {
        let t = |o| Triggers::evaluate(&THRESHOLDS, &o);
        assert_eq!(t(NORMAL), Triggers::default());
        assert!(t(HIGH_CPU).cpu && !t(HIGH_CPU).gpu && !t(HIGH_CPU).core);
        assert!(t(HIGH_GPU).gpu && !t(HIGH_GPU).cpu);
        assert!(t(PEGGED_CORE).core && !t(PEGGED_CORE).cpu);
        // Thresholds are inclusive.
        assert!(
            t(Observation {
                cpu: 80.0,
                ..NORMAL
            })
            .cpu
        );
    }

    #[test]
    fn formats_triggers() {
        assert_eq!(Triggers::default().to_string(), "none");
        let t = Triggers {
            cpu: true,
            gpu: false,
            core: true,
        };
        assert_eq!(t.to_string(), "cpu+core");
    }

    #[test]
    fn needs_sustained_high_samples_to_start() {
        let mut tracker = IncidentTracker::new(THRESHOLDS);
        let transitions = run(&mut tracker, &[HIGH_CPU, NORMAL, HIGH_CPU, HIGH_CPU]);
        assert_eq!(
            transitions,
            [
                Transition::Idle,
                Transition::Idle,
                Transition::Idle,
                Transition::Started
            ]
        );
    }

    #[test]
    fn different_triggers_count_towards_the_same_streak() {
        let mut tracker = IncidentTracker::new(THRESHOLDS);
        let transitions = run(&mut tracker, &[HIGH_GPU, PEGGED_CORE]);
        assert_eq!(transitions[1], Transition::Started);
    }

    #[test]
    fn single_pegged_core_starts_an_incident_with_low_totals() {
        let mut tracker = IncidentTracker::new(THRESHOLDS);
        run(&mut tracker, &[PEGGED_CORE, PEGGED_CORE]);
        let incident = tracker.current().expect("incident started");
        assert_eq!(
            incident.triggers,
            Triggers {
                cpu: false,
                gpu: false,
                core: true
            }
        );
        assert_eq!(incident.peak_proc_core, 99.5);
        assert_eq!(incident.peak_cpu, 10.0);
    }

    #[test]
    fn short_dip_does_not_end_incident() {
        let mut tracker = IncidentTracker::new(THRESHOLDS);
        let transitions = run(
            &mut tracker,
            &[HIGH_CPU, HIGH_CPU, NORMAL, HIGH_CPU, NORMAL],
        );
        assert_eq!(
            &transitions[2..],
            [
                Transition::Ongoing,
                Transition::Ongoing,
                Transition::Ongoing
            ]
        );
    }

    #[test]
    fn ends_after_sustained_normal_samples_with_totals() {
        let mut tracker = IncidentTracker::new(THRESHOLDS);
        let transitions = run(
            &mut tracker,
            &[HIGH_CPU, HIGH_CPU, HIGH_GPU, NORMAL, NORMAL],
        );
        let Transition::Ended(incident) = transitions[4] else {
            panic!("expected end, got {transitions:?}");
        };
        // Samples from the start through the final normal one.
        assert_eq!(incident.samples, 4);
        assert_eq!(incident.triggers.to_string(), "cpu+gpu");
        assert_eq!(incident.peak_cpu, 95.0);
        assert_eq!(incident.peak_gpu, 100.0);
        assert!(tracker.current().is_none());
        // Back to idle: a new incident needs a new sustained streak.
        assert_eq!(tracker.observe(&HIGH_CPU).0, Transition::Idle);
    }

    #[test]
    fn sustain_of_one_starts_and_ends_immediately() {
        let mut tracker = IncidentTracker::new(Thresholds {
            sustain: 1,
            ..THRESHOLDS
        });
        let transitions = run(&mut tracker, &[HIGH_CPU, NORMAL]);
        assert_eq!(transitions[0], Transition::Started);
        assert!(matches!(transitions[1], Transition::Ended(i) if i.samples == 2));
    }

    #[test]
    fn sustain_of_zero_is_treated_as_one() {
        let mut tracker = IncidentTracker::new(Thresholds {
            sustain: 0,
            ..THRESHOLDS
        });
        assert_eq!(tracker.observe(&NORMAL).0, Transition::Idle);
        assert_eq!(tracker.observe(&HIGH_CPU).0, Transition::Started);
    }

    #[test]
    fn finish_closes_open_incident() {
        let mut tracker = IncidentTracker::new(THRESHOLDS);
        run(&mut tracker, &[HIGH_CPU, HIGH_CPU]);
        assert_eq!(tracker.finish().map(|i| i.samples), Some(1));
        assert!(tracker.finish().is_none());
    }
}
