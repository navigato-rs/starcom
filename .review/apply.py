"""Prepare a pinned dependency update; publish only validated source blobs."""
import base64
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import urllib.request
import zlib

root = Path(sys.argv[1])
base = 'ac7893f809b5b8730fc081ac1424bc05f3256d22'
revision = '6bc515dabb5e4fc118719308ae7fe2bdf1efe185'
paths = ['.github/workflows/release.yml', 'Cargo.lock', 'Cargo.toml', 'PRIVACY.md']
def git(*args):
    return subprocess.check_output(['git', '-C', str(root), *args]).decode().strip()
assert git('rev-parse', 'HEAD') == base
if len(sys.argv) == 2:
    assert not git('status', '--porcelain')
    packed = Path(__file__).with_name('changes.zlib').read_bytes()
    assert hashlib.sha256(packed).hexdigest() == 'def21bf14c015ccbcd01082426f2b4cdfec8fc3d4c984728c1ae6442cc3a60a3'
    patch = zlib.decompress(packed).decode()
    for flags in [('--check',), ()]:
        subprocess.run(['git', '-C', str(root), 'apply', *flags, '-'], input=patch, text=True, check=True)
    path = root / '.github/workflows/release.yml'
    text = path.read_text()
    assert text.count('SUPPORT_REVISION') == 3
    path.write_text(text.replace('SUPPORT_REVISION', revision))
    path = root / 'Cargo.toml'
    text = path.read_text()
    old = '8c9f3aaceab0ab77332f01ef4d61fd32926a9138'
    assert text.count(old) == 1
    path.write_text(text.replace(old, revision))
else:
    assert sorted(git('diff', '--name-only').splitlines()) == sorted(paths)
    git('add', '--', *paths)
    git('diff', '--cached', '--check')
    entries = []
    for path in paths:
        mode, expected, *_ = git('ls-files', '--stage', '--', path).split()
        data = (root / path).read_bytes()
        request = urllib.request.Request('https://api.github.com/repos/navigato-rs/starcom/git/blobs', json.dumps({'content': base64.b64encode(data).decode(), 'encoding': 'base64'}).encode(), headers={'Authorization': 'Bearer ' + os.environ['GH_TOKEN'], 'Content-Type': 'application/json'})
        with urllib.request.urlopen(request, timeout=30) as response:
            actual = json.load(response)['sha']
        assert actual == expected
        entries.append({'path': path, 'mode': mode, 'type': 'blob', 'sha': actual})
    output = root.parent / 'export'
    output.mkdir(exist_ok=True)
    (output / 'publication.json').write_text(json.dumps({'parent': base, 'tree': git('write-tree'), 'entries': entries}))
