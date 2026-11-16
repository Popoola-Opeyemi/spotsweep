//! End-to-end tests using temporary directory trees and the real executable.
//!
//! Every test gets its own data directory, config, SQLite database and
//! LaunchAgent directory.  No test touches the user's real Application Support
//! or Spotlight configuration.

use serde_json::Value;
use spotlight_dev_exclusions::config::Config;
use spotlight_dev_exclusions::database::Db;
use spotlight_dev_exclusions::model::ScanKind;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct TestEnv {
    _temp: tempfile::TempDir,
    data: PathBuf,
    root: PathBuf,
    config: PathBuf,
    launch_agents: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("projects");
        let config = data.join("config.toml");
        let launch_agents = temp.path().join("LaunchAgents");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&data).unwrap();
        let mut cfg = Config::default();
        cfg.scan.roots = vec![root.to_string_lossy().to_string()];
        cfg.export.directory = data.join("exports").to_string_lossy().to_string();
        cfg.save(&config).unwrap();
        Self {
            _temp: temp,
            data,
            root,
            config,
            launch_agents,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_spotlight-dev"));
        command
            .env("SPOTLIGHT_DEV_EXCLUSIONS_DATA_DIR", &self.data)
            .env("SPOTLIGHT_DEV_EXCLUSIONS_CONFIG", &self.config)
            .env(
                "SPOTLIGHT_DEV_EXCLUSIONS_LAUNCH_AGENTS_DIR",
                &self.launch_agents,
            )
            .env("RUST_LOG", "warn");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn run_ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "command {:?} failed\nstdout:\n{}\nstderr:\n{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.run_ok(args);
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn make_project(root: &Path, name: &str, artifact: &str) -> PathBuf {
    let project = root.join(name);
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("src/main.js"), "console.log('hello');\n").unwrap();
    let path = project.join(artifact);
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn scan_status_export_mark_and_persistence() {
    let env = TestEnv::new();
    make_project(&env.root, "alpha", "node_modules");
    make_project(&env.root, "beta", "dist");

    let scan = env.run_ok(&["scan"]);
    let text = String::from_utf8_lossy(&scan.stdout);
    assert!(text.contains("new directories: 2"), "{text}");

    let status = env.json(&["status", "--json"]);
    assert_eq!(status["total_directories"], 2);
    assert_eq!(status["pending"], 2);
    assert_eq!(status["by_status"]["pending_manual_exclusion"], 2);

    // A separate process sees the same SQLite state.
    let status_again = env.json(&["status", "--json"]);
    assert_eq!(status_again["total_directories"], 2);

    let list = env.json(&["list", "--json"]);
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert!(list.as_array().unwrap().iter().all(|row| {
        row["path"]
            .as_str()
            .map(|p| p.starts_with(env.root.canonicalize().unwrap().to_str().unwrap()))
            .unwrap_or(false)
    }));

    let export = env.run_ok(&["export"]);
    let export_text = String::from_utf8_lossy(&export.stdout);
    assert!(export_text.contains("Next steps"), "{export_text}");
    let export_path = export_text
        .lines()
        .find(|line| line.contains("spotlight-exclusions-"))
        .map(|line| line.trim())
        .unwrap();
    let content = fs::read_to_string(export_path).unwrap();
    assert!(content.contains("System Settings -> Spotlight -> Search Privacy"));
    assert!(content.contains("node_modules"));
    assert!(content.contains("dist"));

    let state = env.json(&["state", "--json"]);
    assert_eq!(state["table_counts"]["directories"], 2);
    assert_eq!(state["table_counts"]["exports"], 1);

    env.run_ok(&["mark", "--all"]);
    let verified = env.run_ok(&["verify", "--confirm"]);
    assert!(String::from_utf8_lossy(&verified.stdout).contains("Verification outcome: verified"));
    let status = env.json(&["status", "--json"]);
    assert_eq!(status["by_status"]["verified"], 2);
    assert_eq!(status["pending"], 0);

    // Exporting never marks anything verified.
    let export_dir = env.data.join("exports");
    let exported = fs::read_dir(&export_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(exported.len(), 1);
}

#[test]
fn add_and_remove_path_preserve_history() {
    let env = TestEnv::new();
    let second = env._temp.path().join("more-projects");
    fs::create_dir_all(&second).unwrap();
    let nm = make_project(&second, "gamma", "node_modules");

    let add = env.run_ok(&["add-path", second.to_str().unwrap()]);
    assert!(String::from_utf8_lossy(&add.stdout).contains("Added scan root"));
    let status = env.json(&["status", "--json"]);
    assert_eq!(status["roots_configured"], 2);
    assert_eq!(status["pending"], 1);

    let list = env.json(&["list", "--json"]);
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|row| row["path"].as_str() == nm.canonicalize().unwrap().to_str())
    );

    let remove = env.run_ok(&["remove-path", second.to_str().unwrap()]);
    assert!(String::from_utf8_lossy(&remove.stdout).contains("not deleted"));
    let status = env.json(&["status", "--json"]);
    assert_eq!(status["roots_configured"], 1);
    // History remains even though the root is no longer active.
    assert_eq!(status["total_directories"], 1);
    let state = env.json(&["state", "--json"]);
    assert_eq!(state["table_counts"]["scan_roots"], 2);
}

