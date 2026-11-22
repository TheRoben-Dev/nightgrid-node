//! The hub's socket: the node's framing and `hello`, the `fleet.*` methods.

use std::sync::Arc;

use nightgrid_node_proto::hub::{
    cap, method as hm, IngestBatch, Members, ProfileId, ProfilePull, ProfilePush, SyncParams,
};
use nightgrid_node_proto::{
    method, negotiate, Caps, CursorParams, ErrorCode, Hello, Request, Response, Welcome, MAX_LINE, PROTO_MIN,
    PROTO_VERSION,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::fleet::{self, Hub};
use crate::profile;

pub fn caps(hub: &Hub) -> Caps {
    let mut c: Caps = [(cap::FLEET.to_owned(), 1)].into_iter().collect();
    if hub.config.profile.enabled {
        c.insert(cap::PROFILE.to_owned(), 1);
    }
    c
}

#[derive(Debug, Default)]
pub struct Session {
    pub proto: Option<u32>,
    pub uid: String,
}

fn params<T: DeserializeOwned>(v: &Value) -> Result<T, Response> {
    let v = if v.is_null() { Value::Object(Default::default()) } else { v.clone() };
    serde_json::from_value(v).map_err(|e| Response::err(0, ErrorCode::BadRequest, format!("params: {e}")))
}

fn internal(e: impl std::fmt::Display) -> Response {
    nightgrid_node::warn!("hub request failed: {e}");
    Response::err(0, ErrorCode::Internal, "internal error (see the hub's log)")
}

fn own_uid(session: &Session) -> bool {
    // SAFETY: geteuid has no preconditions.
    session.uid.parse::<u32>().ok() == Some(unsafe { libc::geteuid() })
}

fn profile_err((c, m): (ErrorCode, String)) -> Response {
    if c == ErrorCode::Internal {
        return internal(m);
    }
    Response::err(0, c, m)
}

pub fn handle_line(hub: &Hub, session: &mut Session, line: &[u8]) -> Response {
    let req: Request = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => return Response::err(0, ErrorCode::BadRequest, format!("not a request: {e}")),
    };
    let id = req.id;
    let mut r = dispatch(hub, session, &req).unwrap_or_else(|e| e);
    r.id = id;
    r
}

fn dispatch(hub: &Hub, session: &mut Session, req: &Request) -> Result<Response, Response> {
    let now = crate::now_secs();
    if req.method == method::HELLO {
        let h: Hello = params(&req.params)?;
        let Some(v) = negotiate(h.proto_min, h.proto_max, PROTO_MIN, PROTO_VERSION) else {
            return Err(Response::err(
                0,
                ErrorCode::UnsupportedProto,
                format!("hub speaks protocol {PROTO_MIN}-{PROTO_VERSION}"),
            ));
        };
        session.proto = Some(v);
        let client: String = h.client.chars().filter(|c| !c.is_control()).take(80).collect();
        if own_uid(session) && client.starts_with("nightgrid-hub") {
            nightgrid_node::debug!("hello from {client:?} as uid {}", session.uid);
        } else {
            nightgrid_node::info!("hello from {client:?} as uid {} (protocol {v})", session.uid);
        }
        return Ok(Response::ok(0, Welcome { proto: v, node: hub.identity.clone(), caps: caps(hub) }));
    }
    if session.proto.is_none() {
        return Err(Response::err(0, ErrorCode::HelloRequired, "send hello first"));
    }
    match req.method.as_str() {
        hm::MEMBERS_GET => {
            let members = hub.store().members().map_err(internal)?;
            Ok(Response::ok(0, Members { members }))
        }
        hm::MEMBERS_SET => {
            let p: Members = params(&req.params)?;
            fleet::validate(&p.members).map_err(|e| Response::err(0, ErrorCode::BadRequest, e))?;
            hub.store().set_members(&p.members, now).map_err(internal)?;
            nightgrid_node::info!("hub members updated by uid {} ({} members)", session.uid, p.members.len());
            Ok(Response::ok(0, p))
        }
        hm::OVERVIEW => fleet::overview(hub, now).map(|o| Response::ok(0, o)).map_err(internal),
        hm::SYNC => {
            let p: SyncParams = params(&req.params)?;
            fleet::sync(hub, &p).map(|v| Response::ok(0, v)).map_err(|(c, m)| {
                if c == ErrorCode::Internal {
                    nightgrid_node::warn!("fleet.sync: {m}");
                }
                Response::err(0, c, m)
            })
        }
        hm::EVENTS_SYNC => {
            let p: CursorParams = params(&req.params)?;
            fleet::hub_events(hub, &p).map(|v| Response::ok(0, v)).map_err(internal)
        }
        hm::PLAN => {
            if !own_uid(session) {
                return Err(Response::err(0, ErrorCode::Forbidden, "only the hub's collector may ask for the plan"));
            }
            fleet::plan(hub).map(|p| Response::ok(0, p)).map_err(internal)
        }
        hm::INGEST => {
            if !own_uid(session) {
                return Err(Response::err(0, ErrorCode::Forbidden, "only the hub's collector may report"));
            }
            let b: IngestBatch = params(&req.params)?;
            fleet::ingest(hub, b, now).map(|_| Response::ok(0, Value::Null)).map_err(internal)
        }
        hm::PROFILE_LIST => profile::list(hub).map(|v| Response::ok(0, v)).map_err(profile_err),
        hm::PROFILE_INFO => {
            let p: ProfileId = params(&req.params)?;
            profile::info(hub, &p).map(|v| Response::ok(0, v)).map_err(profile_err)
        }
        hm::PROFILE_PUSH => {
            let p: ProfilePush = params(&req.params)?;
            let r = profile::push(hub, &p, now).map_err(profile_err)?;
            if r.accepted > 0 {
                nightgrid_node::debug!(
                    "profile push by uid {}: {} stored, {} ignored",
                    session.uid,
                    r.accepted,
                    r.ignored
                );
            }
            Ok(Response::ok(0, r))
        }
        hm::PROFILE_PULL => {
            let p: ProfilePull = params(&req.params)?;
            profile::pull(hub, &p).map(|v| Response::ok(0, v)).map_err(profile_err)
        }
        hm::PROFILE_DROP => {
            let p: ProfileId = params(&req.params)?;
            let dropped = profile::drop(hub, &p).map_err(profile_err)?;
            if dropped {
                nightgrid_node::info!("profile {} dropped by uid {}", p.profile_id, session.uid);
            }
            Ok(Response::ok(0, serde_json::json!({ "dropped": dropped })))
        }
        m => Err(Response::err(0, ErrorCode::UnknownMethod, format!("unknown method {m:?}"))),
    }
}

