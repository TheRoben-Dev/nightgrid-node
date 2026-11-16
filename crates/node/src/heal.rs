//! Self-healing (N4a): rules the app pushes, evaluated on the node.
//!
//! A rule names a condition (an unhealthy or stopped container, a full
//! disk), how long it must hold, and a typed action. Rules are off and
//! dry-run by default, and every run goes through [`crate::actions::run`],
//! so root's `[actions] allow`, the global rate limit and the audit log
//! apply to them exactly as to the app's own requests.
//!
//! Per rule and target, a run waits `forMinutes` after the condition
//! started and again after each run (to give the fix time to work), is
//! capped per hour and per day, and after `giveUpAfter` runs without the
//! condition clearing the rule stops and raises a warning until it clears.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use nightgrid_node_proto::{
    action, ActionOutcome, ActionRequest, AlertKind, ContainerInfo, EventKind, HealRule, HealRules, HealStatus,
    HealStatusResult, HealWhen, Level,
};

use crate::daemon::{now_secs, Shared};
use crate::events::NewEvent;
use crate::store::Store;

pub const META_RULES: &str = "heal_rules";
const CHANGES_TICK: Duration = Duration::from_secs(5);
const DAY: i64 = 86_400;

/// A condition that holds now: what kind, and on which container or mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cond {
    pub when: HealWhen,
    pub target: String,
}

#[derive(Debug, Clone, Default)]
struct Track {
    since: i64,
    attempts: u32,
    last_run: Option<i64>,
    gave_up: bool,
}

/// What the evaluator decided for one rule and target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Run { rule: String, target: String, request: ActionRequest },
    GiveUp { rule: String, target: String, attempts: u32 },
}

#[derive(Debug, Default)]
pub struct Heal {
    rules: Vec<HealRule>,
    track: BTreeMap<(String, String), Track>,
    /// Run times per (rule, target) over the last day; survives the
    /// condition clearing so a flapping container still hits the caps.
    runs: BTreeMap<(String, String), Vec<i64>>,
}

impl Heal {
    pub fn load(store: &Store) -> Self {
        let rules: HealRules =
            store.meta(META_RULES).ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default();
        Self { rules: rules.rules, ..Default::default() }
    }

    pub fn rules(&self) -> HealRules {
        HealRules { rules: self.rules.clone() }
    }

    /// Replaces the rules (already validated). What the old rules were
    /// tracking starts over; the per-day run counts stay.
    pub fn set_rules(&mut self, rules: HealRules) {
        self.rules = rules.rules;
        self.track.clear();
    }

    pub fn status(&self) -> HealStatusResult {
        HealStatusResult {
            enabled: self.rules.iter().filter(|r| r.enabled).count() as u32,
            tracking: self
                .track
                .iter()
                .map(|((rule, target), t)| HealStatus {
                    rule: rule.clone(),
                    target: target.clone(),
                    since: t.since,
                    attempts: t.attempts,
                    last_run: t.last_run,
                    gave_up: t.gave_up,
                })
                .collect(),
        }
    }

    /// One evaluation: which runs are due given the conditions that hold now.
    pub fn step(&mut self, now: i64, conds: &[Cond]) -> Vec<Plan> {
        let mut plans = Vec::new();
        let mut live = Vec::new();
        for rule in self.rules.iter().filter(|r| r.enabled) {
            for c in conds {
                if c.when != rule.when || (rule.target != "*" && rule.target != c.target) {
                    continue;
                }
                let key = (rule.id.clone(), c.target.clone());
                live.push(key.clone());
                let t = self.track.entry(key.clone()).or_insert_with(|| Track { since: now, ..Default::default() });
                let wait = i64::from(rule.for_minutes) * 60;
                if t.gave_up || now - t.since < wait || t.last_run.is_some_and(|l| now - l < wait) {
                    continue;
                }
                if t.attempts >= rule.give_up_after {
                    t.gave_up = true;
                    plans.push(Plan::GiveUp { rule: rule.id.clone(), target: c.target.clone(), attempts: t.attempts });
                    continue;
                }
                let runs = self.runs.entry(key).or_default();
                runs.retain(|&r| now - r < DAY);
                let hour = runs.iter().filter(|&&r| now - r < 3600).count() as u32;
                if hour >= rule.max_per_hour || runs.len() as u32 >= rule.max_per_day {
                    continue;
                }
                runs.push(now);
                t.attempts += 1;
                t.last_run = Some(now);
                let target = action::takes_container(&rule.action).then(|| c.target.clone());
                plans.push(Plan::Run {
                    rule: rule.id.clone(),
                    target: c.target.clone(),
                    request: ActionRequest { action: rule.action.clone(), target, dry_run: rule.dry_run },
                });
            }
        }
        // A condition that cleared resets its rule's attempts.
        self.track.retain(|k, _| live.contains(k));
        self.runs.retain(|_, v| v.last().is_some_and(|&r| now - r < DAY));
        plans
    }
}

