use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One NNTP server. Passwords are stored in plain JSON inside the local settings
/// database (see README "Credential storage").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub username: String,
    pub password: String,
    /// Simultaneous connections allowed by this server/account.
    pub connections: u16,
    /// Lower value = tried first. Ties keep list order.
    pub priority: i32,
    pub enabled: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 563,
            tls: true,
            username: String::new(),
            password: String::new(),
            connections: 8,
            priority: 0,
            enabled: true,
        }
    }
}

impl ServerConfig {
    /// Enabled and has a non-empty host.
    pub fn is_usable(&self) -> bool {
        self.enabled && !self.host.trim().is_empty()
    }

    /// `host:port` for display and connecting.
    pub fn address(&self) -> String {
        format!("{}:{}", self.host.trim(), self.port)
    }

    pub fn has_credentials(&self) -> bool {
        !self.username.is_empty()
    }
}

/// Post-processing policy (PAR2 repair, archive extraction, cleanup).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PostProcessSettings {
    pub enabled: bool,
    pub repair: bool,
    pub extract: bool,
    /// After every archive set extracts: delete the archive volumes. On by default.
    pub cleanup: bool,
    /// Once PAR2 verified or repaired the download (or it has no PAR2 data) and nothing
    /// failed to extract: delete the parity files, par2's `.1` backups and release
    /// sidecars (.sfv, .nfo, ...), with or without archives. On by default.
    pub remove_leftovers: bool,
}

impl Default for PostProcessSettings {
    fn default() -> Self {
        Self { enabled: true, repair: true, extract: true, cleanup: true, remove_leftovers: true }
    }
}

/// The single settings document, persisted as JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub download_dir: PathBuf,
    pub watch_folder: PathBuf,
    pub watch_enabled: bool,
    /// Periodic watch-folder scan interval; 0 = manual scans only.
    pub watch_interval_secs: u64,
    /// Start the queue automatically after a watch-folder import.
    pub watch_auto_start: bool,
    /// Global cap on simultaneous segment fetches across all servers.
    pub max_connections: u16,
    /// KiB/s, 0 = unlimited.
    pub speed_limit_kbps: u64,
    /// Retries per segment for transient errors, walking servers in priority order.
    pub retries: u32,
    /// Stop a download as soon as more is missing than its PAR2 recovery data can rebuild.
    pub stop_unrepairable: bool,
    /// Shut the machine down when the queue empties (with a cancellable countdown).
    pub auto_shutdown: bool,
    pub auto_shutdown_countdown_secs: u32,
    pub postprocess: PostProcessSettings,
    pub servers: Vec<ServerConfig>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: default_download_dir(),
            watch_folder: PathBuf::new(),
            watch_enabled: false,
            watch_interval_secs: 60,
            watch_auto_start: false,
            max_connections: 20,
            speed_limit_kbps: 0,
            retries: 2,
            stop_unrepairable: true,
            auto_shutdown: false,
            auto_shutdown_countdown_secs: 60,
            postprocess: PostProcessSettings::default(),
            servers: vec![ServerConfig::default()],
        }
    }
}

impl Settings {
    /// The first enabled server with a non-empty host — the one runs use.
    pub fn first_usable_server(&self) -> Option<&ServerConfig> {
        self.servers.iter().find(|s| s.is_usable())
    }

    /// All usable servers: the first usable server leads, the rest follow by
    /// ascending `priority` (stable). Used for retry/fail-over.
    pub fn usable_servers(&self) -> Vec<ServerConfig> {
        let mut v: Vec<ServerConfig> = self.servers.iter().filter(|s| s.is_usable()).cloned().collect();
        if v.len() > 1 {
            let first = v.remove(0);
            v.sort_by_key(|s| s.priority);
            v.insert(0, first);
        }
        v
    }

    /// [`Settings::usable_servers`] with the fail-over servers moved ahead of the primary,
    /// which goes last: the order of a "retry on backup" run. `None` with fewer than two
    /// usable servers (there is no backup to ask).
    pub fn backup_first_servers(&self) -> Option<Vec<ServerConfig>> {
        let mut v = self.usable_servers();
        if v.len() < 2 {
            return None;
        }
        v.rotate_left(1);
        Some(v)
    }

