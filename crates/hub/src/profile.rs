//! `profile` 1 (0.45.0): the Nightgrid app's profile, held for apps that are
//! never open at the same time. The app seals every item before it leaves;
//! the hub keeps opaque records keyed by `(profileId, tbl, uid)`, applies
//! last write wins on `modified` so a replayed old push changes nothing, and
//! hands them out by a per-profile `seq`, like `events.sync`.

use std::collections::HashMap;
use std::sync::Mutex;

use nightgrid_node_proto::hub::{
    ProfileId, ProfileInfo, ProfileList, ProfilePull, ProfilePullResult, ProfilePush, ProfilePushResult,
};
use nightgrid_node_proto::ErrorCode;

use crate::fleet::Hub;
use crate::store::PushError;

/// Most records in one push.
pub const MAX_RECORDS: usize = 1000;
/// Largest sealed item, in characters of base64.
pub const MAX_BLOB: usize = 1024 * 1024;
/// Default and largest page for `profile.pull`.
pub const PULL_DEFAULT: usize = 500;
pub const PULL_MAX: usize = 2000;
/// Blobs per pull answer stay well under the socket's 4 MiB line.
pub const PULL_BYTES: usize = 3 * 1024 * 1024;

type Error = (ErrorCode, String);

fn bad(m: impl Into<String>) -> Error {
    (ErrorCode::BadRequest, m.into())
}

fn sql(e: impl std::fmt::Display) -> Error {
    (ErrorCode::Internal, e.to_string())
}

/// Pushes counted per profile in the current minute.
#[derive(Default)]
pub struct RateLimit(Mutex<HashMap<String, (i64, u32)>>);

impl RateLimit {
    fn allow(&self, id: &str, now: i64, per_minute: u32) -> bool {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let minute = now / 60;
        m.retain(|_, (at, _)| *at == minute);
        let e = m.entry(id.to_owned()).or_insert((minute, 0));
        if e.1 >= per_minute {
            return false;
        }
        e.1 += 1;
        true
    }
}

fn token(what: &str, s: &str, min: usize, max: usize, extra: &str) -> Result<(), Error> {
    if s.len() < min || s.len() > max {
        return Err(bad(format!("{what} must be {min}-{max} characters")));
    }
    if !s.bytes().all(|b| b.is_ascii_alphanumeric() || extra.as_bytes().contains(&b)) {
        return Err(bad(format!("{what} may hold only letters, digits and {extra}")));
    }
    Ok(())
}

pub fn check_id(id: &str) -> Result<(), Error> {
    token("profileId", id, 8, 64, "_-")
}

fn enabled(hub: &Hub) -> Result<(), Error> {
    if hub.config.profile.enabled {
        Ok(())
    } else {
        Err((ErrorCode::CapabilityDisabled, "profile is off on this hub ([profile] enabled)".into()))
    }
}

fn max_bytes(hub: &Hub) -> u64 {
    u64::from(hub.config.profile.max_mb) * 1024 * 1024
}

pub fn list(hub: &Hub) -> Result<ProfileList, Error> {
    enabled(hub)?;
    let st = hub.store();
    let mut profiles = Vec::new();
    for id in st.profile_ids().map_err(sql)? {
        if let Some(i) = st.profile_info(&id, max_bytes(hub)).map_err(sql)? {
            profiles.push(i);
        }
    }
    Ok(ProfileList { profiles })
}

/// An unknown profile answers as an empty one, so a fresh app can ask
/// without a special case.
pub fn info(hub: &Hub, p: &ProfileId) -> Result<ProfileInfo, Error> {
    enabled(hub)?;
    check_id(&p.profile_id)?;
    let max = max_bytes(hub);
    let i = hub.store().profile_info(&p.profile_id, max).map_err(sql)?;
    Ok(i.unwrap_or(ProfileInfo {
        profile_id: p.profile_id.clone(),
        items: 0,
        tombstones: 0,
        bytes: 0,
        max_bytes: max,
        seq: 0,
        last_push: None,
        last_device: None,
        pruned_through: 0,
    }))
}