/// The conditions that hold now, from the alert engine and the container list.
pub fn conditions(active: &[nightgrid_node_proto::Alert], containers: &[ContainerInfo]) -> Vec<Cond> {
    let mut out = Vec::new();
    for c in containers {
        if c.state == "running" && c.health.as_deref() == Some("unhealthy") {
            out.push(Cond { when: HealWhen::ContainerUnhealthy, target: c.name.clone() });
        }
    }
    for a in active.iter().filter(|a| a.resolved.is_none()) {
        if let Some(name) = a.key.strip_prefix("docker:") {
            // The same key covers "unhealthy"; down means it isn't running
            // (and still exists, or there is nothing to start).
            let down = containers.iter().any(|c| c.name == name && c.state != "running");
            if a.kind == AlertKind::ContainerDown && down {
                out.push(Cond { when: HealWhen::ContainerDown, target: name.to_owned() });
            }
        } else if let Some(mount) = a.key.strip_prefix("disk:") {
            if a.kind == AlertKind::DiskFull {
                out.push(Cond { when: HealWhen::DiskFull, target: mount.to_owned() });
            }
        }
    }
    out
}

fn tick(shared: &Shared) {
    let active = shared.active.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let containers = shared.containers.lock().unwrap_or_else(|e| e.into_inner()).result.containers.clone();
    let conds = conditions(&active, &containers);
    let plans = shared.heal.lock().unwrap_or_else(|e| e.into_inner()).step(now_secs(), &conds);
    for p in plans {
        match p {
            Plan::Run { rule, target, request } => {
                let rec = crate::actions::run(shared, &request, &format!("rule {rule}"));
                if rec.outcome == ActionOutcome::Refused {
                    crate::debug!("rule {rule} on {target}: {}", rec.detail);
                }
            }
            Plan::GiveUp { rule, target, attempts } => {
                crate::warn!("self-healing rule {rule} gave up on {target} after {attempts} runs");
                let e = NewEvent::new(
                    now_secs(),
                    EventKind::Action,
                    Level::Warn,
                    format!("self-healing gave up on {target}"),
                    format!(
                        "rule {rule} ran {attempts} times and the condition still holds; it won't run again until \
                         the condition clears. See actions.log."
                    ),
                );
                let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
                if let Err(err) = shared.bus.record(&st, &e) {
                    crate::warn!("could not record a heal event: {err}");
                }
            }
        }
    }
}

