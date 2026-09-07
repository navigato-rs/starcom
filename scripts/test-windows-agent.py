#!/usr/bin/env python3
"""Exercise the Windows OpenSSH-agent pipe with one disposable Ed25519 key.

The caller starts the ssh-agent service. Only this script's temporary identity
is added and removed; no user identity or trust store is modified.
"""
import os
from pathlib import Path
import subprocess
import tempfile

with tempfile.TemporaryDirectory(prefix="starcom-agent-") as directory:
    key = Path(directory) / "identity"
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key)], check=True, timeout=15)
    subprocess.run(["ssh-add", str(key)], check=True, timeout=15)
    try:
        environment = os.environ.copy()
        environment.pop("SSH_AUTH_SOCK", None)
        environment["SUNSET_AGENT_TEST_PUBKEY"] = str(key) + ".pub"
        subprocess.run(["cargo", "test", "--locked", "-p", "sunset-client", "--lib", "signs_with_isolated_openssh_agent", "--", "--ignored", "--test-threads=1"],
                       env=environment, check=True, timeout=120)
    finally:
        subprocess.run(["ssh-add", "-d", str(key)], check=True, timeout=15)
