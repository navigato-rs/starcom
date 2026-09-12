#![cfg(all(feature = "ssh", target_os = "linux"))]
//! Rust-native SSH migration and desktop-worker acceptance against an isolated host.

use starcom::ssh;
use std::{env, fs, io, path, time};

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
        strict_host_key_checking: ssh::StrictHostKeyChecking::Yes,
        timeout: time::Duration::from_secs(5),
        jumps: Vec::new(),
    }
}

#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn rustcrypto_identity_algorithms_and_rekeyed_stream_work() {
    for key in ["id_ed25519", "id_rsa", "id_ecdsa"] {
        let mut options = options();
        options.authentication = ssh::Authentication::identity(root().join(key));
        let mut channel = ssh::Connection::connect(&options)
            .unwrap()
            .exec("python3 -c 'import sys; sys.stdout.write(\"0123456789abcdef\" * 8192)' ")
            .unwrap();
        let deadline = time::Instant::now() + time::Duration::from_secs(10);
        let mut received = Vec::new();
        let mut buffer = [0; 8192];
        while !channel.eof() {
            assert!(time::Instant::now() < deadline, "rekeyed stream stalled");
            match io::Read::read(&mut channel, &mut buffer) {
                Ok(count) => received.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    channel.wait(deadline).unwrap()
                }
                Err(error) => panic!("stream: {error}"),
            }
        }
        assert_eq!(received, b"0123456789abcdef".repeat(8192), "{key}");
    }
}

#[cfg(feature = "gui")]
#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn desktop_worker_publishes_live_view_and_rejects_cancelled_requests() {
    use starcom::{core, desktop, session};
    use std::{sync, thread};

    let client = desktop::Client::new(sync::Arc::new(|| {})).unwrap();
    let connection = desktop::Connection {
        options: options(),
        session: core::SessionName::new("starcom").unwrap(),
        socket: Some(root().join("tmux.sock").to_str().unwrap().to_owned()),
        history: 20,
        access: session::Access::Interactive,
        reconnect: false,
    };
    client.connect(connection.clone()).unwrap();
    let deadline = time::Instant::now() + time::Duration::from_secs(10);
    while client.phase() != desktop::Phase::Watching {
        assert!(
            time::Instant::now() < deadline,
            "worker did not attach: {:?}",
            client.phase()
        );
        thread::sleep(time::Duration::from_millis(10));
    }
    client.with_view(|view| {
        let view = view.unwrap();
        assert_eq!(view.panes().len(), 2);
        assert!(view.panes().values().any(|pane| {
            pane.terminal
                .screen_lines()
                .iter()
                .any(|row| row.contains("STARCOM_PRIMARY_READY"))
        }));
    });
    client.disconnect();
    assert_eq!(client.phase(), desktop::Phase::Disconnected);
    client.with_view(|view| assert!(view.is_some(), "disconnect discarded the last view"));
    client.connect(connection).unwrap();
    client.demo().unwrap();
    thread::sleep(time::Duration::from_millis(300));
    assert_eq!(
        client.phase(),
        desktop::Phase::Demo,
        "stale connection replaced demo"
    );
    client.with_view(|view| assert_eq!(view.unwrap().panes().len(), 2));
}

