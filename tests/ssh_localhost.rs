#![cfg(all(feature = "ssh", target_os = "linux"))]
//! Only scripts/test-ssh.sh supplies this disposable, loopback-only fixture.
use starcom::{core, inspect, sftp, ssh};
use std::{env, fs, io, path, process, thread, time};

fn root() -> path::PathBuf {
    path::PathBuf::from(env::var_os("STARCOM_TEST_DIR").expect("run scripts/test-ssh.sh"))
}

fn options() -> ssh::Options {
    ssh::Options {
        host: "127.0.0.1".to_owned(),
        port: fs::read_to_string(root().join("port"))
            .unwrap()
            .trim()
            .parse()
            .unwrap(),
        user: env::var("STARCOM_TEST_USER").unwrap(),
        known_hosts: root().join("known_hosts"),
        authentication: ssh::Authentication::identity(root().join("id_ed25519")),
        host_key_alias: None,
        timeout: time::Duration::from_secs(5),
        jumps: Vec::new(),
    }
}

fn tmux(args: &[&str]) -> String {
    let result = process::Command::new("timeout")
        .args(["5s", "tmux", "-S"])
        .arg(root().join("tmux.sock"))
        .args(args)
        .output()
        .unwrap();
    assert!(result.status.success(), "tmux: {:?}", result.stderr);
    String::from_utf8(result.stdout).unwrap()
}

#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn inspect_existing_primary_and_alternate_screens_without_mutation() {
    let before = tmux(&[
        "list-panes",
        "-s",
        "-t",
        "starcom",
        "-F",
        "#{pane_id} #{pane_width} #{pane_height} #{pane_pid}",
    ]);
    let environment = tmux(&["show-environment", "-t", "starcom", "STARCOM_TEST_ENV"]);
    let session = core::SessionName::new("starcom").unwrap();
    let socket = root().join("tmux.sock");
    let mut inspector = inspect::Inspector::attach(&options(), &session, socket.to_str()).unwrap();
    let observed = inspector.observe(20).unwrap();
    assert_eq!(observed.captures.len(), 2);
    assert!(
        observed
            .captures
            .iter()
            .any(|capture| capture.pane.alternate_screen)
    );
    assert!(observed.fingerprint.starts_with("SHA256:"));
    let rows: Vec<_> = observed
        .captures
        .iter()
        .flat_map(|capture| &capture.escaped_rows)
        .collect();
    assert!(rows.iter().any(|row| row.contains("STARCOM_PRIMARY_READY")));
    assert!(
        rows.iter()
            .any(|row| row.contains("STARCOM_ALTERNATE_READY"))
    );
    drop(inspector);
    assert_eq!(
        before,
        tmux(&[
            "list-panes",
            "-s",
            "-t",
            "starcom",
            "-F",
            "#{pane_id} #{pane_width} #{pane_height} #{pane_pid}"
        ])
    );
    assert_eq!(
        environment,
        tmux(&["show-environment", "-t", "starcom", "STARCOM_TEST_ENV"])
    );
    let deadline = time::Instant::now() + time::Duration::from_secs(5);
    while !tmux(&["list-clients", "-F", "#{client_pid}"])
        .trim()
        .is_empty()
    {
        assert!(
            time::Instant::now() < deadline,
            "inspection client did not detach"
        );
        thread::sleep(time::Duration::from_millis(20));
    }
    tmux(&["has-session", "-t", "=starcom"]);
}

#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn trust_failures_precede_authentication_and_do_not_rewrite_known_hosts() {
    for (name, kind) in [
        ("known_hosts.empty", ssh::Kind::UnknownHostKey),
        ("known_hosts.bad", ssh::Kind::ChangedHostKey),
        ("known_hosts.revoked", ssh::Kind::Configuration),
    ] {
        let mut options = options();
        options.known_hosts = root().join(name);
        options.authentication = ssh::Authentication::identity(root().join("DOES_NOT_EXIST"));
        let before = fs::read(&options.known_hosts).unwrap();
        let error = ssh::Connection::connect(&options)
            .err()
            .expect("unsafe connection succeeded");
        assert_eq!(error.kind, kind, "{error}");
        assert_eq!(before, fs::read(&options.known_hosts).unwrap());
    }
}

#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn hashed_known_hosts_and_explicit_agent_authentication_work() {
    let mut options = options();
    options.known_hosts = root().join("known_hosts.hashed");
    drop(ssh::Connection::connect(&options).unwrap());
    options.authentication = ssh::Authentication::agent();
    drop(ssh::Connection::connect(&options).unwrap());
}

