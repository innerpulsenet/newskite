//! Idle NNTP connections kept between jobs.
//!
//! A [`ConnectionPool`] holds authenticated connections that a finished run handed back, so
//! the next job (or the next worker of the same job) starts fetching without a TCP + TLS
//! handshake and a new login. Entries are keyed by the full server identity (host, port,
//! TLS, credentials) so a changed setting never reuses a stale session. Servers drop idle
//! sessions after a few minutes and a dead one costs a failed first request, so entries
//! older than [`POOL_IDLE_MAX`] are closed instead of reused.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use newskite_nntp::NntpConnection;
use newskite_types::ServerConfig;

/// Idle connections older than this are closed rather than handed out again.
pub const POOL_IDLE_MAX: Duration = Duration::from_secs(90);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Key {
    host: String,
    port: u16,
    tls: bool,
    username: String,
    password: String,
}

impl Key {
    fn of(cfg: &ServerConfig) -> Key {
        Key {
            host: cfg.host.trim().to_owned(),
            port: cfg.port,
            tls: cfg.tls,
            username: cfg.username.clone(),
            password: cfg.password.clone(),
        }
    }
}

struct Idle {
    key: Key,
    conn: NntpConnection,
    since: Instant,
}

/// Idle NNTP connections shared by consecutive runs. See the module docs.
#[derive(Default)]
pub struct ConnectionPool {
    idle: Mutex<Vec<Idle>>,
}

impl std::fmt::Debug for ConnectionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionPool").field("idle", &self.idle_count()).finish()
    }
}

impl ConnectionPool {
    /// An empty pool.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of idle connections currently held (expired ones included until the next
    /// [`take`](Self::take)).
    pub fn idle_count(&self) -> usize {
        lock(&self.idle).len()
    }

    /// Hands out the most recently returned healthy connection to `cfg`'s server, if any.
    /// Expired entries for any server are closed on the way.
    pub(crate) fn take(&self, cfg: &ServerConfig) -> Option<NntpConnection> {
        let key = Key::of(cfg);
        let mut expired = Vec::new();
        let picked = {
            let mut idle = lock(&self.idle);
            let now = Instant::now();
            let mut i = 0;
            while i < idle.len() {
                if now.saturating_duration_since(idle[i].since) >= POOL_IDLE_MAX {
                    expired.push(idle.swap_remove(i).conn);
                } else {
                    i += 1;
                }
            }
            idle.iter().rposition(|e| e.key == key).map(|p| idle.remove(p).conn)
        };
        close(expired);
        picked
    }

    /// Returns a connection for later reuse. Broken connections and ones with a pipelined
    /// reply pending are closed instead, and at most `cfg.connections` idle connections are
    /// kept per server (the oldest surplus one is closed) so the server never sees more
    /// sessions from us than its configured limit.
    pub(crate) fn put(&self, cfg: &ServerConfig, conn: NntpConnection) {
        if conn.is_broken() || conn.outstanding_bodies() > 0 {
            close(vec![conn]);
            return;
        }
        let key = Key::of(cfg);
        let cap = usize::from(cfg.connections.max(1));
        let mut surplus = Vec::new();
        {
            let mut idle = lock(&self.idle);
            while idle.iter().filter(|e| e.key == key).count() >= cap {
                match idle.iter().position(|e| e.key == key) {
                    Some(p) => surplus.push(idle.remove(p).conn),
                    None => break,
                }
            }
            idle.push(Idle { key, conn, since: Instant::now() });
        }
        close(surplus);
    }

    /// Closes every idle connection: politely (`QUIT` in a background task) when called
    /// inside a tokio runtime, otherwise by dropping the sockets.
    pub fn close_idle(&self) {
        let all: Vec<NntpConnection> = std::mem::take(&mut *lock(&self.idle)).into_iter().map(|e| e.conn).collect();
        close(all);
    }
}

fn close(conns: Vec<NntpConnection>) {
    if conns.is_empty() {
        return;
    }
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            for conn in conns {
                handle.spawn(conn.quit());
            }
        }
        Err(_) => drop(conns),
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
