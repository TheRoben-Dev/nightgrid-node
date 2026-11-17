//! UPS shutdown guard (N7c): this host powers itself off when its UPS has
//! been on battery too long, with the app closed.
//!
//! There is no node-to-node link: every host with a node follows the UPS
//! reading it already takes (`[health] ups`, through the health helper) and
//! decides alone, with root's limits. A "group" shutdown is the hosts on one
//! UPS each with their own limits, so the ones that must stay up longest
//! (storage, the NUT server) get larger ones. The power-off is the ordinary
//! `power.poweroff` action through the root helper, so it needs root to
//! allow it, and it is in the action log. A countdown is announced first;
//! power coming back or `ups.cancel` stops it, and a cancelled shutdown stays
//! off for the rest of that outage, like the app's group shutdown (D-112).
//!
//! With the node-to-node link (N8c), a host can also follow a paired peer's
//! guard: when that peer starts its countdown, this host starts its own
//! (with its own `countdown_secs`), and the peer's power coming back or a
//! cancel there ends it. That needs root's `[peers] serve` to list "ups"
//! here, and the app to mark the peer `followUps`. Hosts that can't read
//! the UPS themselves (no NUT client) can follow the one that can.

use nightgrid_node_proto::{action, ActionRequest, EventKind, Level, UpsGuard, UpsNotice, UpsPhase};

use crate::config::Config;
use crate::daemon::Shared;
use crate::events::NewEvent;
use crate::health::ups::{words, UpsReading};

/// What a step asks the caller to do.
#[derive(Debug, Clone)]
pub enum Step {
    Event(NewEvent),
    PowerOff,
}

/// The guard's state; settings come from the config each step.
#[derive(Debug, Default, Clone)]
pub struct Guard {
    pub phase: UpsPhase,
    pub since: Option<i64>,
    pub deadline: Option<i64>,
    pub reason: Option<String>,
    /// The peer whose shutdown this one follows.
    pub led_by: Option<String>,
}

impl Guard {
    /// Whether readings should come often (on battery or counting down).
    pub fn active(&self) -> bool {
        matches!(self.phase, UpsPhase::OnBattery | UpsPhase::Countdown)
    }

    /// Takes a reading (or, with `None`, just the clock).
    pub fn step(&mut self, cfg: &Config, r: Option<&UpsReading>, now: i64) -> Vec<Step> {
        let mut out = Vec::new();
        let u = &cfg.ups;
        if !u.shutdown {
            *self = Guard::default();
            return out;
        }
        let target = u.target(&cfg.health).unwrap_or("UPS").to_owned();
        match r {
            // No answer (the NUT server may have lost power too): keep going.
            Some(r) if r.error.is_some() => {}
            None => {}
            // Following a peer: its notices decide, not this host's reading.
            Some(r) if !r.on_battery && self.led_by.is_some() => {}
            Some(r) if !r.on_battery => {
                if self.phase == UpsPhase::Countdown {
                    out.push(Step::Event(NewEvent::new(
                        now,
                        EventKind::Other,
                        Level::Ok,
                        "UPS shutdown cancelled: power is back",
                        format!("{target} is on line power again"),
                    )));
                }
                if self.phase != UpsPhase::ShuttingDown || u.dry_run {
                    *self = Guard::default();
                }
            }
            Some(r) => {
                let since = *self.since.get_or_insert(now);
                if self.phase == UpsPhase::Idle {
                    self.phase = UpsPhase::OnBattery;
                }
                if self.phase == UpsPhase::OnBattery {
                    if let Some(why) = limit(cfg, r, now - since) {
                        self.reason = Some(why.clone());
                        self.phase = UpsPhase::Countdown;
                        self.deadline = Some(now + i64::from(u.countdown_secs));
                        out.push(Step::Event(NewEvent::new(
                            now,
                            EventKind::Other,
                            Level::Critical,
                            if u.countdown_secs == 0 {
                                "UPS shutdown now".to_owned()
                            } else {
                                format!("UPS shutdown in {}", words(u.countdown_secs))
                            },
                            format!(
                                "{target}: {why}. This host powers off{} unless power returns or it is cancelled in the app.",
                                if u.dry_run { " (dry run: only logged)" } else { "" }
                            ),
                        )));
                    }
                }
            }
        }
        if self.phase == UpsPhase::Countdown && self.deadline.is_some_and(|d| now >= d) {
            self.phase = UpsPhase::ShuttingDown;
            out.push(Step::PowerOff);
        }
        out
    }