#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn missing_sessions_and_servers_are_not_created() {
    let socket = root().join("tmux.sock");
    let missing = core::SessionName::new("missing-starcom-session").unwrap();
    let result = inspect::Inspector::attach(&options(), &missing, socket.to_str())
        .and_then(|mut inspector| inspector.observe(0));
    assert!(result.is_err());
    let absent_socket = root().join("absent.sock");
    let result = inspect::Inspector::attach(&options(), &missing, absent_socket.to_str())
        .and_then(|mut inspector| inspector.observe(0));
    assert!(result.is_err());
    assert!(
        !absent_socket.exists(),
        "inspection created a new tmux server"
    );
    assert_eq!(
        tmux(&["list-sessions", "-F", "#{session_name}"]).trim(),
        "starcom"
    );
}

#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn stdout_and_stderr_remain_separate_without_a_pty() {
    let mut channel = ssh::Connection::connect(&options())
        .unwrap()
        .exec("printf output; printf diagnostic >&2; test ! -t 0")
        .unwrap();
    let deadline = time::Instant::now() + time::Duration::from_secs(5);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        assert!(time::Instant::now() < deadline);
        let mut progress = false;
        match channel.read_stderr(&mut buffer) {
            Ok(count) => {
                stderr.extend_from_slice(&buffer[..count]);
                progress |= count != 0;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("stderr: {error}"),
        }
        match io::Read::read(&mut channel, &mut buffer) {
            Ok(count) => {
                stdout.extend_from_slice(&buffer[..count]);
                progress |= count != 0;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("stdout: {error}"),
        }
        if channel.eof() {
            break;
        }
        if !progress {
            channel.wait(deadline).unwrap();
        }
    }
    assert_eq!(stdout, b"output");
    assert_eq!(stderr, b"diagnostic");
}

#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn stalled_channel_has_a_deadline() {
    let mut channel = ssh::Connection::connect(&options())
        .unwrap()
        .exec("exec sleep 2")
        .unwrap();
    let deadline = time::Instant::now() + time::Duration::from_millis(100);
    let mut buffer = [0; 16];
    loop {
        match io::Read::read(&mut channel, &mut buffer) {
            Ok(0) => panic!("sleep exited before timeout"),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("read: {error}"),
        }
        if let Err(error) = channel.wait(deadline) {
            assert_eq!(error.kind, ssh::Kind::Timeout);
            break;
        }
    }
}

#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn inspection_cli_exercises_the_real_embedded_transport() {
    let options = options();
    let result = process::Command::new(env!("CARGO_BIN_EXE_starcom-inspect"))
        .args([
            "--host",
            "127.0.0.1",
            "--user",
            &options.user,
            "--session",
            "starcom",
            "--port",
            &options.port.to_string(),
        ])
        .arg("--known-hosts")
        .arg(&options.known_hosts)
        .arg("--identity")
        .arg(root().join("id_ed25519"))
        .arg("--socket")
        .arg(root().join("tmux.sock"))
        .args(["--history", "0", "--timeout", "5"])
        .output()
        .unwrap();
    assert!(result.status.success(), "CLI stderr: {:?}", result.stderr);
    let stdout = String::from_utf8(result.stdout).unwrap();
    assert!(stdout.contains("STARCOM_PRIMARY_READY"));
    assert!(stdout.contains("STARCOM_ALTERNATE_READY"));
    assert!(stdout.contains("Read-only observations"));
    assert!(!stdout.contains('\x1b'));
}

#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn live_models_restore_existing_primary_and_alternate_screens() {
    let name = core::SessionName::new("starcom").unwrap();
    let socket = root().join("tmux.sock");
    let session =
        starcom::session::Session::attach(&options(), &name, socket.to_str(), 20).unwrap();
    assert_eq!(session.view().panes().len(), 2);
    let rows: Vec<_> = session
        .view()
        .panes()
        .values()
        .flat_map(|pane| pane.terminal.screen_lines())
        .collect();
    assert!(
        rows.iter()
            .any(|line| line.contains("STARCOM_PRIMARY_READY"))
    );
    assert!(
        rows.iter()
            .any(|line| line.contains("STARCOM_ALTERNATE_READY"))
    );
    assert!(
        session
            .view()
            .panes()
            .values()
            .any(|pane| pane.terminal.is_alternate_screen())
    );
}