#[cfg(feature = "gui")]
#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn desktop_worker_delivers_input_paste_and_resize_once() {
    use starcom::{core, desktop, input, session};
    use std::{os::unix::fs::PermissionsExt as _, process::Command, sync, thread};

    let test_root = root();
    let socket = test_root.join("tmux.sock");
    let reader = test_root.join("input-reader.sh");
    fs::write(
        &reader,
        r#"#!/bin/sh
stty -echo
printf '\033[2J\033[HINPUT_READY\r\n'
IFS= read -r first
IFS= read -r second
IFS= read -r third
printf 'INPUT_RESULT:<%s>|<%s>|<%s>\r\n' "$first" "$second" "$third"
exec sleep 600
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&reader).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&reader, permissions).unwrap();

    let tmux = || {
        let mut command = Command::new("tmux");
        command.arg("-S").arg(&socket);
        command
    };
    let _ = tmux()
        .args(["kill-session", "-t", "starcom-input"])
        .status();
    assert!(
        tmux()
            .args([
                "new-session",
                "-d",
                "-s",
                "starcom-input",
                "-x",
                "100",
                "-y",
                "30",
            ])
            .arg(&reader)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        tmux()
            .args([
                "split-window",
                "-h",
                "-t",
                "starcom-input",
                "exec sleep 600",
            ])
            .status()
            .unwrap()
            .success()
    );

    let ready_deadline = time::Instant::now() + time::Duration::from_secs(5);
    loop {
        let output = tmux()
            .args(["capture-pane", "-p", "-t", "starcom-input:0.0"])
            .output()
            .unwrap();
        assert!(output.status.success());
        if String::from_utf8_lossy(&output.stdout).contains("INPUT_READY") {
            break;
        }
        assert!(
            time::Instant::now() < ready_deadline,
            "input pane did not start"
        );
        thread::sleep(time::Duration::from_millis(20));
    }

    let pane_output = tmux()
        .args([
            "display-message",
            "-p",
            "-t",
            "starcom-input:0.0",
            "#{pane_id}",
        ])
        .output()
        .unwrap();
    assert!(pane_output.status.success());
    let pane = tmuxctl::PaneId(
        String::from_utf8(pane_output.stdout)
            .unwrap()
            .trim()
            .strip_prefix('%')
            .unwrap()
            .parse()
            .unwrap(),
    );

    let client = desktop::Client::new(sync::Arc::new(|| {})).unwrap();
    let connection = desktop::Connection {
        options: options(),
        session: core::SessionName::new("starcom-input").unwrap(),
        socket: Some(socket.to_str().unwrap().to_owned()),
        history: 50,
        access: session::Access::Interactive,
        reconnect: false,
    };
    client.connect(connection.clone()).unwrap();
    let attach_deadline = time::Instant::now() + time::Duration::from_secs(10);
    while client.phase() != desktop::Phase::Watching {
        assert!(
            time::Instant::now() < attach_deadline,
            "desktop worker did not attach"
        );
        thread::sleep(time::Duration::from_millis(10));
    }

    let target = client.target(pane).expect("interactive pane target");
    client
        .submit(target, input::Action::Bytes(b"hello".to_vec()))
        .unwrap();
    client
        .submit(
            target,
            input::Action::Key(input::Key::Enter, input::Modifiers::default()),
        )
        .unwrap();
    client
        .submit(
            target,
            input::Action::Paste(input::Paste::new("world\n").unwrap()),
        )
        .unwrap();
    client
        .submit(
            target,
            input::Action::Paste(input::Paste::new("--store-token\n").unwrap()),
        )
        .unwrap();

    let result_deadline = time::Instant::now() + time::Duration::from_secs(10);
    loop {
        let present = client.with_view(|view| {
            view.and_then(|view| view.panes().get(&pane))
                .is_some_and(|pane| {
                    pane.terminal
                        .screen_lines()
                        .join("")
                        .contains("INPUT_RESULT:<hello>|<world>|<--store-token>")
                })
        });
        if present {
            break;
        }
        assert!(
            time::Instant::now() < result_deadline,
            "terminal input or paste was lost"
        );
        thread::sleep(time::Duration::from_millis(10));
    }

    let before = client.with_view(|view| view.unwrap().panes()[&pane].state.size.columns());
    let desired = before + 5;
    client.allow_remote_resize(true);
    let target = client.target(pane).expect("target before resize");
    client
        .submit(
            target,
            input::Action::Resize(input::Resize {
                axis: input::Axis::Columns,
                cells: desired,
            }),
        )
        .unwrap();
    let resize_deadline = time::Instant::now() + time::Duration::from_secs(10);
    loop {
        let resized = client.phase() == desktop::Phase::Watching
            && client.with_view(|view| {
                view.and_then(|view| view.panes().get(&pane))
                    .is_some_and(|pane| pane.state.size.columns() == desired)
            });
        if resized {
            break;
        }
        assert!(
            time::Instant::now() < resize_deadline,
            "remote pane was not resynchronized"
        );
        thread::sleep(time::Duration::from_millis(10));
    }

    let capture = tmux()
        .args(["capture-pane", "-p", "-S", "-50", "-t", "starcom-input:0.0"])
        .output()
        .unwrap();
    assert!(capture.status.success());
    assert_eq!(
        String::from_utf8_lossy(&capture.stdout)
            .replace('\n', "")
            .matches("INPUT_RESULT:<hello>|<world>|<--store-token>")
            .count(),
        1,
        "input or paste was duplicated"
    );

    let mut direct = session::Session::attach_with_access(
        &connection.options,
        &connection.session,
        connection.socket.as_deref(),
        connection.history,
        session::Access::Interactive,
    )
    .unwrap();
    assert!(
        tmux()
            .args([
                "set-window-option",
                "-t",
                "starcom-input",
                "synchronize-panes",
                "on",
            ])
            .status()
            .unwrap()
            .success()
    );
    let direct_pane = *direct.view().panes().keys().next().unwrap();
    let error = direct
        .interact(direct_pane, &input::Action::Bytes(b"blocked".to_vec()))
        .unwrap_err();
    assert!(error.to_string().contains("synchronize-panes"));

    client.disconnect();
    let _ = tmux()
        .args(["kill-session", "-t", "starcom-input"])
        .status();
}