#[test]
fn config_changes_control_dot_directory_scanning() {
    let env = TestEnv::new();
    let next = env.root.join("web/.next");
    fs::create_dir_all(&next).unwrap();

    env.run_ok(&["scan"]);
    let status = env.json(&["status", "--json"]);
    assert_eq!(status["total_directories"], 0);

    env.run_ok(&["config", "set", "rules.scan_dot_directories", "true"]);
    env.run_ok(&["rescan"]);
    let list = env.json(&["list", "--json"]);
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|row| row["path"].as_str() == next.canonicalize().unwrap().to_str())
    );
}

#[test]
fn interrupted_scan_state_is_recovered_on_reopen() {
    let env = TestEnv::new();
    fs::create_dir_all(env.root.join("project/node_modules")).unwrap();
    let db_path = env.data.join("state.sqlite3");

    let mut config = Config::default();
    config.scan.roots = vec![env.root.to_string_lossy().to_string()];
    config.save(&env.config).unwrap();

    let mut db = Db::open_path(&db_path).unwrap();
    db.sync_roots(std::slice::from_ref(&env.root)).unwrap();
    let session = db.start_scan(ScanKind::Initial, 1).unwrap();
    drop(db);

    let db = Db::open_path(&db_path).unwrap();
    let scans = db.recent_scans(1).unwrap();
    assert_eq!(scans[0].id, session.id);
    assert!(scans[0].interrupted);
}

#[cfg(target_os = "macos")]
#[test]
fn daemon_discovers_new_node_modules_and_data_survives_sigterm() {
    let env = TestEnv::new();
    let mut child = env
        .command()
        .arg("daemon")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Wait for the daemon's initial scan and watcher startup.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("daemon did not initialise its database in time");
        }
        if env
            .json(&["status", "--json"])
            .get("last_scan_at")
            .and_then(Value::as_i64)
            .is_some()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(500));

    let project = env.root.join("live-project");
    fs::create_dir_all(project.join("src")).unwrap();
    let nm = project.join("node_modules");
    fs::create_dir_all(&nm).unwrap();
    let nm = nm.canonicalize().unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut discovered = false;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("daemon exited early with {status}");
        }
        let list = env.json(&["list", "--json"]);
        if list
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["path"].as_str() == nm.canonicalize().unwrap().to_str())
        {
            discovered = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    // SAFETY: sending SIGTERM to a child process we spawned is safe.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let status = child.wait().unwrap();
    assert!(status.success(), "daemon did not exit cleanly on SIGTERM");
    assert!(
        discovered,
        "daemon did not discover {} via FSEvents without a full scan",
        nm.display()
    );

    let list = env.json(&["list", "--json"]);
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|row| row["path"].as_str() == nm.canonicalize().unwrap().to_str())
    );

    // Log file exists outside the project tree.
    let logs = fs::read_dir(env.data.join("logs")).unwrap().count();
    assert!(logs > 0);
}