struct LiveFixture {
    name: String,
}

impl Drop for LiveFixture {
    fn drop(&mut self) {
        let _ = process::Command::new("timeout")
            .args(["5s", "tmux", "-S"])
            .arg(root().join("tmux.sock"))
            .args(["kill-session", "-t", &format!("={}", self.name)])
            .output();
    }
}

fn wait_for(mut predicate: impl FnMut() -> bool) {
    let deadline = time::Instant::now() + time::Duration::from_secs(5);
    while !predicate() {
        assert!(
            time::Instant::now() < deadline,
            "fixture condition timed out"
        );
        thread::sleep(time::Duration::from_millis(10));
    }
}

#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn snapshot_pending_bytes_resume_once_and_resize_rebuilds_models() {
    let fixture = LiveFixture {
        name: "starcom-sync-fragment".to_owned(),
    };
    let script = root().join("pending.py");
    let advance = root().join("advance");
    let _ = fs::remove_file(&advance);
    fs::write(
        &script,
        concat!(
            "import pathlib, sys, time\n",
            "root = pathlib.Path(__file__).parent\n",
            "sys.stdout.write('\\x1b[2J\\x1b[HBASE\\x1b[2')\n",
            "sys.stdout.flush()\n",
            "while not (root / 'advance').exists(): time.sleep(0.01)\n",
            "sys.stdout.write('K\\rZ')\n",
            "sys.stdout.flush()\n",
            "time.sleep(60)\n",
        ),
    )
    .unwrap();
    tmux(&[
        "new-session",
        "-d",
        "-s",
        &fixture.name,
        "-x",
        "40",
        "-y",
        "8",
        &format!("exec python3 '{}'", script.display()),
    ]);
    let pane = tmux(&["list-panes", "-t", &fixture.name, "-F", "#{pane_id}"])
        .trim()
        .to_owned();
    let pid = tmux(&["display-message", "-p", "-t", &pane, "#{pane_pid}"]);
    wait_for(|| tmux(&["capture-pane", "-p", "-P", "-C", "-t", &pane]).contains("\\033[2"));
    let name = core::SessionName::new(&fixture.name).unwrap();
    let socket = root().join("tmux.sock");
    let mut session =
        starcom::session::Session::attach(&options(), &name, socket.to_str(), 20).unwrap();
    let id = *session.view().panes().keys().next().unwrap();
    assert_eq!(
        session.view().panes()[&id].terminal.screen_lines()[0],
        "BASE"
    );
    fs::write(advance, b"next").unwrap();
    let deadline = time::Instant::now() + time::Duration::from_secs(5);
    while session.view().panes()[&id].terminal.screen_lines()[0] != "Z" {
        assert!(
            time::Instant::now() < deadline,
            "pending escape was lost or duplicated"
        );
        session.poll(deadline).unwrap();
        assert_eq!(session.view().status(), starcom::snapshot::Status::Watching);
    }
    assert_eq!(
        tmux(&["capture-pane", "-p", "-t", &pane]).lines().next(),
        Some("Z")
    );
    tmux(&["resize-window", "-t", &fixture.name, "-x", "60", "-y", "10"]);
    let deadline = time::Instant::now() + time::Duration::from_secs(5);
    while session.view().status() == starcom::snapshot::Status::Watching {
        assert!(
            time::Instant::now() < deadline,
            "resize notification not observed"
        );
        session.poll(deadline).unwrap();
    }
    assert_eq!(
        session.view().status(),
        starcom::snapshot::Status::NeedsResync
    );
    session.synchronize().unwrap();
    assert_eq!(
        session.view().panes()[&id].terminal.size(),
        core::Size::new(60, 10).unwrap()
    );
    assert_eq!(session.view().panes()[&id].terminal.screen_lines()[0], "Z");
    assert_eq!(
        pid,
        tmux(&["display-message", "-p", "-t", &pane, "#{pane_pid}"])
    );
    tmux(&["detach-client", "-s", &fixture.name]);
    let deadline = time::Instant::now() + time::Duration::from_secs(5);
    while session.view().status() != starcom::snapshot::Status::Disconnected {
        assert!(time::Instant::now() < deadline, "disconnect not observed");
        let _ = session.poll(deadline);
    }
    assert_eq!(session.view().panes()[&id].terminal.screen_lines()[0], "Z");
    assert_eq!(
        pid,
        tmux(&["display-message", "-p", "-t", &pane, "#{pane_pid}"])
    );
}