/// M3: a transport drop must reattach on its own, publish freshly reconstructed
/// models under a new epoch, and never replay what was in flight when it dropped.
#[cfg(feature = "gui")]
#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn transport_loss_reattaches_without_replaying_input_or_creating_a_session() {
    use starcom::{core, desktop, input, session};
    use std::sync;

    let socket = root().join("tmux.sock");
    let session_name = "starcom-reconnect";
    let _ = tmux()
        .args(["kill-session", "-t", session_name])
        .status()
        .unwrap();
    assert!(
        tmux()
            .args([
                "new-session",
                "-d",
                "-s",
                session_name,
                "-x",
                "80",
                "-y",
                "24",
                "cat > /dev/null",
            ])
            .status()
            .unwrap()
            .success()
    );

    let client = desktop::Client::new(sync::Arc::new(|| {})).unwrap();
    client
        .connect(desktop::Connection {
            options: options(),
            session: core::SessionName::new(session_name).unwrap(),
            socket: Some(socket.to_str().unwrap().to_owned()),
            history: 20,
            access: session::Access::Interactive,
            reconnect: true,
        })
        .unwrap();
    wait_until(20, "worker did not attach", || {
        client.phase() == desktop::Phase::Watching
    });

    let pane = client.with_view(|view| *view.unwrap().panes().keys().next().unwrap());
    let first_generation = client.generation();
    let first_session = client.with_view(|view| view.unwrap().session);

    // Something identifiable on screen, delivered exactly once.
    let target = client.target(pane).expect("live target");
    client
        .submit(target, input::Action::Bytes(b"BEFORE-DROP\r".to_vec()))
        .unwrap();
    wait_until(20, "input never reached the pane", || {
        pane_text(session_name).contains("BEFORE-DROP")
    });

    // Kill the tmux client process behind the control channel. The channel dies
    // with no %exit, which is what an abrupt transport loss looks like.
    let clients = tmux()
        .args(["list-clients", "-t", session_name, "-F", "#{client_pid}"])
        .output()
        .unwrap();
    let control_pid: i32 = String::from_utf8_lossy(&clients.stdout)
        .split_whitespace()
        .next()
        .expect("the control client must be attached")
        .parse()
        .unwrap();
    assert!(
        std::process::Command::new("kill")
            .args(["-9", &control_pid.to_string()])
            .status()
            .unwrap()
            .success()
    );

    // While offline the last view stays readable, but nothing may be queued.
    wait_until(20, "worker did not notice the drop", || {
        matches!(
            client.phase(),
            desktop::Phase::Reconnecting | desktop::Phase::Connecting
        )
    });
    client.with_view(|view| assert!(view.is_some(), "the last view was discarded"));
    assert!(
        client.target(pane).is_none(),
        "input tokens must not be issued while disconnected"
    );
    assert!(
        client
            .submit(target, input::Action::Bytes(b"NEVER-SENT\r".to_vec()))
            .is_err(),
        "input must not be queued for replay after a drop"
    );

    // It comes back by itself, with rebuilt models under a new epoch.
    wait_until(60, "worker did not reattach", || {
        client.phase() == desktop::Phase::Watching
    });
    assert!(
        client.generation() > first_generation,
        "reattachment must publish freshly reconstructed models"
    );
    assert_eq!(
        client.with_view(|view| view.unwrap().session),
        first_session,
        "the same tmux session must be reattached, never a replacement"
    );
    assert!(
        client.retry().is_none(),
        "retry state outlived the recovery"
    );
    let recovered = client.target(pane).expect("input is live again");
    assert_ne!(
        recovered, target,
        "a token from the lost attachment must not compare equal to a new one"
    );

    // Nothing was replayed, and no second session was invented.
    let text = pane_text(session_name);
    assert_eq!(
        text.matches("BEFORE-DROP").count(),
        1,
        "output was duplicated across the reconnect"
    );
    assert!(
        !text.contains("NEVER-SENT"),
        "input rejected while offline was delivered anyway"
    );
    let sessions = tmux()
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&sessions.stdout)
            .lines()
            .filter(|name| name.starts_with(session_name))
            .count(),
        1,
        "reconnection created a replacement session"
    );

    client.disconnect();
    let _ = tmux().args(["kill-session", "-t", session_name]).status();
}