    /// A notice from a paired peer this host follows (`name`).
    pub fn peer(&mut self, cfg: &Config, name: &str, n: &UpsNotice, now: i64) -> Vec<Step> {
        let mut out = Vec::new();
        let u = &cfg.ups;
        if !u.shutdown {
            return out;
        }
        let ours = self.led_by.as_deref() == Some(name);
        match n.phase {
            UpsPhase::Countdown | UpsPhase::ShuttingDown => {
                if !matches!(self.phase, UpsPhase::Idle | UpsPhase::OnBattery) {
                    return out;
                }
                let why = format!(
                    "{name} is shutting down for {}{}",
                    n.target.as_deref().unwrap_or("its UPS"),
                    n.reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default()
                );
                self.since.get_or_insert(now);
                self.phase = UpsPhase::Countdown;
                self.deadline = Some(now + i64::from(u.countdown_secs));
                self.reason = Some(why.clone());
                self.led_by = Some(name.to_owned());
                out.push(Step::Event(NewEvent::new(
                    now,
                    EventKind::Other,
                    Level::Critical,
                    if u.countdown_secs == 0 {
                        "UPS shutdown now".to_owned()
                    } else {
                        format!("UPS shutdown in {}", words(u.countdown_secs))
                    },
                    format!(
                        "{why}. This host follows it and powers off{} unless power returns or it is cancelled in the app.",
                        if u.dry_run { " (dry run: only logged)" } else { "" }
                    ),
                )));
                if self.deadline.is_some_and(|d| now >= d) {
                    self.phase = UpsPhase::ShuttingDown;
                    out.push(Step::PowerOff);
                }
            }
            UpsPhase::Idle | UpsPhase::Cancelled if ours => {
                let back = n.phase == UpsPhase::Idle;
                if self.phase == UpsPhase::Countdown {
                    out.push(Step::Event(NewEvent::new(
                        now,
                        EventKind::Other,
                        if back { Level::Ok } else { Level::Warn },
                        if back { "UPS shutdown cancelled: power is back" } else { "UPS shutdown cancelled" },
                        if back {
                            format!("{name} is on line power again")
                        } else {
                            format!("{name} cancelled its shutdown, so this host stays on too")
                        },
                    )));
                }
                if self.phase != UpsPhase::ShuttingDown || u.dry_run {
                    *self = Guard::default();
                }
            }
            _ => {}
        }
        out
    }

    /// `ups.cancel`: stops a countdown (or the watch) for this outage.
    pub fn cancel(&mut self, now: i64) -> Option<NewEvent> {
        if !matches!(self.phase, UpsPhase::OnBattery | UpsPhase::Countdown) {
            return None;
        }
        let was = self.phase;
        self.phase = UpsPhase::Cancelled;
        self.deadline = None;
        Some(NewEvent::new(
            now,
            EventKind::Other,
            Level::Warn,
            "UPS shutdown cancelled",
            if was == UpsPhase::Countdown {
                "cancelled from the app; this host stays on for the rest of this outage"
            } else {
                "turned off from the app for the rest of this outage"
            },
        ))
    }

    pub fn status(&self, cfg: &Config) -> UpsGuard {
        let u = &cfg.ups;
        UpsGuard {
            enabled: u.shutdown,
            dry_run: u.dry_run,
            target: u.target(&cfg.health).map(str::to_owned),
            on_battery_minutes: u.on_battery_minutes,
            below_percent: u.below_percent,
            low_battery: u.low_battery,
            countdown_secs: u.countdown_secs,
            phase: self.phase,
            since: self.since,
            deadline: self.deadline,
            reason: self.reason.clone(),
            problem: problem(cfg),
            led_by: self.led_by.clone(),
        }
    }
}