#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn watch_cli_uses_embedded_transport_and_escapes_its_final_screens() {
    let options = options();
    let result = process::Command::new("timeout")
        .arg("15s")
        .arg(env!("CARGO_BIN_EXE_starcom-inspect"))
        .args([
            "--host",
            "127.0.0.1",
            "--user",
            &options.user,
            "--session",
            "starcom",
            "--port",
            &options.port.to_string(),
        ])
        .arg("--known-hosts")
        .arg(&options.known_hosts)
        .arg("--identity")
        .arg(root().join("id_ed25519"))
        .arg("--socket")
        .arg(root().join("tmux.sock"))
        .args(["--history", "20", "--timeout", "5", "--watch", "1"])
        .output()
        .unwrap();
    assert!(result.status.success(), "watch stderr: {:?}", result.stderr);
    let stdout = String::from_utf8(result.stdout).unwrap();
    assert!(stdout.contains("STARCOM_PRIMARY_READY"));
    assert!(stdout.contains("STARCOM_ALTERNATE_READY"));
    assert!(stdout.contains("Read-only live models"));
    assert!(!stdout.contains('\x1b'));
}

#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn continuous_output_crosses_snapshot_boundary_without_gaps_or_duplicates() {
    let fixture = LiveFixture {
        name: "starcom-sync-stream".to_owned(),
    };
    let script = root().join("continuous.py");
    let ready = root().join("continuous-ready");
    let _ = fs::remove_file(&ready);
    fs::write(
        &script,
        concat!(
            "import pathlib, sys, time\n",
            "root = pathlib.Path(__file__).parent\n",
            "sys.stdout.write('\\x1b[2J\\x1b[H')\n",
            "for i in range(200):\n",
            "    sys.stdout.write(f'record-{i:04d}\\r\\n')\n",
            "    sys.stdout.flush()\n",
            "    if i == 20: (root / 'continuous-ready').write_text('ready')\n",
            "    time.sleep(0.003)\n",
            "sys.stdout.write('END\\r\\n')\n",
            "sys.stdout.flush()\n",
            "time.sleep(60)\n",
        ),
    )
    .unwrap();
    tmux(&[
        "new-session",
        "-d",
        "-s",
        &fixture.name,
        "-x",
        "40",
        "-y",
        "8",
        &format!("exec python3 '{}'", script.display()),
    ]);
    wait_for(|| ready.exists());
    let name = core::SessionName::new(&fixture.name).unwrap();
    let socket = root().join("tmux.sock");
    let mut session =
        starcom::session::Session::attach(&options(), &name, socket.to_str(), 1000).unwrap();
    let id = *session.view().panes().keys().next().unwrap();
    let deadline = time::Instant::now() + time::Duration::from_secs(5);
    while !session.view().panes()[&id]
        .terminal
        .screen_lines()
        .iter()
        .any(|line| line == "END")
    {
        assert!(
            time::Instant::now() < deadline,
            "live output did not finish"
        );
        session.poll(deadline).unwrap();
        assert_eq!(session.view().status(), starcom::snapshot::Status::Watching);
    }
    let terminal = &session.view().panes()[&id].terminal;
    let history = alacritty_terminal::grid::Dimensions::history_size(terminal.model().grid());
    let mut records = Vec::new();
    for row in -(history as i32)..terminal.size().rows() as i32 {
        let text = terminal.model().bounds_to_string(
            alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(row),
                alacritty_terminal::index::Column(0),
            ),
            alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(row),
                alacritty_terminal::index::Column(terminal.size().columns() - 1),
            ),
        );
        let text = text.trim();
        if text.starts_with("record-") {
            records.push(text.to_owned());
        }
    }
    let expected: Vec<_> = (0..200).map(|index| format!("record-{index:04}")).collect();
    assert_eq!(
        records, expected,
        "snapshot/live cut lost or duplicated records"
    );
}