/// Handle to the self-healing thread.
pub struct Healer {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Healer {
    pub fn start(shared: Arc<Shared>) -> std::io::Result<Self> {
        let (stop, rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new().name("heal".into()).spawn(move || {
            let mut n = 0u32;
            while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(CHANGES_TICK) {
                // The rollback timer every 5 s, the rules every 15 s.
                crate::rollback::tick(&shared);
                n = n.wrapping_add(1);
                if n % 3 == 0 {
                    tick(&shared);
                }
            }
        })?;
        Ok(Self { stop, thread: Some(thread) })
    }

    pub fn stop(mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str) -> HealRule {
        HealRule { id: id.into(), enabled: true, dry_run: false, ..Default::default() }
    }

    fn unhealthy(name: &str) -> Cond {
        Cond { when: HealWhen::ContainerUnhealthy, target: name.into() }
    }

    fn runs(p: &[Plan]) -> Vec<(&str, &str)> {
        p.iter()
            .filter_map(|p| match p {
                Plan::Run { rule, target, .. } => Some((rule.as_str(), target.as_str())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn waits_then_runs_backs_off_and_gives_up() {
        let mut h = Heal::default();
        h.set_rules(HealRules { rules: vec![HealRule { give_up_after: 2, ..rule("r") }] });
        let c = [unhealthy("web")];
        assert!(h.step(0, &c).is_empty(), "condition must hold forMinutes first");
        assert!(h.step(299, &c).is_empty());
        let p = h.step(300, &c);
        assert_eq!(runs(&p), [("r", "web")]);
        let Plan::Run { request, .. } = &p[0] else { panic!() };
        assert_eq!(request.target.as_deref(), Some("web"));
        assert!(!request.dry_run);
        assert!(h.step(400, &c).is_empty(), "waits again after a run");
        assert_eq!(runs(&h.step(600, &c)), [("r", "web")]);
        let p = h.step(900, &c);
        assert_eq!(p, [Plan::GiveUp { rule: "r".into(), target: "web".into(), attempts: 2 }]);
        assert!(h.step(1200, &c).is_empty());
        assert!(h.status().tracking[0].gave_up);
        // Clearing resets: it can run again after forMinutes.
        assert!(h.step(1300, &[]).is_empty());
        assert!(h.status().tracking.is_empty());
        h.step(1310, &c);
        assert_eq!(runs(&h.step(1610, &c)), [("r", "web")]);
    }

    #[test]
    fn caps_per_hour_survive_flapping() {
        let mut h = Heal::default();
        h.set_rules(HealRules { rules: vec![HealRule { for_minutes: 1, max_per_hour: 2, ..rule("r") }] });
        let c = [unhealthy("web")];
        let mut total = 0;
        let mut t = 0;
        for _ in 0..10 {
            h.step(t, &c);
            total += runs(&h.step(t + 60, &c)).len();
            h.step(t + 70, &[]); // clears, then comes back
            t += 120;
        }
        assert_eq!(total, 2);
    }

    #[test]
    fn matches_kind_and_target_and_skips_disabled() {
        let mut h = Heal::default();
        h.set_rules(HealRules {
            rules: vec![
                HealRule { target: "db".into(), for_minutes: 1, ..rule("only-db") },
                HealRule { enabled: false, for_minutes: 1, ..rule("off") },
                HealRule {
                    when: HealWhen::DiskFull,
                    action: action::IMAGES_PRUNE.into(),
                    for_minutes: 1,
                    dry_run: true,
                    ..rule("disk")
                },
            ],
        });
        let c = [unhealthy("web"), unhealthy("db"), Cond { when: HealWhen::DiskFull, target: "/".into() }];
        h.step(0, &c);
        let p = h.step(60, &c);
        assert_eq!(runs(&p), [("only-db", "db"), ("disk", "/")]);
        let Plan::Run { request, .. } = &p[1] else { panic!() };
        assert!(request.target.is_none() && request.dry_run);
        assert_eq!(h.status().enabled, 2);
    }

    #[test]
    fn conditions_from_alerts_and_containers() {
        use nightgrid_node_proto::{Alert, Severity};
        let ci = |name: &str, state: &str, health: Option<&str>| ContainerInfo {
            id: "x".into(),
            name: name.into(),
            image: "i".into(),
            state: state.into(),
            status: String::new(),
            health: health.map(Into::into),
            project: None,
        };
        let al = |key: &str, kind| Alert {
            id: 1,
            key: key.into(),
            kind,
            severity: Severity::Warn,
            title: String::new(),
            detail: String::new(),
            since: 0,
            resolved: None,
        };
        let cs = [ci("web", "running", Some("unhealthy")), ci("db", "exited", None), ci("ok", "running", None)];
        let alerts = [
            al("docker:web", AlertKind::ContainerDown),
            al("docker:db", AlertKind::ContainerDown),
            al("docker:gone", AlertKind::ContainerDown),
            al("disk:/srv", AlertKind::DiskFull),
        ];
        let c = conditions(&alerts, &cs);
        assert_eq!(
            c,
            [
                unhealthy("web"),
                Cond { when: HealWhen::ContainerDown, target: "db".into() },
                Cond { when: HealWhen::DiskFull, target: "/srv".into() },
            ]
        );
    }

    #[test]
    fn rules_persist() {
        let store = Store::open_in_memory().unwrap();
        let rules = HealRules { rules: vec![rule("r")] };
        store.set_meta(META_RULES, &serde_json::to_string(&rules).unwrap()).unwrap();
        assert_eq!(Heal::load(&store).rules(), rules);
    }
}
