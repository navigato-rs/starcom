#!/usr/bin/env bash
# Disposable loopback sshd and tmux. Never use the contributor's default socket.
set -euo pipefail
work=$(mktemp -d /tmp/starcom-ssh.XXXXXX)
chmod 700 "$work"
sshd_pid=""
noforward_pid=""
agent_started=0
cleanup() {
    tmux -S "$work/tmux.sock" kill-server 2>/dev/null || true
    if [[ -f "$work/sshd.pid" ]]; then sudo kill "$(cat "$work/sshd.pid")" 2>/dev/null || true; fi
    if [[ -f "$work/sshd.noforward.pid" ]]; then sudo kill "$(cat "$work/sshd.noforward.pid")" 2>/dev/null || true; fi
    if [[ -n "$sshd_pid" ]]; then sudo kill "$sshd_pid" 2>/dev/null || true; fi
    if [[ -n "$noforward_pid" ]]; then sudo kill "$noforward_pid" 2>/dev/null || true; fi
    if [[ "$agent_started" == 1 ]]; then ssh-agent -k >/dev/null 2>&1 || true; fi
    rm -rf "$work"
}
trap cleanup EXIT
sshd_bin=$(command -v sshd || true)
if [[ -z $sshd_bin ]]; then
    echo 'OpenSSH sshd not found; install openssh-server' >&2
    exit 1
fi
ssh-keygen -q -t ed25519 -N '' -f "$work/host_key"
ssh-keygen -q -t ed25519 -N '' -f "$work/id_ed25519"
ssh-keygen -q -t rsa -b 2048 -N '' -f "$work/id_rsa"
ssh-keygen -q -t ecdsa -b 256 -N '' -f "$work/id_ecdsa"
cat "$work/id_ed25519.pub" "$work/id_rsa.pub" "$work/id_ecdsa.pub" > "$work/authorized_keys"
python3 - "$work/port" <<'PY'
import socket, sys
with socket.socket() as sock:
    sock.bind(('127.0.0.1', 0))
    with open(sys.argv[1], 'w') as out:
        out.write(str(sock.getsockname()[1]))
PY
port=$(cat "$work/port")
user=$(id -un)
cat > "$work/sshd_config" <<EOF
Port $port
ListenAddress 127.0.0.1
HostKey $work/host_key
PidFile $work/sshd.pid
AuthorizedKeysFile $work/authorized_keys
AllowUsers $user
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin prohibit-password
UsePAM yes
UseDNS no
GSSAPIAuthentication no
# Test fixture only: the unique authorized-key path lives under /tmp.
StrictModes no
RekeyLimit 16K
PrintMotd no
LogLevel ERROR
AllowTcpForwarding yes
EOF
sftp_server=""
sshd_root=$(dirname "$(dirname "$(readlink -f "$sshd_bin")")")
for candidate in \
    "$sshd_root/libexec/sftp-server" \
    /usr/lib/openssh/sftp-server \
    /usr/libexec/openssh/sftp-server \
    /usr/libexec/sftp-server \
    /usr/lib/ssh/sftp-server
do
    if [[ -x $candidate ]]; then
        sftp_server=$candidate
        break
    fi
done
if [[ -z $sftp_server ]]; then
    echo 'OpenSSH sftp-server not found; install openssh-sftp-server' >&2
    exit 1
fi
printf 'Subsystem sftp %s\n' "$sftp_server" >> "$work/sshd_config"
# A second server that forbids forwarding, so the refusal path is exercised
# against a real sshd rather than assumed. Bastions commonly disable it.
python3 - "$work/port.noforward" <<'PY'
import socket, sys
with socket.socket() as sock:
    sock.bind(('127.0.0.1', 0))
    with open(sys.argv[1], 'w') as out:
        out.write(str(sock.getsockname()[1]))
PY
noforward_port=$(cat "$work/port.noforward")
sed -e "s/^Port .*/Port $noforward_port/" \
    -e "s/^AllowTcpForwarding .*/AllowTcpForwarding no/" \
    -e "s|^PidFile .*|PidFile $work/sshd.noforward.pid|" \
    "$work/sshd_config" > "$work/sshd_config.noforward"
sudo mkdir -p /run/sshd
# The logs are in this user's private temporary directory; only sshd itself
# needs elevation. shellcheck's redirect warning does not apply here.
# shellcheck disable=SC2024
sudo "$sshd_bin" -D -e -f "$work/sshd_config" > "$work/sshd.log" 2>&1 &
sshd_pid=$!
# shellcheck disable=SC2024
sudo "$sshd_bin" -D -e -f "$work/sshd_config.noforward" > "$work/sshd.noforward.log" 2>&1 &
noforward_pid=$!
python3 - "$port" "$work/sshd.log" <<'PY'
import socket, sys, time
end = time.monotonic() + 10
while time.monotonic() < end:
    try:
        with socket.create_connection(('127.0.0.1', int(sys.argv[1])), timeout=0.2):
            break
    except OSError:
        time.sleep(0.05)