/// Serves the socket until the process ends.
pub async fn serve(listener: tokio::net::UnixListener, hub: Arc<Hub>) {
    let slots = Arc::new(tokio::sync::Semaphore::new(hub.config.server.max_connections as usize));
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            nightgrid_node::warn!("connection refused: too many open");
            continue;
        };
        let hub = hub.clone();
        tokio::spawn(async move {
            let uid = stream.peer_cred().map(|c| c.uid().to_string()).unwrap_or_else(|_| "?".into());
            if let Err(e) = connection(stream, &hub, uid.clone()).await {
                nightgrid_node::debug!("client (uid {uid}): {e}");
            }
            drop(permit);
        });
    }
}

async fn connection(stream: tokio::net::UnixStream, hub: &Arc<Hub>, uid: String) -> std::io::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);
    let mut session = Session { proto: None, uid };
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = (&mut r).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        let resp = if n > MAX_LINE {
            Response::err(0, ErrorCode::TooLarge, "line too long")
        } else {
            let hub = hub.clone();
            let line = std::mem::take(&mut buf);
            let mut s = std::mem::take(&mut session);
            // Store work can take a moment; keep the runtime free for others.
            let (resp, s, line) = tokio::task::spawn_blocking(move || {
                let r = handle_line(&hub, &mut s, &line);
                (r, s, line)
            })
            .await
            .map_err(std::io::Error::other)?;
            session = s;
            buf = line;
            resp
        };
        let mut out = serde_json::to_vec(&resp).unwrap_or_default();
        out.push(b'\n');
        w.write_all(&out).await?;
        if n > MAX_LINE {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::store::Store;
    use serde_json::json;

    fn call(h: &Hub, s: &mut Session, m: &str, p: Value) -> Response {
        handle_line(h, s, &serde_json::to_vec(&Request { id: 1, method: m.into(), params: p }).unwrap())
    }

    #[test]
    fn hello_first_and_only_the_collector_reports() {
        let h = Hub::new(Config::default(), Store::open_in_memory().unwrap());
        let mut s = Session { proto: None, uid: "4242".into() };
        let r = call(&h, &mut s, hm::OVERVIEW, Value::Null);
        assert_eq!(r.error.unwrap().code, ErrorCode::HelloRequired);
        let w = call(&h, &mut s, method::HELLO, json!({"protoMin": 1, "protoMax": 2, "client": "test"}));
        let w: Welcome = serde_json::from_value(w.result_value().unwrap()).unwrap();
        assert_eq!((w.caps.get("fleet"), w.node.node_id.len()), (Some(&1), 32));
        assert_eq!(w.caps.get("profile"), Some(&1));
        let p = json!({"profileId": "p-0123456789abcdef", "device": "desk",
                       "records": [{"tbl": "hosts", "uid": "u1", "modified": 5, "blob": "c2VhbGVk"}]});
        let r = call(&h, &mut s, hm::PROFILE_PUSH, p.clone());
        assert_eq!(r.result_value().unwrap()["accepted"], 1);
        assert_eq!(call(&h, &mut s, hm::PROFILE_PUSH, p).result_value().unwrap()["ignored"], 1, "replay");
        let r = call(&h, &mut s, hm::PROFILE_PULL, json!({"profileId": "p-0123456789abcdef", "after": 0}));
        let v = r.result_value().unwrap();
        assert_eq!((v["records"][0]["seq"].as_i64(), v["records"][0]["blob"].as_str()), (Some(1), Some("c2VhbGVk")));
        assert_eq!(v["records"][0]["deleted"], false);
        assert!(call(&h, &mut s, hm::OVERVIEW, Value::Null).error.is_none());
        let r = call(&h, &mut s, hm::INGEST, json!({"fingerprint": "", "answers": []}));
        assert_eq!(r.error.unwrap().code, ErrorCode::Forbidden);
        let r = call(&h, &mut s, hm::PLAN, Value::Null);
        assert_eq!(r.error.unwrap().code, ErrorCode::Forbidden);
        let r = call(&h, &mut s, "actions.run", Value::Null);
        assert_eq!(r.error.unwrap().code, ErrorCode::UnknownMethod, "the hub runs nothing");
        let bad = json!({"members": [{"id": "x", "name": "a", "address": "1.2.3.4:5", "fingerprint": "y"}]});
        assert_eq!(call(&h, &mut s, hm::MEMBERS_SET, bad).error.unwrap().code, ErrorCode::BadRequest);
    }
}
