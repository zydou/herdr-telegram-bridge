//! Subprocess-level lock semantics against the real binary: the second
//! daemon must exit, `stop` must trust only the flock, and a daemon that
//! will not die is reported as a refusal — never as a fake success.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct TestEnv {
    _state: tempfile::TempDir,
    _config: tempfile::TempDir,
}

impl TestEnv {
    fn new() -> TestEnv {
        TestEnv {
            _state: tempfile::tempdir().unwrap(),
            // no config.toml in it: the daemon naps on a config error but
            // keeps holding the lock, which is all these tests need
            _config: tempfile::tempdir().unwrap(),
        }
    }

    fn state(&self) -> &std::path::Path {
        self._state.path()
    }

    fn run(&self, cmd: &str) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_htb"));
        c.arg(cmd)
            .env("HERDR_PLUGIN_STATE_DIR", self.state())
            .env("HERDR_PLUGIN_CONFIG_DIR", self._config.path())
            .env_remove("HERDR_BIN_PATH");
        c
    }

    fn spawn_daemon(&self) -> std::process::Child {
        self.run("daemon")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }
}

fn wait_until(what: &str, mut probe: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !probe() {
        if Instant::now() > deadline {
            panic!("timed out waiting for {what}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn status_running(env: &TestEnv) -> bool {
    let out = env.run("status").output().unwrap();
    String::from_utf8_lossy(&out.stdout).contains("running")
}

#[test]
fn second_daemon_exits_and_stop_releases_the_lock() {
    let env = TestEnv::new();
    let mut first = env.spawn_daemon();
    wait_until("daemon to take the lock", || status_running(&env));

    // a second daemon must refuse and exit non-zero, the first keeps running
    let second = env.spawn_daemon().wait_with_output().unwrap();
    assert!(!second.status.success(), "second daemon must not succeed");
    assert!(
        first.try_wait().unwrap().is_none(),
        "first daemon must survive"
    );

    // stop succeeds only once the lock is actually released
    let stopped = env.run("stop").output().unwrap();
    assert!(
        stopped.status.success(),
        "stop must succeed: {:?}",
        String::from_utf8_lossy(&stopped.stdout)
    );
    wait_until("daemon process to exit", || {
        first.try_wait().unwrap().is_some()
    });

    // and a second stop is a clean no-op
    let again = env.run("stop").output().unwrap();
    assert!(again.status.success());
    let _ = std::io::stdout().flush();
}

#[test]
fn stop_reports_refusal_when_the_lock_never_releases() {
    let env = TestEnv::new();
    let mut daemon = env.spawn_daemon();
    wait_until("daemon to take the lock", || status_running(&env));

    // sabotage the pid so the kill cannot reach the daemon: the lock stays
    // held, and stop must say so instead of claiming success
    let lockfile = env.state().join("bot.lock");
    std::fs::write(&lockfile, "1\n").unwrap(); // pid 1 is unkillable for us

    let started = Instant::now();
    let stopped = env.run("stop").output().unwrap();
    assert!(
        started.elapsed() >= Duration::from_secs(9),
        "must actually wait the timeout out"
    );
    assert!(!stopped.status.success(), "stop must refuse, not lie");
    assert!(
        String::from_utf8_lossy(&stopped.stderr).contains("refusing"),
        "{:?}",
        String::from_utf8_lossy(&stopped.stderr)
    );

    // cleanup: the daemon is still holding the lock
    let _ = daemon.kill();
    let _ = daemon.wait();
}