#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn snapshot_refuses_yielding_user_hooks_without_removing_them() {
    let fixture = LiveFixture {
        name: "starcom-sync-hooks".to_owned(),
    };
    let marker = root().join("hook-fired");
    let _ = fs::remove_file(&marker);
    tmux(&[
        "new-session",
        "-d",
        "-s",
        &fixture.name,
        "-x",
        "40",
        "-y",
        "8",
        "exec sleep 60",
    ]);
    tmux(&[
        "set-hook",
        "-t",
        &fixture.name,
        "after-capture-pane",
        &format!("run-shell 'touch {}; sleep 1'", marker.display()),
    ]);
    let before = tmux(&["show-hooks", "-t", &fixture.name]);
    let name = core::SessionName::new(&fixture.name).unwrap();
    let socket = root().join("tmux.sock");
    let result = starcom::session::Session::attach(&options(), &name, socket.to_str(), 20);
    let error = match result {
        Ok(_) => panic!("unsafe capture hook was accepted"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("after-capture-pane"),
        "unexpected error: {error:#}"
    );
    assert_eq!(before, tmux(&["show-hooks", "-t", &fixture.name]));
    assert!(!marker.exists(), "capture ran before rejecting the hook");
}

/// M4: asking what exists must never bring a tmux server into existence, and
/// creating one must be a separate, deliberate act that does not attach.
#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn discovery_never_starts_a_server_and_creation_is_explicit() {
    use starcom::sessions;

    let socket = root().join("tmux.sock");
    let listed = sessions::list(&options(), socket.to_str()).unwrap();
    assert!(
        listed.iter().any(|summary| summary.name == "starcom"),
        "the fixture session was not listed: {listed:?}"
    );
    for summary in &listed {
        // A listed name must be one the attach path would accept.
        assert!(core::SessionName::new(summary.name.clone()).is_ok());
    }

    // A socket with no server must report that, and must still not exist after.
    let absent = root().join("discovery-absent.sock");
    assert!(!absent.exists());
    let error = format!(
        "{:#}",
        sessions::list(&options(), absent.to_str()).unwrap_err()
    );
    // tmux words this differently for a default socket and an explicit -S path.
    assert!(
        error.contains("no server running") || error.contains("error connecting to"),
        "unexpected error: {error}"
    );
    assert!(
        !absent.exists(),
        "listing sessions started a tmux server on a socket that had none"
    );

    // Creating is the one path allowed to start a server, and it does not attach.
    let created = core::SessionName::new("starcom-created").unwrap();
    sessions::create(
        &options(),
        absent.to_str(),
        &created,
        core::Size::new(90, 25).unwrap(),
    )
    .unwrap();
    assert!(absent.exists(), "creation did not start the server");
    let listed = sessions::list(&options(), absent.to_str()).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "starcom-created");
    assert_eq!(
        listed[0].attached, 0,
        "creating a session must not attach to it"
    );

    // The fixture's own server is untouched by all of this.
    assert_eq!(
        tmux(&["list-sessions", "-F", "#{session_name}"]).trim(),
        "starcom"
    );
    let _ = process::Command::new("tmux")
        .arg("-S")
        .arg(&absent)
        .arg("kill-server")
        .status();
}

/// SFTP upload writes a file the shell can read. The fixture sshd must offer
/// the sftp subsystem; scripts/test-ssh.sh adds it when sftp-server exists.
#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn sftp_put_writes_a_file_the_shell_can_read() {
    let name = format!("starcom-sftp-{}.txt", process::id());
    let src = env::temp_dir().join(&name);
    fs::write(&src, b"starcom-sftp-payload\n").unwrap();
    struct Cleanup(path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _cleanup = Cleanup(src.clone());

    let mut opts = options();
    opts.timeout = time::Duration::from_secs(15);
    opts.jumps.push(options());
    let paths = sftp::put_files(&opts, std::slice::from_ref(&src), |_| {}).unwrap();
    assert_eq!(paths.len(), 1);
    let remote = &paths[0];
    assert!(
        remote.contains("/starcom-") && remote.ends_with(&format!("-{name}")),
        "uploaded path should be a unique temp name: {remote}"
    );
    assert!(
        remote
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b)),
        "uploaded path is not safe to exec: {remote}"
    );

    let mut channel = ssh::Connection::connect(&opts)
        .unwrap()
        .exec(&format!("exec cat {remote}"))
        .unwrap();
    let deadline = time::Instant::now() + time::Duration::from_secs(10);
    let mut got = Vec::new();
    let mut buffer = [0; 256];
    while !channel.eof() {
        assert!(time::Instant::now() < deadline, "cat stalled");
        match io::Read::read(&mut channel, &mut buffer) {
            Ok(0) => break,
            Ok(count) => got.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                channel.wait(deadline).unwrap()
            }
            Err(error) => panic!("cat: {error}"),
        }
    }
    assert_eq!(got, b"starcom-sftp-payload\n");

    let mut rm = ssh::Connection::connect(&opts)
        .unwrap()
        .exec(&format!("exec rm -f {remote}"))
        .unwrap();
    let deadline = time::Instant::now() + time::Duration::from_secs(10);
    while !rm.eof() {
        assert!(time::Instant::now() < deadline, "rm stalled");
        match io::Read::read(&mut rm, &mut buffer) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => rm.wait(deadline).unwrap(),
            Err(error) => panic!("rm: {error}"),
        }
    }
}