/// A destroyed session is not a transport fault: it must stop and say so rather
/// than reattaching to whatever now answers to that name.
#[cfg(feature = "gui")]
#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn a_destroyed_session_stops_instead_of_reattaching() {
    use starcom::{core, desktop, session};
    use std::{sync, thread};

    let session_name = "starcom-destroyed";
    let _ = tmux()
        .args(["kill-session", "-t", session_name])
        .status()
        .unwrap();
    assert!(
        tmux()
            .args([
                "new-session",
                "-d",
                "-s",
                session_name,
                "-x",
                "80",
                "-y",
                "24",
                "cat > /dev/null",
            ])
            .status()
            .unwrap()
            .success()
    );
    let client = desktop::Client::new(sync::Arc::new(|| {})).unwrap();
    client
        .connect(desktop::Connection {
            options: options(),
            session: core::SessionName::new(session_name).unwrap(),
            socket: Some(root().join("tmux.sock").to_str().unwrap().to_owned()),
            history: 20,
            access: session::Access::ReadOnly,
            reconnect: true,
        })
        .unwrap();
    wait_until(20, "worker did not attach", || {
        client.phase() == desktop::Phase::Watching
    });
    assert!(
        tmux()
            .args(["kill-session", "-t", session_name])
            .status()
            .unwrap()
            .success()
    );
    wait_until(20, "worker kept watching a destroyed session", || {
        matches!(
            client.phase(),
            desktop::Phase::Disconnected | desktop::Phase::Failed
        )
    });
    // Give any (incorrect) retry schedule time to fire before asserting.
    thread::sleep(time::Duration::from_secs(2));
    assert!(
        matches!(
            client.phase(),
            desktop::Phase::Disconnected | desktop::Phase::Failed
        ),
        "a destroyed session must not be reattached automatically: {:?}",
        client.phase()
    );
    assert!(client.retry().is_none(), "a destroyed session was retried");
    client.with_view(|view| assert!(view.is_some(), "the last view was discarded"));
}

/// The disposable fixture socket. Never the contributor's default tmux server.
fn tmux() -> std::process::Command {
    let mut command = std::process::Command::new("tmux");
    command.arg("-S").arg(root().join("tmux.sock"));
    command
}

fn pane_text(session: &str) -> String {
    let capture = tmux()
        .args(["capture-pane", "-p", "-S", "-50", "-t", session])
        .output()
        .unwrap();
    assert!(capture.status.success());
    String::from_utf8_lossy(&capture.stdout).into_owned()
}

