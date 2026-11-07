use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::ops::Deref;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Dočasný adresář pro jeden test. Po testu se smaže.
struct Scratch(PathBuf);

impl Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn scratch() -> Scratch {
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("cli-tests")
        .join(format!("{}-{id}", std::process::id()));
    fs::create_dir_all(&path).unwrap();
    Scratch(path)
}

/// Unikátní cesta v systémovém dočasném adresáři. Kanonizovaná, protože na macOS
/// vedou `/tmp` i `/var` přes symlink a agent-log symlinky v cestě odmítá.
fn outside_path(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("agent-log-{prefix}-{}-{nanos}", std::process::id()))
}

fn agent(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-log"));
    command.current_dir(dir).env_remove("AGENT_LOG_FILE");
    command
}

fn output_text(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Počká na proces nejvýš `limit`. Když nedoběhne, zabije ho a test selže.
fn wait_with_timeout(mut child: Child, limit: Duration, what: &str) -> Output {
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{what}: proces nedoběhl do {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn add_and_list_roundtrip_uses_rfc3339_and_mode_0600() {
    let dir = scratch();
    let added = agent(&dir).args(["add", "čtení souboru"]).output().unwrap();
    assert!(added.status.success(), "{added:?}");
    let (stdout, _) = output_text(&added);
    let ts = stdout
        .trim()
        .strip_prefix('[')
        .and_then(|rest| rest.split("] ").next())
        .unwrap();
    assert!(is_rfc3339(ts), "čas nemá být bez pásma: {ts}");

    let log = fs::read_to_string(dir.join("agent.log")).unwrap();
    let value: serde_json::Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
    assert_eq!(value["message"], "čtení souboru");
    assert!(is_rfc3339(value["ts"].as_str().unwrap()));
    assert_eq!(mode(&dir.join("agent.log")), 0o600);

    let listed = agent(&dir).args(["list"]).output().unwrap();
    assert!(listed.status.success(), "{listed:?}");
    assert!(output_text(&listed).0.contains("čtení souboru"));
}

#[test]
fn tightens_existing_world_readable_log() {
    let dir = scratch();
    let path = dir.join("agent.log");
    File::create(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let added = agent(&dir).args(["add", "po opravě"]).output().unwrap();
    assert!(added.status.success(), "{added:?}");
    assert_eq!(mode(&path), 0o600);
}

#[test]
fn rejects_symlink_and_leaves_target_intact() {
    let dir = scratch();
    let secret = dir.join("secret.txt");
    fs::write(&secret, "SECRET").unwrap();
    symlink(&secret, dir.join("link.log")).unwrap();

    for args in [
        vec!["--file", "link.log", "add", "append"],
        vec!["--file", "link.log", "clear"],
    ] {
        let result = agent(&dir).args(&args).output().unwrap();
        assert!(!result.status.success(), "{args:?} {result:?}");
        let (_, stderr) = output_text(&result);
        assert!(stderr.contains("symlink"), "{stderr}");
        assert_eq!(fs::read_to_string(&secret).unwrap(), "SECRET");
    }
}

#[test]
fn rejects_intermediate_symlink() {
    let dir = scratch();
    let outside = scratch();
    symlink(&outside, dir.join("linked")).unwrap();

    let result = agent(&dir)
        .args(["--file", "linked/agent.log", "add", "útěk"])
        .output()
        .unwrap();
    assert!(!result.status.success(), "{result:?}");
    assert!(output_text(&result).1.contains("symlink"));
    assert!(!outside.join("agent.log").exists());
}

#[test]
fn rejects_hard_link_and_leaves_target_intact() {
    let dir = scratch();
    let victim = dir.join("victim.txt");
    fs::write(&victim, "DATA").unwrap();
    fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).unwrap();
    fs::hard_link(&victim, dir.join("agent.log")).unwrap();

    for args in [vec!["add", "x"], vec!["clear"], vec!["list"]] {
        let result = agent(&dir).args(&args).output().unwrap();
        assert!(!result.status.success(), "{args:?} {result:?}");
        let (_, stderr) = output_text(&result);
        assert!(stderr.contains("hard link"), "{args:?} {stderr}");
    }
    assert_eq!(fs::read_to_string(&victim).unwrap(), "DATA");
    assert_eq!(mode(&victim), 0o644);
}

#[test]
fn fifo_is_rejected_without_hanging() {
    let dir = scratch();
    let status = Command::new("mkfifo")
        .arg(dir.join("pipe.log"))
        .status()
        .unwrap();
    assert!(status.success());

    for args in [
        vec!["--file", "pipe.log", "list"],
        vec!["--file", "pipe.log", "add", "x"],
        vec!["--file", "pipe.log", "clear"],
    ] {
        let child = agent(&dir)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = wait_with_timeout(child, Duration::from_secs(5), "FIFO");
        assert!(!output.status.success(), "{args:?} {output:?}");
        let (_, stderr) = output_text(&output);
        assert!(stderr.contains("obyčejný soubor"), "{args:?} {stderr}");
    }
}

#[test]
fn rejects_path_outside_workspace_without_flag() {
    let dir = scratch();
    let inner = dir.join("inner");
    fs::create_dir(&inner).unwrap();
    let escaped = dir.join("secret.log");

    let result = agent(&inner)
        .args(["--file", "../secret.log", "add", "ne"])
        .output()
        .unwrap();
    assert!(!result.status.success(), "{result:?}");
    assert!(output_text(&result).1.contains("--allow-outside"));
    assert!(!escaped.exists());

    let outside = outside_path("outside");
    let result = agent(&dir)
        .args(["--file", outside.to_str().unwrap(), "clear"])
        .output()
        .unwrap();
    assert!(!result.status.success(), "{result:?}");
    assert!(output_text(&result).1.contains("--allow-outside"));
    assert!(!outside.exists());
}

#[test]
fn allow_outside_writes_but_still_refuses_symlink() {
    let dir = scratch();
    let outside = outside_path("ok");
    let result = agent(&dir)
        .args([
            "--allow-outside",
            "--file",
            outside.to_str().unwrap(),
            "add",
            "mimo",
        ])
        .output()
        .unwrap();
    let written = fs::read_to_string(&outside);
    let _ = fs::remove_file(&outside);
    assert!(result.status.success(), "{result:?}");
    assert!(written.unwrap().contains("mimo"));

    let secret = dir.join("secret.txt");
    fs::write(&secret, "SECRET").unwrap();
    symlink(&secret, dir.join("link.log")).unwrap();
    let result = agent(&dir)
        .args(["--allow-outside", "--file", "link.log", "clear"])
        .output()
        .unwrap();
    assert!(!result.status.success(), "{result:?}");
    assert_eq!(fs::read_to_string(&secret).unwrap(), "SECRET");
}

#[test]
fn list_sanitizes_terminal_escapes() {
    let dir = scratch();
    let entry = agent_log::Entry {
        ts: "2026-01-01T00:00:00Z\nFAKE\u{1b}]0;title\u{1b}\\".to_owned(),
        message: "line\nnext\u{1b}[31mRED".to_owned(),
    };
    let mut file = File::create(dir.join("agent.log")).unwrap();
    writeln!(file, "{}", serde_json::to_string(&entry).unwrap()).unwrap();

    let listed = agent(&dir).args(["list"]).output().unwrap();
    assert!(listed.status.success(), "{listed:?}");
    let (stdout, _) = output_text(&listed);
    assert_eq!(stdout.lines().count(), 1, "{stdout:?}");
    assert!(!stdout.as_bytes().contains(&0x1b));
    assert!(stdout.contains("FAKE"), "{stdout}");
    assert!(stdout.contains('\u{FFFD}'));
}

#[test]
fn corrupt_lines_fail_and_missing_file_is_not_empty() {
    let dir = scratch();
    let missing = agent(&dir).args(["list"]).output().unwrap();
    assert!(!missing.status.success(), "{missing:?}");
    let (stdout, stderr) = output_text(&missing);
    assert!(stderr.contains("log neexistuje"), "{stderr}");
    assert!(!stdout.contains("prázdný"));

    File::create(dir.join("agent.log")).unwrap();
    let empty = agent(&dir).args(["list"]).output().unwrap();
    assert!(empty.status.success(), "{empty:?}");
    assert!(output_text(&empty).0.contains("Log je prázdný."));

    let mut file = File::create(dir.join("agent.log")).unwrap();
    writeln!(file, r#"{{"ts":"2026-01-01T00:00:00Z","message":"good"}}"#).unwrap();
    writeln!(file, "not-json").unwrap();
    writeln!(file, r#"{{"message":"missing ts"}}"#).unwrap();
    drop(file);

    let listed = agent(&dir).args(["list"]).output().unwrap();
    assert!(!listed.status.success(), "{listed:?}");
    let (stdout, stderr) = output_text(&listed);
    assert!(stdout.contains("good"), "{stdout}");
    assert!(stderr.contains("poškozen"), "{stderr}");
}

#[test]
fn appends_after_line_without_trailing_newline() {
    let dir = scratch();
    fs::write(
        dir.join("agent.log"),
        r#"{"ts":"2026-01-01T00:00:00Z","message":"bez konce"}"#,
    )
    .unwrap();

    let added = agent(&dir).args(["add", "nový"]).output().unwrap();
    assert!(added.status.success(), "{added:?}");
    let listed = agent(&dir).args(["list"]).output().unwrap();
    assert!(listed.status.success(), "{listed:?}");
    let (stdout, _) = output_text(&listed);
    assert!(
        stdout.contains("bez konce") && stdout.contains("nový"),
        "{stdout}"
    );
}

#[test]
fn max_control_message_roundtrips() {
    let dir = scratch();
    let message = "\u{1}".repeat(4096);
    let added = agent(&dir).args(["add", &message]).output().unwrap();
    assert!(added.status.success(), "{added:?}");
    let listed = agent(&dir).args(["list"]).output().unwrap();
    assert!(listed.status.success(), "{listed:?}");
    let (stdout, stderr) = output_text(&listed);
    assert!(stderr.is_empty(), "{stderr}");
    assert_eq!(stdout.chars().filter(|c| *c == '\u{FFFD}').count(), 4096);
}

#[test]
fn rejects_oversized_message_without_creating_log() {
    let dir = scratch();
    let message = "x".repeat(4097);
    let result = agent(&dir).args(["add", &message]).output().unwrap();
    assert!(!result.status.success(), "{result:?}");
    assert!(output_text(&result).1.contains("4096"));
    assert!(!dir.join("agent.log").exists());
}

#[test]
fn limit_shows_last_entries_only() {
    let dir = scratch();
    for message in ["jedna", "dva", "tri"] {
        let result = agent(&dir).args(["add", message]).output().unwrap();
        assert!(result.status.success(), "{result:?}");
    }
    let listed = agent(&dir).args(["list", "--limit", "2"]).output().unwrap();
    assert!(listed.status.success(), "{listed:?}");
    let (stdout, _) = output_text(&listed);
    assert!(!stdout.contains("jedna"));
    assert!(stdout.contains("dva") && stdout.contains("tri"));
}

#[test]
fn directory_target_is_rejected() {
    let dir = scratch();
    fs::create_dir(dir.join("not-a-file")).unwrap();
    for args in [
        vec!["--file", "not-a-file", "add", "x"],
        vec!["--file", "not-a-file", "list"],
    ] {
        let result = agent(&dir).args(&args).output().unwrap();
        assert!(!result.status.success(), "{args:?} {result:?}");
        assert!(output_text(&result).1.contains("obyčejný soubor"));
    }
}

#[test]
fn env_path_is_confined() {
    let dir = scratch();
    let outside = outside_path("env");
    let result = agent(&dir)
        .env("AGENT_LOG_FILE", &outside)
        .args(["add", "z prostředí"])
        .output()
        .unwrap();
    assert!(!result.status.success(), "{result:?}");
    assert!(!outside.exists());

    let result = agent(&dir)
        .env("AGENT_LOG_FILE", "from-env.log")
        .args(["add", "uvnitř"])
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert!(dir.join("from-env.log").is_file());
}

#[test]
fn concurrent_adds_stay_valid_json_lines() {
    let dir = scratch();
    let mut handles = Vec::new();
    for i in 0..8 {
        let path = dir.to_path_buf();
        handles.push(std::thread::spawn(move || {
            agent(&path)
                .args(["add", &format!("msg-{i}")])
                .output()
                .unwrap()
        }));
    }
    for handle in handles {
        let output = handle.join().unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    let raw = fs::read_to_string(dir.join("agent.log")).unwrap();
    let mut count = 0;
    for line in raw.lines() {
        let value: serde_json::Value = serde_json::from_str(line).expect(line);
        assert!(value["message"].as_str().unwrap().starts_with("msg-"));
        count += 1;
    }
    assert_eq!(count, 8);
}

#[test]
fn list_does_not_block_add_while_output_is_unread() {
    let dir = scratch();
    let entry = agent_log::Entry {
        ts: "2026-01-01T00:00:00Z".to_owned(),
        message: "x".repeat(200),
    };
    let line = serde_json::to_string(&entry).unwrap();
    let mut file = File::create(dir.join("agent.log")).unwrap();
    // Zhruba 450 kB, víc než se vejde do bufferu roury.
    for _ in 0..2000 {
        writeln!(file, "{line}").unwrap();
    }
    drop(file);

    let mut list = agent(&dir)
        .arg("list")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // První řádek výpisu znamená, že `list` už soubor přečetl. Zbytek nikdo nečte,
    // takže `list` zůstane viset na zápisu do roury jako u otevřeného pageru.
    let mut first = String::new();
    BufReader::new(list.stdout.as_mut().unwrap())
        .read_line(&mut first)
        .unwrap();
    assert!(first.contains("xxx"), "{first:?}");

    let add = agent(&dir)
        .args(["add", "během výpisu"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = wait_with_timeout(add, Duration::from_secs(5), "add během list");
    let _ = list.kill();
    let _ = list.wait();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn closed_stdout_is_not_an_error() {
    let dir = scratch();
    for args in [vec!["add", "x"], vec!["list"], vec!["clear"]] {
        let mut child = agent(&dir)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stdout.take());
        let output = wait_with_timeout(child, Duration::from_secs(5), "zavřený stdout");
        assert!(output.status.success(), "{args:?} {output:?}");
        let (_, stderr) = output_text(&output);
        assert!(stderr.is_empty(), "{args:?} {stderr}");
    }
}

#[test]
fn clear_creates_empty_private_file() {
    let dir = scratch();
    let result = agent(&dir).args(["clear"]).output().unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(fs::read_to_string(dir.join("agent.log")).unwrap(), "");
    assert_eq!(mode(&dir.join("agent.log")), 0o600);
}

#[test]
fn newline_in_message_stays_one_json_line_and_one_printed_line() {
    let dir = scratch();
    let result = agent(&dir)
        .args(["add", "první\ndruhý\u{1b}[0m"])
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    let raw = fs::read(dir.join("agent.log")).unwrap();
    assert_eq!(raw.iter().filter(|byte| **byte == b'\n').count(), 1);
    assert!(!raw.contains(&0x1b));
    let (stdout, _) = output_text(&result);
    assert_eq!(stdout.lines().count(), 1);
    assert!(!stdout.as_bytes().contains(&0x1b));
}

fn is_rfc3339(value: &str) -> bool {
    let Some((date, rest)) = value.split_once('T') else {
        return false;
    };
    let Some((time, zone)) = split_zone(rest) else {
        return false;
    };
    date.len() == 10
        && time.len() == 8
        && (zone == "Z" || (zone.len() == 6 && (zone.starts_with('+') || zone.starts_with('-'))))
}

fn split_zone(value: &str) -> Option<(&str, &str)> {
    if let Some(time) = value.strip_suffix('Z') {
        return Some((time, "Z"));
    }
    let split_at = value.rfind(['+', '-'])?;
    Some(value.split_at(split_at))
}
