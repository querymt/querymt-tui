// src/server_manager.rs — Local qmtcode server discovery, spawning, and supervision.
//
// All server lifecycle logic is isolated here.  The rest of the codebase only
// interacts through:
//   - `ServerEvent`          (channel messages to the run_loop)
//   - `ServerState`          (stored on App for UI display)
//   - `ServerManagerConfig`  (built from TuiConfig in main)
//   - `find_binary()`        (called once at startup)
//   - `supervisor()`         (spawned as a tokio task)

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use fs2::FileExt;
use tokio::sync::mpsc;

use crate::config::ServerLaunchMode;

// ── Public types ──────────────────────────────────────────────────────────────

/// Events sent from the supervisor task to the TUI run_loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerEvent {
    /// Supervisor is about to spawn the server process.
    Starting,
    /// Server is up and accepting TCP connections.
    Started,
    /// No `qmtcode` binary could be found.
    BinaryNotFound,
    /// Server process failed to start.
    StartFailed { error: String },
    /// Server process exited (will be restarted if owner).
    Stopped { reason: String },
    /// `--api` start failed; retrying with `--dashboard` for older qmtcode builds.
    FallingBackToDashboard,
}

/// Server lifecycle state stored on [`crate::app::App`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ServerState {
    /// Auto-start is disabled; no supervision active.
    #[default]
    Disabled,
    /// No `qmtcode` binary was found on startup.
    BinaryNotFound,
    /// Supervisor is spawning the server.
    Starting,
    /// Server is running (either spawned by us or external).
    Running,
    /// Server spawn failed.
    StartFailed { error: String },
    /// Server exited; supervisor will restart it.
    Restarting { reason: String },
}

/// Configuration consumed by [`supervisor()`], built from CLI + config file.
#[derive(Debug, Clone)]
pub struct ServerManagerConfig {
    pub addr: String,
    pub launch_mode: ServerLaunchMode,
    pub binary_args: Vec<String>,
    pub shutdown_on_exit: bool,
    /// Override the lock-file path (default: `~/.cache/qmt/server.lock` or
    /// `$XDG_RUNTIME_DIR/qmt/server.lock`).  Mainly useful for tests.
    pub lock_path: Option<PathBuf>,
    /// How long to wait for the server to accept TCP connections after spawn.
    /// Default: 15 s.
    pub ready_timeout: Option<Duration>,
}

// ── Lock file ─────────────────────────────────────────────────────────────────

/// Default path for the cross-instance spawn lock.
fn default_lock_path() -> PathBuf {
    dirs::runtime_dir()
        .or_else(dirs::cache_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("qmt")
        .join("server.lock")
}

/// Try to acquire an exclusive (non-blocking) lock on `path`.
///
/// Returns `Some(file_handle)` when the lock is acquired.  The lock is held
/// for the lifetime of the returned `File` and auto-released when dropped or
/// if the process crashes.
///
/// Returns `None` when another process already holds the lock.
fn try_acquire_lock_at(path: &Path) -> Option<File> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    file.try_lock_exclusive().ok()?;
    Some(file)
}

/// Try to acquire the default spawn lock.
fn try_acquire_lock() -> Option<File> {
    try_acquire_lock_at(&default_lock_path())
}

// ── Binary discovery ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryDiscovery {
    pub binary: Option<OsString>,
    pub configured_path: Option<String>,
    pub configured_exists: bool,
    pub used_path_lookup: bool,
}

/// Locate the `qmtcode` binary.
///
/// Checks `configured_path` first (if provided), then falls back to a `$PATH`
/// lookup by running `qmtcode --version`.
pub fn find_binary_info(configured_path: Option<&str>) -> BinaryDiscovery {
    let configured_path = configured_path.map(str::to_string);
    let configured_exists = configured_path
        .as_deref()
        .is_some_and(|p| Path::new(p).exists());
    if configured_exists {
        return BinaryDiscovery {
            binary: configured_path.clone().map(OsString::from),
            configured_path,
            configured_exists: true,
            used_path_lookup: false,
        };
    }

    let used_path_lookup = true;
    let ok = std::process::Command::new("qmtcode")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    BinaryDiscovery {
        binary: ok.then(|| OsString::from("qmtcode")),
        configured_path,
        configured_exists,
        used_path_lookup,
    }
}