/// The first of root's limits that is passed.
pub fn limit(cfg: &Config, r: &UpsReading, on_battery_secs: i64) -> Option<String> {
    let u = &cfg.ups;
    if u.low_battery && r.low_battery {
        return Some("the UPS reports low battery".into());
    }
    if let (true, Some(c)) = (u.below_percent > 0, r.charge) {
        if c < f32::from(u.below_percent) {
            return Some(format!("battery at {c:.0}%"));
        }
    }
    if u.on_battery_minutes > 0 && on_battery_secs >= i64::from(u.on_battery_minutes) * 60 {
        return Some(format!("on battery for {}", words(on_battery_secs.max(0) as u32)));
    }
    None
}

/// Why the guard couldn't power the host off, if it can't.
pub fn problem(cfg: &Config) -> Option<String> {
    if !cfg.ups.shutdown {
        return None;
    }
    if !cfg.health.enabled {
        return Some("[health] enabled is off".into());
    }
    if cfg.ups.target(&cfg.health).is_none() && !cfg.peers.serve.iter().any(|s| s == "ups") {
        return Some("no UPS to follow: list it in [health] ups, or follow a peer's (\"ups\" in [peers] serve)".into());
    }
    if !cfg.actions.allow.iter().any(|a| a == action::POWER_POWEROFF) {
        return Some(format!("{} isn't in [actions] allow", action::POWER_POWEROFF));
    }
    None
}