fn wait_until(seconds: u64, message: &str, mut ready: impl FnMut() -> bool) {
    let deadline = time::Instant::now() + time::Duration::from_secs(seconds);
    while !ready() {
        assert!(time::Instant::now() < deadline, "{message}");
        std::thread::sleep(time::Duration::from_millis(20));
    }
}

/// A drop in the middle of a paste transaction must never deliver that paste
/// twice. Uncertain delivery is not retried, so at most one copy may appear.
#[cfg(feature = "gui")]
#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn loss_during_a_paste_never_delivers_it_twice() {
    use starcom::{core, desktop, input, session};
    use std::sync;

    let session_name = "starcom-paste-loss";
    reset_session(session_name, "cat > /dev/null");
    let client = desktop::Client::new(sync::Arc::new(|| {})).unwrap();
    client
        .connect(desktop::Connection {
            options: options(),
            session: core::SessionName::new(session_name).unwrap(),
            socket: Some(root().join("tmux.sock").to_str().unwrap().to_owned()),
            history: 40,
            access: session::Access::Interactive,
            reconnect: true,
        })
        .unwrap();

    // Several rounds, killed at different points, so at least one lands inside
    // the multi-command set-buffer/paste-buffer transaction.
    for round in 0..3 {
        wait_until(60, "worker did not attach", || {
            client.phase() == desktop::Phase::Watching
        });
        let pane = client.with_view(|view| *view.unwrap().panes().keys().next().unwrap());
        let Some(target) = client.target(pane) else {
            continue;
        };
        let marker = format!("PASTE-{round}");
        // Large enough to be chunked across many tmux commands.
        let body = format!("{marker}{}\n", "x".repeat(20_000));
        let paste = input::Paste::new(&body).unwrap();
        let submitted = client.submit(target, input::Action::Paste(paste)).is_ok();
        std::thread::sleep(time::Duration::from_millis(round * 12));
        kill_control_client(session_name);
        // A drop inside the multi-command paste transaction is still transport
        // loss, so it must reconnect rather than stop as an unclassified fault.
        wait_until(60, "worker did not recover from a mid-paste drop", || {
            client.phase() == desktop::Phase::Watching
        });
        let seen = pane_text(session_name).matches(&marker).count();
        assert!(
            seen <= 1,
            "round {round}: paste appeared {seen} times after a mid-transaction drop"
        );
        assert!(submitted || seen == 0);
    }
    client.disconnect();
    let _ = tmux().args(["kill-session", "-t", session_name]).status();
}

