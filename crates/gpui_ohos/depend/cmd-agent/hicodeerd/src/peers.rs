//! Which client instances are alive, and the process trees they started.
//!
//! A client leaves no notice when it goes away: its process is killed outright,
//! or it starts again under a new identity while its earlier children keep
//! running. What it starts is long-lived by design -- language servers, agents,
//! interactive shells -- so without a record those outlive the client that
//! needed them and another set is started the next time it runs.
//!
//! What ends an instance's ownership of what it started is the loss of its
//! management connection, and nothing else. A client holds that connection open
//! for as long as it runs and polls the management listener over it, so the
//! connection is a statement of presence: while it is up the instance is there
//! and its trees are left alone, and when it goes the trees go with it. No
//! timer is involved -- an instance that is merely idle, or one frozen by the
//! system while the device sleeps, keeps its connection and keeps its trees.
//!
//! That equivalence -- a dropped connection means a process that is gone --
//! rests on this being a loopback connection that neither side closes while the
//! instance is alive: the only way left for it to end is the client's file
//! descriptors being reclaimed by the kernel when its process goes away. Should
//! the daemon and its clients ever be split across machines, a connection could
//! then drop for reasons that say nothing about the process, and a grace period
//! would have to be given before retiring anything.
//!
//! The management connection must therefore stay silent and long-lived: no
//! keepalive, and a rekey interval long enough that a frozen client is never
//! asked to answer (see `REKEY_BYTE_LIMIT` / `REKEY_TIME_LIMIT` in `main`).
//! The pooled command connections may carry a keepalive and rekey sooner,
//! because losing one says nothing about the instance and retires nothing.
//!
//! Identity rides on the SSH user name (see `protocol::CLIENT_ID_PREFIX`):
//! every connection a client opens -- the pooled command connections and the
//! management connection alike -- already carries one, so no payload format had
//! to change and no extra request had to be invented.
//!
//! When the daemon itself exits, nothing may outlive it: [`retire_all`] takes
//! down every recorded tree on the way out.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

/// Grace between the polite signal and the one that cannot be ignored.
const TERM_GRACE: Duration = Duration::from_secs(1);
/// Lowest process group id worth signalling: 0 addresses the caller's own
/// group and 1 belongs to init, so both reach far beyond an instance's tree.
const MIN_GROUP: i32 = 2;

/// One client instance: the process groups it started that have not exited
/// yet, and the management connections currently open on its behalf. The
/// instance is alive for exactly as long as the latter is non-empty.
struct Peer {
    groups: BTreeSet<i32>,
    management_conns: BTreeSet<u64>,
}

/// Live instances, keyed by the identity their connections authenticated as.
static PEERS: LazyLock<Mutex<HashMap<String, Peer>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Hands out the token that identifies one management connection. Several may
/// be open for the same instance while a client reconnects, and telling them
/// apart is what keeps one connection closing from retiring an instance that
/// another is still vouching for.
static NEXT_MANAGEMENT_CONN: AtomicU64 = AtomicU64::new(1);

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

/// Allocates the token a management connection is known by.
pub(crate) fn new_management_token() -> u64 {
    NEXT_MANAGEMENT_CONN.fetch_add(1, Ordering::Relaxed)
}

/// Records that the instance behind `client_id` exists.
///
/// Called for every connection an instance opens, the pooled command
/// connections included, so a tree started before the management connection is
/// established is still recorded under the instance. Nothing beyond that join
/// happens: an instance already on record is left exactly as it is, and the
/// others are not touched at all.
pub(crate) fn touch(client_id: &str) {
    if !is_tracked(client_id) {
        return;
    }
    let fresh = {
        let mut table = peers();
        if table.contains_key(client_id) {
            false
        } else {
            table.insert(
                client_id.to_string(),
                Peer {
                    groups: BTreeSet::new(),
                    management_conns: BTreeSet::new(),
                },
            );
            true
        }
    };
    if fresh {
        log::info!("conn: client {client_id} connected");
    }
}

/// Records a management connection as open on behalf of `client_id`. The
/// instance stays alive until every such connection has been closed.
pub(crate) fn management_opened(client_id: &str, token: u64) {
    if !is_tracked(client_id) {
        return;
    }
    let mut table = peers();
    if let Some(peer) = table.get_mut(client_id) {
        peer.management_conns.insert(token);
        return;
    }
    let mut peer = Peer {
        groups: BTreeSet::new(),
        management_conns: BTreeSet::new(),
    };
    peer.management_conns.insert(token);
    table.insert(client_id.to_string(), peer);
}

/// Records a management connection as closed. The instance is over -- and
/// everything it started goes with it -- once none are left.
pub(crate) fn management_closed(client_id: &str, token: u64) {
    let over = {
        let mut table = peers();
        let Some(peer) = table.get_mut(client_id) else {
            return;
        };
        if !peer.management_conns.remove(&token) {
            return;
        }
        peer.management_conns.is_empty()
    };
    if over {
        retire(client_id, "management connection closed");
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
}

/// Forgets a process group that exited on its own.
pub(crate) fn drop_group(client_id: &str, pgid: i32) {
    let mut table = peers();
    let Some(peer) = table.get_mut(client_id) else {
        return;
    };
    peer.groups.remove(&pgid);
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

/// Takes down everything every instance started and clears the record.
///
/// Runs on the daemon's own way out, where there is no runtime to hand an
/// escalation to and no client left to serve: the trees are signalled in one
/// pass, given the same grace [`retire`] would give them, and then insisted
/// upon, so no child can outlive the daemon that owns it.
pub(crate) fn retire_all(reason: &str) {
    let drained: Vec<(String, BTreeSet<i32>)> = {
        let mut table = peers();
        table
            .drain()
            .map(|(client_id, peer)| (client_id, peer.groups))
            .collect()
    };
    if drained.is_empty() {
        return;
    }
    let instances = drained.len();
    let mut groups: BTreeSet<i32> = BTreeSet::new();
    for (client_id, peer_groups) in drained {
        log::warn!("conn: client {client_id} {reason}");
        groups.extend(peer_groups);
    }
    log::warn!(
        "conn: {reason}; taking down {} group(s) from {instances} client(s)",
        groups.len()
    );
    signal_now(&groups, libc::SIGTERM);
    std::thread::sleep(TERM_GRACE);
    signal_now(&groups, libc::SIGKILL);
}

/// Signals every group politely, then again without appeal once the grace has
/// passed. The escalation runs off the caller, so the connection teardown that
/// noticed the instance is gone is never held up by it.
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