pub fn push(hub: &Hub, p: &ProfilePush, now: i64) -> Result<ProfilePushResult, Error> {
    enabled(hub)?;
    check_id(&p.profile_id)?;
    if p.records.len() > MAX_RECORDS {
        return Err(bad(format!("at most {MAX_RECORDS} records per push")));
    }
    for r in &p.records {
        token("tbl", &r.tbl, 1, 64, "_-.:+/=")?;
        token("uid", &r.uid, 1, 128, "_-.:+/=")?;
        if r.modified < 0 {
            return Err(bad("modified must be unix milliseconds"));
        }
        match &r.blob {
            Some(b) => token("blob", b, 1, MAX_BLOB, "+/=_-")?,
            None if !r.deleted => return Err(bad("an item that isn't deleted needs its sealed blob")),
            None => {}
        }
    }
    if !hub.profile_rate.allow(&p.profile_id, now, hub.config.profile.pushes_per_minute) {
        return Err(bad(format!(
            "rate limited: at most {} pushes per minute for one profile",
            hub.config.profile.pushes_per_minute
        )));
    }
    let device: Option<String> = p.device.as_ref().map(|d| d.chars().filter(|c| !c.is_control()).take(80).collect());
    let max = max_bytes(hub);
    let r = hub.store().profile_push(
        &p.profile_id,
        device.as_deref(),
        &p.records,
        now,
        max,
        hub.config.profile.max_profiles as usize,
    );
    r.map_err(|e| match e {
        PushError::TooLarge(b) => (
            ErrorCode::TooLarge,
            format!("the profile would hold {b} bytes, over this hub's cap of {max} ([profile] max_mb)"),
        ),
        PushError::TooMany => (
            ErrorCode::TooLarge,
            format!("this hub already holds {} profiles ([profile] max_profiles)", hub.config.profile.max_profiles),
        ),
        PushError::Sql(e) => sql(e),
    })
}

pub fn pull(hub: &Hub, p: &ProfilePull) -> Result<ProfilePullResult, Error> {
    enabled(hub)?;
    check_id(&p.profile_id)?;
    let limit = p.limit.map_or(PULL_DEFAULT, |l| (l as usize).clamp(1, PULL_MAX));
    let after = p.after.max(0);
    let (records, more, pruned) = hub.store().profile_pull(&p.profile_id, after, limit, PULL_BYTES).map_err(sql)?;
    let next = records.last().map_or(after, |r| r.seq);
    Ok(ProfilePullResult { records, next, more, resync: after > 0 && after < pruned })
}

pub fn drop(hub: &Hub, p: &ProfileId) -> Result<bool, Error> {
    enabled(hub)?;
    check_id(&p.profile_id)?;
    hub.store().profile_drop(&p.profile_id).map_err(sql)
}