pub fn find_binary(configured_path: Option<&str>) -> Option<OsString> {
    find_binary_info(configured_path).binary
}

// ── TCP probe ─────────────────────────────────────────────────────────────────

/// Check whether something is listening on `addr` (quick TCP connect).
pub async fn probe(addr: &str) -> bool {
    tokio::net::TcpStream::connect(addr).await.is_ok()
}

/// Poll `addr` until a connection succeeds or `timeout` elapses.
async fn wait_until_ready(addr: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if probe(addr).await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// ── Build spawn arguments ─────────────────────────────────────────────────────

/// Return the effective CLI arguments for the spawned server.
///
/// When `extra_args` is empty the default is mode-dependent.
fn build_spawn_args(
    addr: &str,
    launch_mode: ServerLaunchMode,
    extra_args: &[String],
) -> Vec<String> {
    if extra_args.is_empty() {
        match launch_mode {
            ServerLaunchMode::Api => vec![format!("--api={addr}")],
            ServerLaunchMode::Dashboard => vec![format!("--dashboard={addr}")],
        }
    } else {
        extra_args.to_vec()
    }
}

// ── Supervisor task ───────────────────────────────────────────────────────────

/// Long-running async task that manages the `qmtcode` server process.
///
/// * Acquires a file lock so that only one TUI instance spawns the server.
/// * Probes the configured address before spawning.
/// * Restarts the server automatically if it exits while the TUI is running.
/// * Kills the child on shutdown when `config.shutdown_on_exit` is `true`.
pub async fn supervisor(
    config: ServerManagerConfig,
    binary: OsString,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    mut shutdown_rx: mpsc::Receiver<()>,
) {
    // Attempt to become the "owner" — the single instance allowed to spawn.
    let lock_path = config.lock_path.clone().unwrap_or_else(default_lock_path);
    let _lock_guard = try_acquire_lock_at(&lock_path);
    let is_owner = _lock_guard.is_some();
    let ready_timeout = config.ready_timeout.unwrap_or(Duration::from_secs(15));

    // Fallback: when the default Api mode is in use (no custom binary_args), and
    // the server fails to start, automatically retry with --dashboard so that
    // users with older qmtcode builds (which lack --api) are not stuck.
    let can_fallback = config.launch_mode == ServerLaunchMode::Api && config.binary_args.is_empty();
    let mut effective_mode = config.launch_mode;
    let mut api_fallback_done = false;

    loop {
        // ── Phase 1: server already running — wait for it to go down ──────
        if probe(&config.addr).await {
            let _ = event_tx.send(ServerEvent::Started);
            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => return,
                    _ = tokio::time::sleep(Duration::from_secs(3)) => {
                        if !probe(&config.addr).await { break; }
                    }
                }
            }
            if !is_owner {
                let _ = event_tx.send(ServerEvent::Stopped {
                    reason: "server went down (managed by another instance)".into(),
                });
                tokio::select! {
                    _ = shutdown_rx.recv() => return,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => continue,
                }
            }
        }

        // ── Phase 2: not the owner — keep polling ─────────────────────────
        if !is_owner {
            tokio::select! {
                _ = shutdown_rx.recv() => return,
                _ = tokio::time::sleep(Duration::from_secs(2)) => continue,
            }
        }

        // ── Phase 3: we are the owner, spawn the server ───────────────────
        let _ = event_tx.send(ServerEvent::Starting);

        let args = build_spawn_args(&config.addr, effective_mode, &config.binary_args);

        let mut child = match tokio::process::Command::new(&binary)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = event_tx.send(ServerEvent::StartFailed {
                    error: e.to_string(),
                });
                tokio::select! {
                    _ = shutdown_rx.recv() => return,
                    _ = tokio::time::sleep(Duration::from_secs(5)) => continue,
                }
            }
        };

        // Wait for the server to accept connections, bailing early if the
        // child exits first (e.g. it rejected the --api flag and quit).
        let server_up = {
            let deadline = tokio::time::Instant::now() + ready_timeout;
            loop {
                if probe(&config.addr).await {
                    break true;
                }
                if let Ok(Some(_)) = child.try_wait() {
                    // Child exited before the port was open; one final probe
                    // in case it daemonised and the parent just exited.
                    break probe(&config.addr).await;
                }
                if tokio::time::Instant::now() >= deadline {
                    break false;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        };

        if !server_up {
            let _ = child.kill().await;

            // First failure with --api: immediately retry with --dashboard.
            if can_fallback && !api_fallback_done {
                api_fallback_done = true;
                effective_mode = ServerLaunchMode::Dashboard;
                let _ = event_tx.send(ServerEvent::FallingBackToDashboard);
                continue;
            }

            let _ = event_tx.send(ServerEvent::StartFailed {
                error: format!("server not responding after {} s", ready_timeout.as_secs()),
            });
            tokio::select! {
                _ = shutdown_rx.recv() => return,
                _ = tokio::time::sleep(Duration::from_secs(5)) => continue,
            }
        }

        let _ = event_tx.send(ServerEvent::Started);

        tokio::select! {
            _ = shutdown_rx.recv() => {
                if config.shutdown_on_exit {
                    let _ = child.kill().await;
                }
                return;
            }
            status = child.wait() => {
                let reason = match status {
                    Ok(s) => format!("exited: {s}"),
                    Err(e) => format!("error: {e}"),
                };
                let _ = event_tx.send(ServerEvent::Stopped { reason });
                tokio::select! {
                    _ = shutdown_rx.recv() => return,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    // Helper: create a unique temp dir for lock tests.
    fn temp_lock_path(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "qmt-srv-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("server.lock")
    }

    // ── build_spawn_args ──────────────────────────────────────────────────────

    #[test]
    fn spawn_args_default_uses_api_flag() {
        let args = build_spawn_args("127.0.0.1:3030", ServerLaunchMode::Api, &[]);
        assert_eq!(args, vec!["--api=127.0.0.1:3030"]);
    }

    #[test]
    fn spawn_args_dashboard_mode_uses_dashboard_flag() {
        let args = build_spawn_args("127.0.0.1:3030", ServerLaunchMode::Dashboard, &[]);
        assert_eq!(args, vec!["--dashboard=127.0.0.1:3030"]);
    }

    #[test]
    fn spawn_args_custom_overrides_default() {
        let custom = vec!["--dashboard=0.0.0.0:9999".to_string(), "--mesh".to_string()];
        let args = build_spawn_args("127.0.0.1:3030", ServerLaunchMode::Api, &custom);
        assert_eq!(args, custom);
    }

    // ── Lock file ─────────────────────────────────────────────────────────────

    #[test]
    fn lock_acquired_on_fresh_path() {
        let path = temp_lock_path("fresh");
        let guard = try_acquire_lock_at(&path);
        assert!(guard.is_some(), "should acquire lock on fresh path");
    }

    #[test]
    fn lock_fails_when_already_held() {
        let path = temp_lock_path("double");
        let _first = try_acquire_lock_at(&path).expect("first lock should succeed");
        let second = try_acquire_lock_at(&path);
        assert!(
            second.is_none(),
            "second lock should fail while first is held"
        );
    }

    #[test]
    fn lock_released_on_drop() {
        let path = temp_lock_path("drop");
        {
            let _guard = try_acquire_lock_at(&path).expect("lock should succeed");
        }
        // After drop, another acquisition should succeed.
        let guard = try_acquire_lock_at(&path);
        assert!(guard.is_some(), "lock should be available after drop");
    }

    #[test]
    fn lock_creates_parent_dirs() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let deep = std::env::temp_dir()
            .join(format!("qmt-srv-deep-{}-{nanos}", std::process::id()))
            .join("a")
            .join("b")
            .join("server.lock");
        assert!(!deep.parent().unwrap().exists());
        let guard = try_acquire_lock_at(&deep);
        assert!(guard.is_some(), "should create dirs and acquire lock");
    }

    // ── find_binary ───────────────────────────────────────────────────────────

    #[test]
    fn find_binary_with_valid_configured_path() {
        // Create a temp file to act as a "binary"
        let dir = std::env::temp_dir().join(format!(
            "qmt-srv-bin-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let fake_bin = dir.join("qmtcode");
        File::create(&fake_bin).unwrap();

        let result = find_binary(Some(fake_bin.to_str().unwrap()));
        assert_eq!(result, Some(OsString::from(fake_bin)));
    }

    #[test]
    fn find_binary_info_reports_missing_configured_path_and_path_lookup_attempt() {
        let info = find_binary_info(Some("/nonexistent/path/to/qmtcode"));

        assert_eq!(
            info.configured_path,
            Some("/nonexistent/path/to/qmtcode".into())
        );
        assert!(!info.configured_exists);
        assert!(info.used_path_lookup);
        if let Some(ref binary) = info.binary {
            assert_ne!(binary, "/nonexistent/path/to/qmtcode");
        }
    }

    #[test]
    fn find_binary_info_without_configured_path_still_attempts_path_lookup() {
        let info = find_binary_info(None);

        assert_eq!(info.configured_path, None);
        assert!(info.used_path_lookup);
    }

    // ── probe ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn probe_returns_false_for_unbound_port() {
        // Port 0 trick: bind, get the port, close, then probe the closed port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(!probe(&format!("127.0.0.1:{port}")).await);
    }

    #[tokio::test]
    async fn probe_returns_true_for_listening_port() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        assert!(probe(&addr).await);
    }

    // ── wait_until_ready ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn wait_until_ready_succeeds_when_already_listening() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        assert!(wait_until_ready(&addr, Duration::from_secs(1)).await);
    }

    #[tokio::test]
    async fn wait_until_ready_times_out_when_nothing_listens() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(!wait_until_ready(&format!("127.0.0.1:{port}"), Duration::from_millis(500)).await);
    }

    #[tokio::test]
    async fn wait_until_ready_succeeds_when_listener_appears_later() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let addr = format!("127.0.0.1:{port}");
        let addr2 = addr.clone();

        // Start listener after 300ms
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            tokio::net::TcpListener::bind(&addr2).await.unwrap()
        });

        assert!(wait_until_ready(&addr, Duration::from_secs(2)).await);
        drop(handle);
    }

    // ── supervisor ────────────────────────────────────────────────────────────

    /// Helper: build a test config with an isolated lock file and short timeout.
    fn test_config(addr: &str, label: &str) -> ServerManagerConfig {
        ServerManagerConfig {
            addr: addr.to_string(),
            launch_mode: ServerLaunchMode::Api,
            binary_args: vec![],
            shutdown_on_exit: true,
            lock_path: Some(temp_lock_path(label)),
            ready_timeout: Some(Duration::from_secs(2)),
        }
    }

    #[tokio::test]
    async fn supervisor_sends_started_when_server_already_running() {
        // Start a TCP listener to simulate an already-running server.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let config = test_config(&addr, "sup-already-running");

        tokio::spawn(supervisor(
            config,
            OsString::from("unused-binary"),
            event_tx,
            shutdown_rx,
        ));

        // Should receive Started (server already up).
        let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("should receive event within timeout")
            .expect("channel should not be closed");
        assert_eq!(event, ServerEvent::Started);

        // Shutdown.
        let _ = shutdown_tx.send(()).await;
    }

    #[tokio::test]
    async fn supervisor_sends_start_failed_for_bad_binary() {
        // Use a non-existent binary and a port that is NOT listening.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let config = test_config(&format!("127.0.0.1:{port}"), "sup-bad-binary");

        tokio::spawn(supervisor(
            config,
            OsString::from("/nonexistent/qmtcode-fake-binary"),
            event_tx,
            shutdown_rx,
        ));

        // First event should be Starting.
        let ev1 = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .expect("channel open");
        assert_eq!(ev1, ServerEvent::Starting);

        // Second event should be StartFailed.
        let ev2 = tokio::time::timeout(Duration::from_secs(3), event_rx.recv())
            .await
            .expect("timeout")
            .expect("channel open");
        assert!(
            matches!(ev2, ServerEvent::StartFailed { .. }),
            "expected StartFailed, got {ev2:?}"
        );

        let _ = shutdown_tx.send(()).await;
    }

    #[tokio::test]
    async fn supervisor_shuts_down_on_signal() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let config = test_config(&addr, "sup-shutdown");

        let handle = tokio::spawn(supervisor(
            config,
            OsString::from("unused"),
            event_tx,
            shutdown_rx,
        ));

        // Give supervisor time to enter its loop.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let _ = shutdown_tx.send(()).await;
        let result = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(
            result.is_ok(),
            "supervisor should exit after shutdown signal"
        );
    }

    #[tokio::test]
    async fn supervisor_detects_server_going_down() {
        // Start a listener, let supervisor see it, then drop listener.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let config = test_config(&addr, "sup-going-down");

        tokio::spawn(supervisor(
            config,
            // Use a bad binary so that after the server goes down, the spawn
            // fails — giving us a deterministic StartFailed to assert on.
            OsString::from("/nonexistent/qmtcode-fake"),
            event_tx,
            shutdown_rx,
        ));

        // First: Started (server already up).
        let ev1 = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev1, ServerEvent::Started);

        // Drop the listener to simulate server going down.
        drop(listener);

        // We should see either Stopped or Starting (depending on lock ownership),
        // followed eventually by Starting + StartFailed (bad binary).
        let mut saw_reaction = false;
        for _ in 0..5 {
            if let Ok(Some(
                ServerEvent::Stopped { .. }
                | ServerEvent::Starting
                | ServerEvent::StartFailed { .. },
            )) = tokio::time::timeout(Duration::from_secs(5), event_rx.recv()).await
            {
                saw_reaction = true;
                break;
            }
        }
        assert!(saw_reaction, "supervisor should react to server going down");

        let _ = shutdown_tx.send(()).await;
    }

    // ── Supervisor with real short-lived process ──────────────────────────────

    #[tokio::test]
    async fn supervisor_sends_stopped_when_child_exits() {
        // Use `true` as a binary that exits immediately.
        // Server won't actually listen, but we can test the child-exit path.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let config = test_config(&format!("127.0.0.1:{port}"), "sup-child-exits");

        tokio::spawn(supervisor(
            config,
            OsString::from("true"), // exits immediately with 0
            event_tx,
            shutdown_rx,
        ));

        // Expect: Starting, then StartFailed (ready timeout) or Stopped (child exited).
        let mut saw_starting = false;
        let mut saw_end = false;
        for _ in 0..5 {
            if let Ok(Some(ev)) =
                tokio::time::timeout(Duration::from_secs(5), event_rx.recv()).await
            {
                match ev {
                    ServerEvent::Starting => saw_starting = true,
                    ServerEvent::Stopped { .. } | ServerEvent::StartFailed { .. } => {
                        saw_end = true;
                        break;
                    }
                    _ => {}
                }
            }
        }
        assert!(saw_starting, "should have seen Starting");
        assert!(
            saw_end,
            "should have seen Stopped or StartFailed after child exited"
        );

        let _ = shutdown_tx.send(()).await;
    }

    // ── --api → --dashboard fallback ─────────────────────────────────────────

    /// When launch_mode=Api and binary_args is empty, a binary that exits
    /// immediately should trigger FallingBackToDashboard then a second Starting.
    #[tokio::test]
    async fn supervisor_falls_back_to_dashboard_on_api_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        // `true` exits immediately with success; server never listens.
        let config = ServerManagerConfig {
            addr: format!("127.0.0.1:{port}"),
            launch_mode: ServerLaunchMode::Api,
            binary_args: vec![],
            shutdown_on_exit: true,
            lock_path: Some(temp_lock_path("fallback-api")),
            ready_timeout: Some(Duration::from_millis(300)),
        };

        tokio::spawn(supervisor(
            config,
            OsString::from("true"),
            event_tx,
            shutdown_rx,
        ));

        // Collect events until we see FallingBackToDashboard or give up.
        let mut saw_fallback = false;
        let mut saw_second_starting = false;
        let mut events_seen = 0;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), event_rx.recv()).await {
                Ok(Some(ServerEvent::FallingBackToDashboard)) => {
                    saw_fallback = true;
                }
                Ok(Some(ServerEvent::Starting)) if saw_fallback => {
                    saw_second_starting = true;
                    break;
                }
                Ok(Some(_)) => {}
                _ => break,
            }
            events_seen += 1;
            if events_seen > 10 {
                break;
            }
        }

        let _ = shutdown_tx.send(()).await;
        assert!(saw_fallback, "expected FallingBackToDashboard event");
        assert!(
            saw_second_starting,
            "expected a second Starting after fallback"
        );
    }

    /// When binary_args is non-empty, the fallback must NOT fire even if the
    /// binary exits immediately — the user has overridden the args explicitly.
    #[tokio::test]
    async fn supervisor_no_fallback_when_binary_args_set() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let config = ServerManagerConfig {
            addr: format!("127.0.0.1:{port}"),
            launch_mode: ServerLaunchMode::Api,
            binary_args: vec!["--custom-arg".into()], // non-empty → no fallback
            shutdown_on_exit: true,
            lock_path: Some(temp_lock_path("no-fallback-args")),
            ready_timeout: Some(Duration::from_millis(300)),
        };

        tokio::spawn(supervisor(
            config,
            OsString::from("true"),
            event_tx,
            shutdown_rx,
        ));

        // Drain a handful of events; none should be FallingBackToDashboard.
        let mut saw_fallback = false;
        for _ in 0..6 {
            match tokio::time::timeout(Duration::from_secs(3), event_rx.recv()).await {
                Ok(Some(ServerEvent::FallingBackToDashboard)) => {
                    saw_fallback = true;
                    break;
                }
                Ok(Some(ServerEvent::StartFailed { .. })) => break,
                Ok(Some(_)) => {}
                _ => break,
            }
        }

        let _ = shutdown_tx.send(()).await;
        assert!(
            !saw_fallback,
            "FallingBackToDashboard should not fire when binary_args is set"
        );
    }

    /// When launch_mode is Dashboard from the start, no fallback should fire.
    #[tokio::test]
    async fn supervisor_no_fallback_when_mode_is_dashboard() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let config = ServerManagerConfig {
            addr: format!("127.0.0.1:{port}"),
            launch_mode: ServerLaunchMode::Dashboard, // already dashboard
            binary_args: vec![],
            shutdown_on_exit: true,
            lock_path: Some(temp_lock_path("no-fallback-dashboard")),
            ready_timeout: Some(Duration::from_millis(300)),
        };

        tokio::spawn(supervisor(
            config,
            OsString::from("true"),
            event_tx,
            shutdown_rx,
        ));

        let mut saw_fallback = false;
        for _ in 0..6 {
            match tokio::time::timeout(Duration::from_secs(3), event_rx.recv()).await {
                Ok(Some(ServerEvent::FallingBackToDashboard)) => {
                    saw_fallback = true;
                    break;
                }
                Ok(Some(ServerEvent::StartFailed { .. })) => break,
                Ok(Some(_)) => {}
                _ => break,
            }
        }

        let _ = shutdown_tx.send(()).await;
        assert!(
            !saw_fallback,
            "FallingBackToDashboard should not fire when mode is already Dashboard"
        );
    }
}