/// Sunset can open a `direct-tcpip` channel, and a real OpenSSH server carries
/// data over it. This is the channel `ssh -J` is built on.
#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn a_direct_tcpip_channel_carries_data_through_the_server() {
    use std::{io::Write as _, net, thread};

    // A listener the server will be asked to connect to on our behalf.
    let listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let echo = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut seen = Vec::new();
        let mut buffer = [0; 256];
        while seen.len() < 5 {
            match io::Read::read(&mut stream, &mut buffer) {
                Ok(0) => break,
                Ok(count) => seen.extend_from_slice(&buffer[..count]),
                Err(error) => panic!("forwarded read: {error}"),
            }
        }
        stream.write_all(b"PONG").unwrap();
        stream.flush().unwrap();
        seen
    });

    let mut opts = options();
    // Handshake plus the forwarded open can include a rekey (the fixture
    // uses RekeyLimit 16K). Five seconds is the refusal budget, not this.
    opts.timeout = time::Duration::from_secs(15);
    let mut channel = ssh::Connection::connect(&opts)
        .unwrap()
        .open_forward("127.0.0.1", port)
        .unwrap_or_else(|error| panic!("direct-tcpip channel: {error}"));
    channel.write_all(b"HELLO").unwrap();
    channel.flush().unwrap();
    let deadline = time::Instant::now() + time::Duration::from_secs(10);
    let mut received = Vec::new();
    let mut buffer = [0; 256];
    while received.len() < 4 {
        assert!(time::Instant::now() < deadline, "forward stalled");
        match io::Read::read(&mut channel, &mut buffer) {
            Ok(0) => break,
            Ok(count) => received.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                channel.wait(deadline).unwrap()
            }
            Err(error) => panic!("forward read: {error}"),
        }
    }
    assert_eq!(received, b"PONG", "the server did not carry the reply back");
    assert_eq!(
        echo.join().unwrap(),
        b"HELLO",
        "the server did not carry the request out"
    );

    // Both refusals must be reported promptly rather than waiting out the
    // operation deadline: a destination the server cannot reach, and a server
    // that forbids forwarding at all, as a hardened bastion does.
    let mut refusing = options();
    refusing.port = env::var("STARCOM_NO_FORWARD_PORT")
        .expect("run scripts/test-ssh.sh")
        .parse()
        .unwrap();
    // Port 1 is below the ephemeral range, so nothing can drift onto it.
    for (label, options, target) in [
        ("an unreachable destination", options(), 1),
        ("a server that forbids forwarding", refusing, port),
    ] {
        let started = time::Instant::now();
        let Err(error) = ssh::Connection::connect(&options)
            .unwrap()
            .open_forward("127.0.0.1", target)
        else {
            panic!("{label} must not produce an open forward")
        };
        assert_eq!(error.kind, ssh::Kind::Transport, "{label}: {error}");
        assert!(
            format!("{error}").contains("refused"),
            "{label}: unhelpful message: {error}"
        );
        assert!(
            started.elapsed() < options_timeout(),
            "{label} waited out the deadline instead of reporting: {:?}",
            started.elapsed()
        );
    }
}

/// The fixture's per-operation deadline. A refusal must be reported well inside
/// it, otherwise the caller cannot tell a refusal from an unresponsive host.
fn options_timeout() -> time::Duration {
    options().timeout
}

#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn jump_route_lists_and_attaches_to_the_existing_session() {
    let mut opts = options();
    opts.jumps.push(options());
    let socket = root().join("tmux.sock");
    let names = starcom::sessions::list(&opts, socket.to_str()).unwrap();
    assert!(names.iter().any(|entry| entry.name == "starcom"));
    let session = core::SessionName::new("starcom").unwrap();
    let mut inspector = inspect::Inspector::attach(&opts, &session, socket.to_str()).unwrap();
    let observed = inspector.observe(20).unwrap();
    assert_eq!(observed.captures.len(), 2);
}