/// Hourly: tombstones past `[profile] tombstone_days`.
pub fn prune(hub: &Hub, now: i64) -> Result<usize, String> {
    let cutoff_ms = (now - i64::from(hub.config.profile.tombstone_days) * 86400) * 1000;
    hub.store().profile_prune(cutoff_ms).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::store::Store;
    use nightgrid_node_proto::hub::ProfileRecord;

    const ID: &str = "p-0123456789abcdef";

    fn hub() -> Hub {
        Hub::new(Config::default(), Store::open_in_memory().unwrap())
    }

    fn rec(uid: &str, modified: i64, blob: &str) -> ProfileRecord {
        ProfileRecord { tbl: "hosts".into(), uid: uid.into(), modified, deleted: false, blob: Some(blob.into()) }
    }

    fn push_(h: &Hub, records: Vec<ProfileRecord>) -> Result<ProfilePushResult, Error> {
        push(h, &ProfilePush { profile_id: ID.into(), device: Some("laptop".into()), records }, 1000)
    }

    fn pull_(h: &Hub, after: i64) -> ProfilePullResult {
        pull(h, &ProfilePull { profile_id: ID.into(), after, limit: None }).unwrap()
    }

    #[test]
    fn last_write_wins_and_replays_change_nothing() {
        let h = hub();
        let r = push_(&h, vec![rec("a", 10, "QUFB"), rec("b", 10, "QkJC")]).unwrap();
        assert_eq!((r.accepted, r.ignored, r.seq), (2, 0, 2));
        // An older edit and an identical replay are both ignored.
        let r = push_(&h, vec![rec("a", 9, "T0xE"), rec("b", 10, "QkJC")]).unwrap();
        assert_eq!((r.accepted, r.ignored, r.seq), (0, 2, 2));
        // A newer edit wins and moves to the end of the feed.
        let r = push_(&h, vec![rec("a", 11, "TkVX")]).unwrap();
        assert_eq!((r.accepted, r.seq), (1, 3));
        let p = pull_(&h, 0);
        let got: Vec<_> = p.records.iter().map(|r| (r.seq, r.record.uid.as_str(), r.record.modified)).collect();
        assert_eq!(got, [(2, "b", 10), (3, "a", 11)]);
        assert_eq!((p.next, p.more, p.resync), (3, false, false));
        assert!(pull_(&h, 3).records.is_empty());
        // Same time: a tombstone beats the item, so every copy agrees.
        let tomb = ProfileRecord { tbl: "hosts".into(), uid: "a".into(), modified: 11, deleted: true, blob: None };
        assert_eq!(push_(&h, vec![tomb]).unwrap().accepted, 1);
        let i = info(&h, &ProfileId { profile_id: ID.into() }).unwrap();
        assert_eq!((i.items, i.tombstones, i.seq, i.last_device.as_deref()), (1, 1, 4, Some("laptop")));
        assert_eq!(i.last_push, Some(1000));
    }

    #[test]
    fn caps_validation_and_limits() {
        let mut c = Config::default();
        c.profile.max_mb = 1;
        c.profile.max_profiles = 1;
        c.profile.pushes_per_minute = 3;
        let h = Hub::new(c, Store::open_in_memory().unwrap());
        let big = "A".repeat(700 * 1024);
        assert_eq!(push_(&h, vec![rec("a", 1, &big)]).unwrap().accepted, 1);
        let e = push_(&h, vec![rec("b", 1, &big)]).unwrap_err();
        assert_eq!(e.0, ErrorCode::TooLarge, "over the cap: nothing stored");
        assert_eq!(info(&h, &ProfileId { profile_id: ID.into() }).unwrap().items, 1);
        let other =
            ProfilePush { profile_id: "q-0123456789abcdef".into(), device: None, records: vec![rec("a", 1, "QQ")] };
        assert_eq!(push(&h, &other, 1000).unwrap_err().0, ErrorCode::TooLarge, "one profile at most");
        assert!(push_(&h, vec![]).is_ok(), "third push this minute");
        assert_eq!(push_(&h, vec![]).unwrap_err().0, ErrorCode::BadRequest, "fourth push this minute");
        let h = hub();
        for (r, what) in [
            (rec("a b", 1, "QQ"), "space in uid"),
            (rec("a", -1, "QQ"), "negative time"),
            (rec("a", 1, "not base64!"), "blob charset"),
            (
                ProfileRecord { tbl: "hosts".into(), uid: "a".into(), modified: 1, deleted: false, blob: None },
                "no blob",
            ),
        ] {
            assert_eq!(push_(&h, vec![r]).unwrap_err().0, ErrorCode::BadRequest, "{what}");
        }
        let short = ProfilePush { profile_id: "short".into(), device: None, records: vec![] };
        assert_eq!(push(&h, &short, 1).unwrap_err().0, ErrorCode::BadRequest);
        // An unknown profile reads as empty; an empty push creates nothing.
        assert_eq!(push_(&h, vec![]).unwrap().seq, 0);
        assert!(list(&h).unwrap().profiles.is_empty());
        assert_eq!(info(&h, &ProfileId { profile_id: ID.into() }).unwrap().seq, 0);
    }

    #[test]
    fn pages_prunes_tombstones_and_drops() {
        let h = hub();
        let recs: Vec<_> = (0..5).map(|i| rec(&format!("u{i}"), 1, "QQ")).collect();
        push_(&h, recs).unwrap();
        let p = pull(&h, &ProfilePull { profile_id: ID.into(), after: 0, limit: Some(2) }).unwrap();
        assert_eq!((p.records.len(), p.next, p.more), (2, 2, true));
        let p = pull(&h, &ProfilePull { profile_id: ID.into(), after: 2, limit: Some(5) }).unwrap();
        assert_eq!((p.records.len(), p.next, p.more), (3, 5, false));
        let tomb = ProfileRecord { tbl: "hosts".into(), uid: "u0".into(), modified: 2, deleted: true, blob: None };
        push_(&h, vec![tomb]).unwrap();
        // 91 days later the tombstone (modified at 2 ms) goes.
        assert_eq!(prune(&h, 91 * 86400).unwrap(), 1);
        let i = info(&h, &ProfileId { profile_id: ID.into() }).unwrap();
        assert_eq!((i.items, i.tombstones, i.pruned_through), (4, 0, 6));
        assert!(pull_(&h, 3).resync, "a cursor before the pruned tombstone must start over");
        assert!(!pull_(&h, 0).resync && !pull_(&h, 6).resync);
        assert_eq!(list(&h).unwrap().profiles.len(), 1);
        assert!(drop(&h, &ProfileId { profile_id: ID.into() }).unwrap());
        assert!(list(&h).unwrap().profiles.is_empty());
    }

    #[test]
    fn off_means_off() {
        let mut c = Config::default();
        c.profile.enabled = false;
        let h = Hub::new(c, Store::open_in_memory().unwrap());
        assert_eq!(list(&h).unwrap_err().0, ErrorCode::CapabilityDisabled);
    }
}