/// Runs the steps: records events, and requests the power-off through the
/// ordinary action path (checks, helper, action log).
pub fn apply(shared: &Shared, steps: Vec<Step>) {
    for s in steps {
        match s {
            Step::Event(e) => {
                let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
                if let Err(err) = shared.bus.record(&st, &e) {
                    crate::warn!("could not record a UPS step: {err}");
                }
            }
            Step::PowerOff => {
                let req = ActionRequest {
                    action: action::POWER_POWEROFF.into(),
                    target: None,
                    dry_run: shared.config.ups.dry_run,
                };
                let rec = crate::actions::run(shared, &req, "ups");
                crate::warn!("UPS shutdown: {:?} {}", rec.outcome, rec.detail);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        let mut c = Config::default();
        c.health.ups = vec!["ups@nas".into()];
        c.ups.shutdown = true;
        c.ups.on_battery_minutes = 5;
        c.ups.countdown_secs = 60;
        c.actions.allow = vec![action::POWER_POWEROFF.into()];
        c
    }

    fn reading(s: &str) -> UpsReading {
        crate::health::ups::parse(s)
    }

    fn titles(v: &[Step]) -> Vec<String> {
        v.iter()
            .map(|s| match s {
                Step::Event(e) => e.title.clone(),
                Step::PowerOff => "POWEROFF".into(),
            })
            .collect()
    }

    #[test]
    fn counts_down_then_powers_off() {
        let c = cfg();
        let ob = reading("ups.status: OB DISCHRG\nbattery.charge: 80\n");
        let mut g = Guard::default();
        assert!(g.step(&c, Some(&ob), 1000).is_empty());
        assert_eq!(g.phase, UpsPhase::OnBattery);
        assert!(g.step(&c, Some(&ob), 1200).is_empty());
        let s = g.step(&c, Some(&ob), 1300);
        assert_eq!(titles(&s), ["UPS shutdown in 60 s"]);
        assert_eq!(g.reason.as_deref(), Some("on battery for 5 min"));
        assert!(g.step(&c, None, 1340).is_empty());
        assert_eq!(titles(&g.step(&c, Some(&ob), 1360)), ["POWEROFF"]);
        assert_eq!(g.phase, UpsPhase::ShuttingDown);
        assert!(g.step(&c, Some(&ob), 1370).is_empty(), "only once");
    }

    #[test]
    fn power_back_and_cancel() {
        let c = cfg();
        let ob = reading("ups.status: OB LB\nbattery.charge: 20\n");
        let ol = reading("ups.status: OL CHRG\n");
        let mut g = Guard::default();
        assert_eq!(titles(&g.step(&c, Some(&ob), 0)), ["UPS shutdown in 60 s"]);
        assert_eq!(g.reason.as_deref(), Some("the UPS reports low battery"));
        assert_eq!(titles(&g.step(&c, Some(&ol), 10)), ["UPS shutdown cancelled: power is back"]);
        assert_eq!(g.phase, UpsPhase::Idle);
        g.step(&c, Some(&ob), 20);
        assert!(g.cancel(30).is_some());
        assert!(g.step(&c, Some(&ob), 500).is_empty(), "cancelled for this outage");
        assert_eq!(g.phase, UpsPhase::Cancelled);
        g.step(&c, Some(&ol), 600);
        assert_eq!(g.phase, UpsPhase::Idle);
        assert!(g.cancel(601).is_none());
    }

    #[test]
    fn charge_limit_and_errors() {
        let mut c = cfg();
        c.ups.on_battery_minutes = 0;
        c.ups.below_percent = 50;
        c.ups.countdown_secs = 0;
        let mut g = Guard::default();
        assert!(g.step(&c, Some(&reading("ups.status: OB\nbattery.charge: 60\n")), 0).is_empty());
        assert!(g.step(&c, Some(&reading("Error: Connection failure")), 10).is_empty());
        assert_eq!(g.phase, UpsPhase::OnBattery, "an unreadable UPS changes nothing");
        let s = g.step(&c, Some(&reading("ups.status: OB\nbattery.charge: 45\n")), 20);
        assert_eq!(titles(&s), ["UPS shutdown now", "POWEROFF"]);
    }

    fn notice(phase: UpsPhase) -> UpsNotice {
        UpsNotice { phase, reason: Some("on battery for 5 min".into()), target: Some("ups@localhost".into()) }
    }

    #[test]
    fn follows_a_peer() {
        let mut c = cfg();
        // This host reads no UPS itself.
        c.health.ups.clear();
        c.peers.serve = vec!["ping".into(), "ups".into()];
        assert!(problem(&c).is_none(), "{:?}", problem(&c));
        let mut g = Guard::default();
        let s = g.peer(&c, "nas", &notice(UpsPhase::Countdown), 100);
        assert_eq!(titles(&s), ["UPS shutdown in 60 s"]);
        assert_eq!(g.led_by.as_deref(), Some("nas"));
        assert!(g.reason.as_deref().unwrap().starts_with("nas is shutting down for ups@localhost"));
        assert!(g.peer(&c, "nas", &notice(UpsPhase::ShuttingDown), 120).is_empty(), "already counting down");
        // Line power here doesn't end it: the peer decides.
        assert!(g.step(&c, Some(&reading("ups.status: OL\n")), 130).is_empty());
        assert_eq!(g.phase, UpsPhase::Countdown);
        // Another peer's "power back" doesn't either.
        assert!(g.peer(&c, "pi", &notice(UpsPhase::Idle), 140).is_empty());
        assert_eq!(titles(&g.peer(&c, "nas", &notice(UpsPhase::Idle), 150)), ["UPS shutdown cancelled: power is back"]);
        assert_eq!(g.phase, UpsPhase::Idle);
        assert!(g.led_by.is_none());
        // Again, and this time the clock runs out with no word from the peer.
        g.peer(&c, "nas", &notice(UpsPhase::Countdown), 200);
        assert_eq!(titles(&g.step(&c, None, 261)), ["POWEROFF"]);
        // The peer cancelling ends a countdown too.
        let mut g = Guard::default();
        g.peer(&c, "nas", &notice(UpsPhase::Countdown), 0);
        assert_eq!(titles(&g.peer(&c, "nas", &notice(UpsPhase::Cancelled), 5)), ["UPS shutdown cancelled"]);
        // Off here: nothing.
        c.ups.shutdown = false;
        let mut g = Guard::default();
        assert!(g.peer(&c, "nas", &notice(UpsPhase::Countdown), 0).is_empty());
        assert_eq!(g.phase, UpsPhase::Idle);
    }

    #[test]
    fn off_and_problems() {
        let mut c = cfg();
        c.ups.shutdown = false;
        let mut g = Guard::default();
        assert!(g.step(&c, Some(&reading("ups.status: OB LB\n")), 0).is_empty());
        assert_eq!(g.phase, UpsPhase::Idle);
        c.ups.shutdown = true;
        c.actions.allow.clear();
        assert!(problem(&c).unwrap().contains("[actions] allow"));
        c.health.ups.clear();
        assert!(problem(&c).unwrap().contains("[health] ups"));
        assert!(problem(&cfg()).is_none());
    }
}
