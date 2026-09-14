//! Which client instance is alive, and the process trees it started.
//!
//! A client leaves no notice when it goes away: its process is killed outright,
//! or it starts again under a new identity while its earlier children keep
//! running. What it starts is long-lived by design -- language servers, agents,
//! interactive shells -- so without a record those outlive the client that
//! needed them and another set is started the next time it runs.
//!
//! Two things end an instance's ownership of what it started: silence (no
//! heartbeat for `IDLE_TIMEOUT`) and replacement (a different identity
//! authenticates against the same daemon). Either way the recorded process
//! groups are signalled as a unit, so grandchildren and great-grandchildren go
//! with their parent instead of surviving as orphans.
//!
//! Identity rides on the SSH user name (see `protocol::CLIENT_ID_PREFIX`):
//! every connection a client opens -- the pooled command connections and the
//! periodic management poll alike -- already carries one, so no payload format
//! had to change and no extra request had to be invented. The poll doubles as
//! the heartbeat for free, and only the loss of it is ever written to the log.

use std::collections::{BTreeSet, HashMap};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// How long an instance may go unheard from before its session is treated as
/// over. A client polls the management listener every ten seconds, so this
/// tolerates three missed rounds.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the sweep looks for instances that have gone quiet.
const SWEEP_INTERVAL: Duration = Duration::from_secs(5);
/// Grace between the polite signal and the one that cannot be ignored.
const TERM_GRACE: Duration = Duration::from_secs(1);
/// Lowest process group id worth signalling: 0 addresses the caller's own
/// group and 1 belongs to init, so both reach far beyond an instance's tree.
const MIN_GROUP: i32 = 2;

/// One client instance: when it was last heard from, and the process groups it
/// started that have not exited yet.
struct Peer {
    last_seen: Instant,
    groups: BTreeSet<i32>,
}

/// Live instances, keyed by the identity their connections authenticated as.
static PEERS: LazyLock<Mutex<HashMap<String, Peer>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Borrows the instance table, ignoring a poisoned lock (the table is plain
/// data that stays consistent, so a panic elsewhere must not disable it).
fn peers() -> std::sync::MutexGuard<'static, HashMap<String, Peer>> {
    PEERS.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Whether a user name identifies an instance worth tracking. An older client
/// authenticates as a bare account name, which names no instance: such a
/// connection is left behaving exactly as it did before this record existed.
fn is_tracked(client_id: &str) -> bool {
    client_id.starts_with(crate::protocol::CLIENT_ID_PREFIX)
}

/// Records that the instance behind `client_id` is alive.
///
/// An identity not seen before replaces whatever the daemon finds: the client
/// those earlier entries belonged to is gone, and everything it started goes
/// with it. That is the whole point -- a launch that follows a crash must not
/// inherit the crashed run's servers.
pub(crate) fn touch(client_id: &str) {
    if !is_tracked(client_id) {
        return;
    }
    let replaced: Vec<String> = {
        let mut table = peers();
        if let Some(peer) = table.get_mut(client_id) {
            peer.last_seen = Instant::now();
            return;
        }
        let previous: Vec<String> = table.keys().cloned().collect();
        table.insert(
            client_id.to_string(),
            Peer {
                last_seen: Instant::now(),
                groups: BTreeSet::new(),
            },
        );
        previous
    };
    log::info!("conn: client {client_id} connected");
    for id in replaced {
        retire(&id, "replaced by a new client");
    }
}

/// Records a process group as belonging to an instance.
pub(crate) fn add_group(client_id: &str, pgid: i32) {
    if !is_tracked(client_id) || pgid < MIN_GROUP {
        return;
    }
    let mut table = peers();
    let Some(peer) = table.get_mut(client_id) else {
        return;
    };
    peer.groups.insert(pgid);
    peer.last_seen = Instant::now();
}

/// Forgets a process group that exited on its own.
pub(crate) fn drop_group(client_id: &str, pgid: i32) {
    let mut table = peers();
    let Some(peer) = table.get_mut(client_id) else {
        return;
    };
    peer.groups.remove(&pgid);
}

/// Runs the sweep for the lifetime of the process: instances that have gone
/// quiet are retired with everything they started.
pub(crate) fn spawn_sweeper() {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        ticker.tick().await; // the first tick fires immediately
        loop {
            ticker.tick().await;
            sweep();
        }
    });
}

/// Retires every instance not heard from within the timeout.
fn sweep() {
    let expired: Vec<String> = {
        let table = peers();
        let now = Instant::now();
        table
            .iter()
            .filter(|(_, peer)| now.duration_since(peer.last_seen) >= IDLE_TIMEOUT)
            .map(|(id, _)| id.clone())
            .collect()
    };
    for id in expired {
        retire(&id, "stopped heartbeating");
    }
}

/// Takes down everything an instance started and forgets it.
fn retire(client_id: &str, reason: &str) {
    let groups = {
        let mut table = peers();
        match table.remove(client_id) {
            Some(peer) => peer.groups,
            None => return,
        }
    };
    let count = groups.len();
    signal_groups(groups);
    log::warn!("conn: client {client_id} {reason}; took down {count} group(s)");
}

/// Signals every group politely, then again without appeal once the grace has
/// passed. The escalation runs off the caller, so an authentication request
/// handling a replacement is never held up by it.
fn signal_groups(groups: BTreeSet<i32>) {
    signal_now(&groups, libc::SIGTERM);
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        // Nothing to schedule the follow-up on, so a tree that ignores the
        // polite signal would keep running: insist immediately instead.
        signal_now(&groups, libc::SIGKILL);
        return;
    };
    handle.spawn(async move {
        tokio::time::sleep(TERM_GRACE).await;
        signal_now(&groups, libc::SIGKILL);
    });
}

/// Sends one signal to each group. A negative pid addresses the whole group,
/// which is what reaches the descendants that never appear in any table.
fn signal_now(groups: &BTreeSet<i32>, signal: i32) {
    for &pgid in groups {
        if pgid < MIN_GROUP {
            continue;
        }
        // SAFETY: a negative pid addresses the process group; the group was
        // created by this daemon (see `exec` and `pty`) and is still on record.
        unsafe { libc::kill(-pgid, signal) };
    }
}