else:
    raise SystemExit(open(sys.argv[2]).read() or 'fixture sshd did not start')
PY
# Trust is constructed from our generated server key, not from an unauthenticated
# ssh-keyscan result. All private fixture keys are deleted by the EXIT trap.
read -r kind key _ < "$work/host_key.pub"
printf '[127.0.0.1]:%s %s %s\n' "$port" "$kind" "$key" > "$work/known_hosts"
# Both servers present the same host key; trust the second one's port too.
printf '[127.0.0.1]:%s %s %s\n' "$noforward_port" "$kind" "$key" >> "$work/known_hosts"
printf '@revoked [127.0.0.1]:%s %s %s\n' "$port" "$kind" "$key" > "$work/known_hosts.revoked"
cat "$work/known_hosts" >> "$work/known_hosts.revoked"
read -r kind key _ < "$work/id_ed25519.pub"
printf '[127.0.0.1]:%s %s %s\n' "$port" "$kind" "$key" > "$work/known_hosts.bad"
: > "$work/known_hosts.empty"
cp "$work/known_hosts" "$work/known_hosts.hashed"
ssh-keygen -H -f "$work/known_hosts.hashed" >/dev/null 2>&1
eval "$(ssh-agent -s)" >/dev/null
agent_started=1
ssh-add "$work/id_ed25519" "$work/id_rsa" "$work/id_ecdsa" >/dev/null 2>&1

env -u TMUX tmux -S "$work/tmux.sock" -f /dev/null new-session -d -s starcom -x 100 -y 30 \
    "printf '\033[2J\033[HSTARCOM_PRIMARY_READY\r\n'; exec sleep 600"
tmux -S "$work/tmux.sock" split-window -h -t starcom \
    "printf '\033[?1049h\033[2J\033[HSTARCOM_ALTERNATE_READY'; exec sleep 600"
tmux -S "$work/tmux.sock" set-option -t starcom update-environment STARCOM_TEST_ENV
tmux -S "$work/tmux.sock" set-environment -t starcom STARCOM_TEST_ENV original
# Bounded readiness check; do not race the PTYs' first output against capture.
for _ in $(seq 1 100); do
    count=$(tmux -S "$work/tmux.sock" list-panes -t starcom -F '#{pane_id}' | while read -r pane; do
        tmux -S "$work/tmux.sock" capture-pane -p -t "$pane"
    done | grep -c STARCOM_ || true)
    [[ "$count" == 2 ]] && break
    sleep 0.05
done
[[ "$count" == 2 ]] || { echo 'tmux fixture did not become ready' >&2; exit 1; }
export STARCOM_TEST_DIR="$work" STARCOM_TEST_USER="$user"
export STARCOM_NO_FORWARD_PORT="$noforward_port"
export SUNSET_AGENT_TEST_PUBKEY="$work/id_ed25519.pub"
# cargo exits 0 when a filter matches nothing, so a renamed test or a changed
# cfg would leave this whole fixture green having asserted nothing. Require the
# tests to actually run, and update the counts when tests are added.
run_fixture() {
    local expected=$1 log
    shift
    log=$(mktemp "$work/testlog.XXXXXX")
    "$@" 2>&1 | tee "$log"
    local status=${PIPESTATUS[0]}
    [[ "$status" == 0 ]] || return "$status"
    local passed
    passed=$(awk '/^test result: ok\./ { total += $4 } END { print total + 0 }' "$log")
    if [[ "$passed" != "$expected" ]]; then
        echo "fixture ran $passed tests, expected $expected: a test was renamed, skipped, or cfg'd out" >&2
        return 1
    fi
}

cargo_args=(--locked)
integration_tests=25

# Build first, untimed. The per-run timeouts below bound how long a test may
# take to RUN; letting them also cover compilation makes a cold tree look like
# a hung test.
cargo test "${cargo_args[@]}" --lib --test ssh_localhost --test ssh_migration --no-run
cargo test "${cargo_args[@]}" -p sunset-client --lib --no-run

run_fixture 1 timeout 30s cargo test "${cargo_args[@]}" -p sunset-client --lib signs_with_isolated_openssh_agent -- --ignored --test-threads=1
run_fixture "$integration_tests" timeout 300s cargo test "${cargo_args[@]}" --test ssh_localhost --test ssh_migration -- --ignored --test-threads=1
