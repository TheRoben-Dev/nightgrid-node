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
    HealStatusResult, Level,
};

use crate::fixes::{AutoRule, When};

use crate::daemon::{now_secs, Shared};
use crate::events::NewEvent;
use crate::store::Store;

pub const META_RULES: &str = "heal_rules";
/// Root's automation rules (`fixes.toml`) are tracked under this prefix, so
/// they never collide with the app's rule ids or show in `heal.status`.
pub const LOCAL_PREFIX: &str = "local:";
const CHANGES_TICK: Duration = Duration::from_secs(5);
const DAY: i64 = 86_400;

/// A condition that holds now: what kind, and on which container or mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cond {
    pub when: When,
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
    /// Root's automation rules from `fixes.toml`, ids already prefixed.
    local: Vec<AutoRule>,
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
        self.track.retain(|(id, _), _| id.starts_with(LOCAL_PREFIX));
    }

    /// Replaces root's automation rules (already validated).
    pub fn set_local(&mut self, rules: Vec<AutoRule>) {
        self.local = rules.into_iter().map(|r| AutoRule { id: format!("{LOCAL_PREFIX}{}", r.id), ..r }).collect();
        self.track.retain(|(id, _), _| !id.starts_with(LOCAL_PREFIX));
    }

    /// Whether an enabled automation rule waits for this condition.
    pub fn wants(&self, when: When) -> bool {
        self.local.iter().any(|r| r.enabled && r.when == when)
    }

    /// Unit targets of enabled unit-failed rules (`*` means every listed unit).
    pub fn unit_targets(&self) -> Vec<String> {
        self.local.iter().filter(|r| r.enabled && r.when == When::UnitFailed).map(|r| r.target.clone()).collect()
    }

    pub fn status(&self) -> HealStatusResult {
        HealStatusResult {
            enabled: self.rules.iter().filter(|r| r.enabled).count() as u32,
            tracking: self
                .track
                .iter()
                .filter(|((rule, _), _)| !rule.starts_with(LOCAL_PREFIX))
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
        let all: Vec<AutoRule> = self.rules.iter().map(AutoRule::from_heal).chain(self.local.iter().cloned()).collect();
        for rule in all.iter().filter(|r| r.enabled) {
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
                let target = (action::takes_container(&rule.action) || crate::fixes::takes_unit(&rule.action))
                    .then(|| c.target.clone());
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

/// The conditions that hold now, from the alert engine, the container list
/// and the failed units among the ones root listed.
pub fn conditions(
    active: &[nightgrid_node_proto::Alert],
    containers: &[ContainerInfo],
    failed: &[String],
) -> Vec<Cond> {
    let mut out = Vec::new();
    for c in containers {
        if c.state == "running" && c.health.as_deref() == Some("unhealthy") {
            out.push(Cond { when: When::ContainerUnhealthy, target: c.name.clone() });
        }
    }
    for u in failed {
        out.push(Cond { when: When::UnitFailed, target: u.clone() });
    }
    for a in active.iter().filter(|a| a.resolved.is_none()) {
        if let Some(name) = a.key.strip_prefix("docker:") {
            // The same key covers "unhealthy"; down means it isn't running
            // (and still exists, or there is nothing to start).
            let down = containers.iter().any(|c| c.name == name && c.state != "running");
            if a.kind == AlertKind::ContainerDown && down {
                out.push(Cond { when: When::ContainerDown, target: name.to_owned() });
            }
        } else if let Some(mount) = a.key.strip_prefix("disk:") {
            if a.kind == AlertKind::DiskFull {
                out.push(Cond { when: When::DiskFull, target: mount.to_owned() });
            }
        }
    }
    out
}

/// Picks up changes to root's `fixes.toml` (allow list, automation rules).
pub fn refresh_fixes(shared: &Shared) {
    let rules = shared.fixes.lock().unwrap_or_else(|e| e.into_inner()).refresh(&shared.fixes_path);
    if let Some(rules) = rules {
        shared.heal.lock().unwrap_or_else(|e| e.into_inner()).set_local(rules);
    }
}

/// Windows: the counterpart of systemd's failed state, which the SCM
/// doesn't keep. A listed service is failed when it is stopped and either
/// stopped with an error (an exit code other than 0, or 1077 "never
/// started") or is set to start with Windows. A crashed svchost leaves
/// exit code 0, so an automatic service that isn't running counts too.
#[cfg(windows)]
fn failed_units(units: &[String]) -> Vec<String> {
    use crate::win::scm;
    units
        .iter()
        .filter(|u| match scm::status(u) {
            Ok(Some(st)) if st.state == 1 => {
                !matches!(st.exit, 0 | 1077) || scm::auto_start(u).ok().flatten().unwrap_or(false)
            }
            _ => false,
        })
        .cloned()
        .collect()
}

/// Which of the units are in the failed state (`systemctl is-failed`
/// prints one line per unit, in order, whatever its exit status).
#[cfg(unix)]
fn failed_units(units: &[String]) -> Vec<String> {
    if units.is_empty() {
        return Vec::new();
    }
    let mut cmd = std::process::Command::new("systemctl");
    cmd.arg("is-failed").arg("--").args(units).env("SYSTEMD_PAGER", "");
    match cmd.output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .lines()
            .zip(units)
            .filter(|(state, _)| state.trim() == "failed")
            .map(|(_, u)| u.clone())
            .collect(),
        Err(e) => {
            crate::debug!("systemctl is-failed: {e}");
            Vec::new()
        }
    }
}

fn tick(shared: &Shared) {
    refresh_fixes(shared);
    let active = shared.active.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let containers = shared.containers.lock().unwrap_or_else(|e| e.into_inner()).result.containers.clone();
    let targets = shared.heal.lock().unwrap_or_else(|e| e.into_inner()).unit_targets();
    let failed = if targets.is_empty() {
        Vec::new()
    } else {
        let local = shared.fixes.lock().unwrap_or_else(|e| e.into_inner()).local.clone();
        let listed = crate::fixes::units(&shared.config, &local);
        // Only units root listed: `*` means all of them, a name only if listed.
        let units: Vec<String> = listed.into_iter().filter(|u| targets.iter().any(|t| t == "*" || t == u)).collect();
        failed_units(&units)
    };
    let conds = conditions(&active, &containers, &failed);
    let plans = shared.heal.lock().unwrap_or_else(|e| e.into_inner()).step(now_secs(), &conds);
    for p in plans {
        match p {
            Plan::Run { rule, target, request } => {
                let source = match rule.strip_prefix(LOCAL_PREFIX) {
                    Some(id) => format!("autofix {id}"),
                    None => format!("rule {rule}"),
                };
                let rec = crate::actions::run(shared, &request, &source);
                if rec.outcome == ActionOutcome::Refused {
                    crate::debug!("rule {rule} on {target}: {}", rec.detail);
                }
            }
            Plan::GiveUp { rule, target, attempts } => {
                let rule = rule.strip_prefix(LOCAL_PREFIX).map_or(rule.clone(), |id| format!("autofix {id}"));
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
                if n.is_multiple_of(3) {
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
    use nightgrid_node_proto::HealWhen;

    #[test]
    fn local_rules_run_beside_the_apps_and_stay_out_of_its_status() {
        let mut h = Heal::default();
        h.set_rules(HealRules { rules: vec![HealRule { for_minutes: 1, ..rule("r") }] });
        h.set_local(vec![AutoRule {
            id: "r".into(),
            enabled: true,
            dry_run: false,
            when: When::UnitFailed,
            for_minutes: 1,
            ..Default::default()
        }]);
        assert!(h.wants(When::UnitFailed) && !h.wants(When::DiskFull));
        assert_eq!(h.unit_targets(), ["*"]);
        let c = [unhealthy("web"), Cond { when: When::UnitFailed, target: "nginx.service".into() }];
        h.step(0, &c);
        let p = h.step(60, &c);
        assert_eq!(runs(&p), [("r", "web"), ("local:r", "nginx.service")]);
        let Plan::Run { request, .. } = &p[1] else { panic!() };
        assert_eq!((request.action.as_str(), request.target.as_deref()), ("unit.restart", Some("nginx.service")));
        assert_eq!(h.status().tracking.len(), 1, "the app sees only its own rules");
        assert_eq!(h.status().enabled, 1);
        // Replacing the app's rules keeps tracking root's, and the other way round.
        h.set_rules(HealRules::default());
        assert_eq!(h.track.len(), 1);
        h.set_local(Vec::new());
        assert!(h.track.is_empty());
        assert!(
            conditions(&[], &[], &["a.service".into()])
                == [Cond { when: When::UnitFailed, target: "a.service".into() }]
        );
    }

    fn rule(id: &str) -> HealRule {
        HealRule { id: id.into(), enabled: true, dry_run: false, ..Default::default() }
    }

    fn unhealthy(name: &str) -> Cond {
        Cond { when: When::ContainerUnhealthy, target: name.into() }
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
        let c = [unhealthy("web"), unhealthy("db"), Cond { when: When::DiskFull, target: "/".into() }];
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
            ports: vec![],
            compose_files: vec![],
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
        let c = conditions(&alerts, &cs, &[]);
        assert_eq!(
            c,
            [
                unhealthy("web"),
                Cond { when: When::ContainerDown, target: "db".into() },
                Cond { when: When::DiskFull, target: "/srv".into() },
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