/// Losing the connection while the remote layout is changing must reconstruct
/// the layout tmux actually has, not the one from before the change.
#[cfg(feature = "gui")]
#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn loss_during_a_remote_layout_change_reconstructs_the_new_layout() {
    use starcom::{core, desktop, session};
    use std::sync;

    let session_name = "starcom-layout-loss";
    reset_session(session_name, "cat > /dev/null");
    let client = desktop::Client::new(sync::Arc::new(|| {})).unwrap();
    client
        .connect(desktop::Connection {
            options: options(),
            session: core::SessionName::new(session_name).unwrap(),
            socket: Some(root().join("tmux.sock").to_str().unwrap().to_owned()),
            history: 20,
            access: session::Access::ReadOnly,
            reconnect: true,
        })
        .unwrap();
    wait_until(30, "worker did not attach", || {
        client.phase() == desktop::Phase::Watching
    });
    assert_eq!(client.with_view(|view| view.unwrap().panes().len()), 1);

    // Change the layout from another client and drop the connection immediately,
    // so the resync and the transport loss race each other.
    assert!(
        tmux()
            .args(["split-window", "-h", "-t", session_name, "cat > /dev/null"])
            .status()
            .unwrap()
            .success()
    );
    kill_control_client(session_name);

    wait_until(60, "worker did not recover the new layout", || {
        client.phase() == desktop::Phase::Watching
            && client.with_view(|view| view.is_some_and(|view| view.panes().len() == 2))
    });
    // The reconstructed geometry must be the server's, not a guess.
    let observed: Vec<_> = client.with_view(|view| {
        view.unwrap()
            .panes()
            .values()
            .map(|pane| (pane.state.left, pane.state.size.columns()))
            .collect()
    });
    let expected = tmux()
        .args([
            "list-panes",
            "-t",
            session_name,
            "-F",
            "#{pane_left} #{pane_width}",
        ])
        .output()
        .unwrap();
    let expected: Vec<_> = String::from_utf8_lossy(&expected.stdout)
        .lines()
        .map(|line| {
            let mut parts = line.split_whitespace();
            (
                parts.next().unwrap().parse::<usize>().unwrap(),
                parts.next().unwrap().parse::<usize>().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        observed, expected,
        "reconstructed geometry is not the server's"
    );

    client.disconnect();
    let _ = tmux().args(["kill-session", "-t", session_name]).status();
}

/// A restarted tmux server numbers its first session `$0` again, so a session id
/// alone cannot tell a replacement from a continuation. Reattaching must say so.
#[cfg(feature = "gui")]
#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn a_restarted_server_is_reported_as_a_replacement() {
    use starcom::{core, desktop, session};
    use std::{process::Command, sync};

    // A private socket: this test kills its whole server.
    let socket = root().join("restart.sock");
    let private = || {
        let mut command = Command::new("tmux");
        command.arg("-S").arg(&socket);
        // Killing a server that is not running is expected here; keep the
        // fixture's output about the tests rather than about that.
        command.stderr(std::process::Stdio::null());
        command
    };
    // `kill-server` returns before the old server has finished exiting, and a
    // `new-session` landing on one still shutting down fails with "server exited
    // unexpectedly". That is a fixture race, not a product failure, so retry it
    // rather than failing the run; twelve attempts 10ms apart stay inside the
    // 350ms replacement budget asserted below. Report tmux's own message on the
    // way out: `private()` silences stderr, which left CI with a bare assertion.
    let start = || {
        let mut last = String::new();
        for _ in 0..12 {
            let attempt = private()
                .env_remove("TMUX")
                .stderr(std::process::Stdio::piped())
                .args([
                    "-f",
                    "/dev/null",
                    "new-session",
                    "-d",
                    "-s",
                    "restarted",
                    "-x",
                    "80",
                    "-y",
                    "24",
                    "cat > /dev/null",
                ])
                .output()
                .unwrap();
            if attempt.status.success() {
                return;
            }
            last = String::from_utf8_lossy(&attempt.stderr).trim().to_owned();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("tmux never started the replacement server: {last}");
    };
    let _ = private().args(["kill-server"]).status();
    start();

    let client = desktop::Client::new(sync::Arc::new(|| {})).unwrap();
    client
        .connect(desktop::Connection {
            options: options(),
            session: core::SessionName::new("restarted").unwrap(),
            socket: Some(socket.to_str().unwrap().to_owned()),
            history: 20,
            access: session::Access::ReadOnly,
            reconnect: true,
        })
        .unwrap();
    wait_until(30, "worker did not attach", || {
        client.phase() == desktop::Phase::Watching
    });
    let first_session = client.with_view(|view| view.unwrap().session);

    // SIGKILL the control client so the channel dies without a %exit, then
    // replace the server before the first retry fires.
    let clients = private()
        .args(["list-clients", "-t", "restarted", "-F", "#{client_pid}"])
        .output()
        .unwrap();
    let pid = String::from_utf8_lossy(&clients.stdout)
        .split_whitespace()
        .next()
        .expect("control client attached")
        .to_owned();
    assert!(
        Command::new("kill")
            .args(["-9", &pid])
            .status()
            .unwrap()
            .success()
    );
    // Replace the server well inside the first backoff delay, so the retry has
    // something to attach to and the identity comparison is exercised.
    let replaced = time::Instant::now();
    let _ = private().args(["kill-server"]).status();
    start();
    assert!(
        replaced.elapsed() < time::Duration::from_millis(350),
        "replacing the server took {:?}, longer than the first backoff delay",
        replaced.elapsed()
    );

    wait_until(90, "worker did not settle after the server restart", || {
        matches!(
            client.phase(),
            desktop::Phase::Watching | desktop::Phase::Failed | desktop::Phase::Disconnected
        ) && client.retry().is_none()
    });
    // Either outcome is correct: the retry can land before the replacement
    // server exists, in which case it stops on "no server running". Only the
    // reattached case can prove the replacement is reported, so require it.
    assert_eq!(
        client.phase(),
        desktop::Phase::Watching,
        "the retry did not land on the replacement server; failure was {:?}",
        client.failure()
    );
    // The fresh server reuses $0, so identity must come from more than that.
    assert_eq!(
        client.with_view(|view| view.unwrap().session),
        first_session,
        "this fixture depends on tmux reusing the first session id"
    );
    let continuity = client
        .continuity()
        .expect("a replaced server must be reported, not presented as continuous");
    assert!(
        continuity.contains("restarted"),
        "unexpected continuity report: {continuity}"
    );
    client.disconnect();
    let _ = private().args(["kill-server"]).status();
}

/// Recreate a single-pane fixture session, replacing any leftover of the same name.
fn reset_session(name: &str, command: &str) {
    let _ = tmux().args(["kill-session", "-t", name]).status();
    assert!(
        tmux()
            .env_remove("TMUX")
            .args([
                "new-session",
                "-d",
                "-s",
                name,
                "-x",
                "80",
                "-y",
                "24",
                command,
            ])
            .status()
            .unwrap()
            .success()
    );
}

/// Kill the tmux client process behind Starcom's control channel. The channel
/// dies with no %exit, which is what an abrupt transport loss looks like.
fn kill_control_client(session: &str) {
    let clients = tmux()
        .args(["list-clients", "-t", session, "-F", "#{client_pid}"])
        .output()
        .unwrap();
    for pid in String::from_utf8_lossy(&clients.stdout).split_whitespace() {
        let _ = std::process::Command::new("kill")
            .args(["-9", pid])
            .status();
    }
}

/// Attaching to a session that does not exist is the one failure where the list
/// of sessions is the answer. It must appear without the user asking, and it
/// must not turn into an attachment or start a server.
#[cfg(feature = "gui")]
#[test]
#[ignore = "requires the isolated SSH/tmux fixture"]
fn a_missing_session_lists_what_the_host_does_have() {
    use starcom::{core, desktop, session};
    use std::sync;

    let client = desktop::Client::new(sync::Arc::new(|| {})).unwrap();
    client
        .connect(desktop::Connection {
            options: options(),
            session: core::SessionName::new("starcom-does-not-exist").unwrap(),
            socket: Some(root().join("tmux.sock").to_str().unwrap().to_owned()),
            history: 20,
            access: session::Access::ReadOnly,
            reconnect: true,
        })
        .unwrap();

    wait_until(30, "the attach did not fail", || {
        matches!(
            client.phase(),
            desktop::Phase::Failed | desktop::Phase::Disconnected
        )
    });
    assert_eq!(
        client.failure(),
        Some(starcom::reconnect::Failure::MissingSession),
        "a missing session must be classified as such, not retried"
    );
    assert!(client.retry().is_none(), "a missing session was retried");

    wait_until(30, "the host was not asked what it does have", || {
        matches!(
            client.discovery(),
            Some(desktop::Discovery::Sessions(_) | desktop::Discovery::Failed(_))
        )
    });
    let Some(desktop::Discovery::Sessions(found)) = client.discovery() else {
        panic!("expected a session listing, got {:?}", client.discovery())
    };
    assert!(
        found.iter().any(|summary| summary.name == "starcom"),
        "the listing did not include the fixture session: {found:?}"
    );

    // Listing must not have attached to anything or invented a session.
    assert!(
        matches!(
            client.phase(),
            desktop::Phase::Failed | desktop::Phase::Disconnected
        ),
        "listing turned into an attachment: {:?}",
        client.phase()
    );
    let sessions = tmux()
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&sessions.stdout).contains("starcom-does-not-exist"),
        "a session was created for a failed attach"
    );
    client.disconnect();
}