    /// Effective number of simultaneous fetches: `max_connections` capped by the sum of
    /// usable servers' connection counts, never below 1.
    pub fn effective_connections(&self) -> usize {
        let server_total: usize = self.usable_servers().iter().map(|s| s.connections.max(1) as usize).sum();
        let cap = self.max_connections.max(1) as usize;
        if server_total == 0 { cap } else { cap.min(server_total) }
    }

    /// Speed limit in bytes/second, 0 = unlimited.
    pub fn speed_limit_bps(&self) -> u64 {
        self.speed_limit_kbps.saturating_mul(1024)
    }

    /// Guarantee the "at least one server row" invariant after loading/editing.
    pub fn normalized(mut self) -> Self {
        if self.servers.is_empty() {
            self.servers.push(ServerConfig { enabled: false, ..Default::default() });
        }
        if self.max_connections == 0 {
            self.max_connections = 1;
        }
        self
    }
}

/// `~/Downloads/Newskite` (or the platform equivalent), falling back to the home dir.
pub fn default_download_dir() -> PathBuf {
    if let Some(u) = directories::UserDirs::new() {
        if let Some(d) = u.download_dir() {
            return d.join(crate::APP_NAME);
        }
        return u.home_dir().join(crate::APP_NAME);
    }
    PathBuf::from(crate::APP_NAME)
}

/// Directory for the databases: `~/.local/share/newskite/db` on Linux (the directories crate lowercases it).
pub fn default_data_dir() -> PathBuf {
    directories::ProjectDirs::from("net", "innerpulse", crate::APP_NAME)
        .map(|p| p.data_dir().join("db"))
        .unwrap_or_else(|| PathBuf::from(".newskite-db"))
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn first_usable_skips_disabled_and_empty() {
        let mut s = Settings::default();
        s.servers = vec![
            ServerConfig { host: "".into(), ..Default::default() },
            ServerConfig { host: "a".into(), enabled: false, ..Default::default() },
            ServerConfig { host: "b".into(), priority: 5, ..Default::default() },
            ServerConfig { host: "c".into(), priority: 1, ..Default::default() },
        ];
        assert_eq!(s.first_usable_server().unwrap().host, "b");
        let hosts: Vec<_> = s.usable_servers().into_iter().map(|s| s.host).collect();
        assert_eq!(hosts, vec!["b", "c"]);
    }

    #[test]
    fn settings_json_round_trip_and_defaults_for_missing_fields() {
        let s = Settings::default();
        let j = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&j).unwrap(), s);
        let partial: Settings = serde_json::from_str(r#"{"max_connections": 4}"#).unwrap();
        assert_eq!(partial.max_connections, 4);
        assert_eq!(partial.retries, 2);
        assert!(partial.stop_unrepairable);
    }

    #[test]
    fn backup_first_puts_the_primary_last() {
        let mut s = Settings::default();
        s.servers = vec![ServerConfig { host: "only".into(), ..Default::default() }];
        assert_eq!(s.backup_first_servers(), None);
        s.servers = vec![
            ServerConfig { host: "a".into(), ..Default::default() },
            ServerConfig { host: "off".into(), enabled: false, ..Default::default() },
            ServerConfig { host: "c".into(), priority: 2, ..Default::default() },
            ServerConfig { host: "b".into(), priority: 1, ..Default::default() },
        ];
        let hosts: Vec<String> = s.backup_first_servers().unwrap().into_iter().map(|c| c.host).collect();
        assert_eq!(hosts, ["b", "c", "a"]);
    }

    #[test]
    fn effective_connections_caps() {
        let mut s = Settings::default();
        s.servers = vec![ServerConfig { host: "h".into(), connections: 6, ..Default::default() }];
        s.max_connections = 20;
        assert_eq!(s.effective_connections(), 6);
        s.max_connections = 3;
        assert_eq!(s.effective_connections(), 3);
    }
}
