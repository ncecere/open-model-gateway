"""Independent read-only acceptance of the fixed v0.9.3 publication."""
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import tempfile
import uuid

R = Path('/Users/nicholascecere/projects/rust/open-model-gateway')
OUT = R / '.local/v093-publication'
OUT.mkdir(mode=0o700)
IMAGE = 'ghcr.io/ncecere/open-model-gateway'
DIGEST = REF = None  # derived from the release digest asset below, then verified everywhere
COMMIT = '1ea74865584429892e61313f3c944ca1e6ed39a9'
COSIGN = R / '.local/cosign375-live.SBu4aIC1/cosign'
assert hashlib.sha256(COSIGN.read_bytes()).hexdigest() == '424e2c97d0b45fd7a6b608a87f090f35633f4c23ece5f092d48bba11aaf4bbfa'
ASSETS = {'checksums.txt', 'open-model-gateway-v0.9.3.digest.txt',
          'open-model-gateway-v0.9.3-linux-amd64.sbom.spdx.json',
          'open-model-gateway-v0.9.3-linux-arm64.sbom.spdx.json'}

def output(args, env=None, timeout=90):
    result = subprocess.run(args, env=env, capture_output=True, check=True, timeout=timeout)
    assert len(result.stdout) < 32 * 1024 * 1024
    return result.stdout

signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt()))
release = json.loads(output(['gh', 'release', 'view', 'v0.9.3', '--repo', 'ncecere/open-model-gateway',
    '--json', 'tagName,isDraft,isPrerelease,url,assets']))
assert release['tagName'] == 'v0.9.3' and not release['isDraft'] and not release['isPrerelease']
assert {a['name'] for a in release['assets']} == ASSETS
assert all(0 < a['size'] <= 32 * 1024 * 1024 for a in release['assets'])
tag = json.loads(output(['gh', 'api', 'repos/ncecere/open-model-gateway/git/tags/760b3f6f9e66a04a50734c807b6c21ce179b9c07']))
assert tag['object']['type'] == 'commit' and tag['object']['sha'] == COMMIT
subprocess.run(['gh', 'release', 'download', 'v0.9.3', '--repo', 'ncecere/open-model-gateway', '--dir', str(OUT)], check=True, timeout=120)
assert {p.name for p in OUT.iterdir()} == ASSETS
_published = (OUT / 'open-model-gateway-v0.9.3.digest.txt').read_text().strip()
_m = re.fullmatch(r'ghcr\.io/ncecere/open-model-gateway@(sha256:[0-9a-f]{64})', _published)
assert _m, 'release digest asset shape'
DIGEST = _m.group(1)
REF = IMAGE + '@' + DIGEST
for p in OUT.iterdir():
    assert stat.S_ISREG(p.lstat().st_mode) and p.stat().st_size <= 32 * 1024 * 1024
checks = (OUT / 'checksums.txt').read_text().splitlines()
assert len(checks) == 3
seen = set()
for line in checks:
    match = re.fullmatch(r'([a-f0-9]{64})  ([A-Za-z0-9.-]+)', line)
    assert match
    digest, name = match.groups()
    assert name in ASSETS - {'checksums.txt'} and name not in seen
    seen.add(name)
    assert hashlib.sha256((OUT / name).read_bytes()).hexdigest() == digest
assert seen == ASSETS - {'checksums.txt'}
assert (OUT / 'open-model-gateway-v0.9.3.digest.txt').read_text().strip() == REF

with tempfile.TemporaryDirectory(prefix='omg092-verify-client-') as home:
    plugins = Path('/Applications/Docker.app/Contents/Resources/cli-plugins')
    assert plugins.is_dir()
    Path(home, 'config.json').write_text(json.dumps({'cliPluginsExtraDirs': [str(plugins)]}))
    env = {'PATH': os.environ['PATH'], 'HOME': home, 'DOCKER_CONFIG': home,
           'DOCKER_HOST': 'unix:///Users/nicholascecere/.docker/run/docker.sock'}
    def inspect(ref, *args):
        return output(['docker', 'buildx', 'imagetools', 'inspect', ref, *args], env, 120)
    raw = inspect(REF, '--raw')
    assert hashlib.sha256(raw).hexdigest() == DIGEST[7:]
    (OUT / 'index.json').write_bytes(raw)
    index = json.loads(raw)
    assert len(index['manifests']) == 4
    platforms = {m['platform']['architecture']: m for m in index['manifests'] if m['platform']['os'] == 'linux'}
    assert set(platforms) == {'amd64', 'arm64'}
    for m in index['manifests']:
        if m['platform']['os'] == 'unknown':
            assert m['annotations']['vnd.docker.reference.digest'] in {v['digest'] for v in platforms.values()}
    for tag_name in ('v0.9.3', 'v0.9', 'latest-release'):
        assert hashlib.sha256(inspect(IMAGE + ':' + tag_name, '--raw')).hexdigest() == DIGEST[7:]
    for kind, fmt in [('sboms', '{{json .SBOM}}'), ('provenance', '{{json .Provenance}}')]:
        data = inspect(REF, '--format', fmt)
        (OUT / (kind + '.json')).write_bytes(data)
        doc = json.loads(data)
        assert set(doc) == {'linux/amd64', 'linux/arm64'}
        for arch in ('amd64', 'arm64'):
            if kind == 'sboms':
                spdx = doc['linux/' + arch]['SPDX']
                assert spdx['spdxVersion'].startswith('SPDX-') and spdx['packages']
                asset = json.loads((OUT / f'open-model-gateway-v0.9.3-linux-{arch}.sbom.spdx.json').read_text())
                assert asset == spdx
            else:
                assert doc['linux/' + arch]
    def run_container(image, platform, args, mount_tool=False):
        owner = uuid.uuid4().hex
        name = 'omg092-verify-' + owner
        cmd = ['docker', 'run', '--rm', '--pull', 'missing', '--name', name, '--label', 'omg.local.owner=' + owner,
               '--platform', platform, '--user', '10001:10001', '--read-only', '--cap-drop', 'ALL',
               '--security-opt', 'no-new-privileges', '--memory', '1g', '--cpus', '1', '--pids-limit', '64']
        if mount_tool:
            cmd += ['--tmpfs', '/tmp:rw,noexec,nosuid,size=64m', '--workdir', '/tmp', '-e', 'HOME=/tmp',
                    '-e', 'DOCKER_CONFIG=/tmp/empty', '-v', str(COSIGN) + ':/tool/cosign:ro', '--entrypoint', '/tool/cosign']
        else:
            cmd += ['--network', 'none']
        try:
            result = subprocess.run(cmd + [image, *args], env=env, capture_output=True, check=True, timeout=240)
            assert len(result.stdout) < 32 * 1024 * 1024
            return result.stdout, result.stderr
        finally:
            check = subprocess.run(['docker', 'inspect', '--format', '{{index .Config.Labels "omg.local.owner"}}', name], env=env, capture_output=True, text=True, timeout=5)
            if check.returncode == 0:
                assert check.stdout.strip() == owner
                subprocess.run(['docker', 'rm', '-f', name], env=env, check=True, stdout=subprocess.DEVNULL, timeout=10)
            assert not output(['docker', 'ps', '-a', '--filter', 'label=omg.local.owner=' + owner, '--format', '{{.ID}}'], env, 5).strip()
    signature, log = run_container('gcr.io/distroless/cc-debian13@sha256:3ee00291cf386ec52a552e6a858d555debd74776eb4bfa53fa3df2cd6677515e',
        'linux/amd64', ['verify', '--certificate-identity',
        'https://github.com/ncecere/open-model-gateway/.github/workflows/image.yml@refs/tags/v0.9.3',
        '--certificate-oidc-issuer', 'https://token.actions.githubusercontent.com', REF], True)
    (OUT / 'signature.json').write_bytes(signature)
    (OUT / 'verify.log').write_bytes(log)
    signatures = json.loads(signature)
    assert signatures and all(s['critical']['image']['docker-manifest-digest'] == DIGEST for s in signatures)
    version, _ = run_container(REF, 'linux/arm64', ['--version'])
    assert version.decode().strip() == 'open-model-gateway 0.9.3'
    schema, _ = run_container(REF, 'linux/arm64', ['schema-version'])
    schema = json.loads(schema)
    assert schema['latest_version'] == 67 and len(schema['migrations']) == 66
    cfg = json.loads(output(['docker', 'image', 'inspect', REF], env))[0]
    assert cfg['Architecture'] == 'arm64'
    assert cfg['Config']['Labels']['org.opencontainers.image.revision'] == COMMIT
    assert cfg['Config']['Labels']['org.opencontainers.image.version'] == 'v0.9.3'
print(json.dumps({'release': release['url'], 'root': DIGEST, 'checksums': 'all3 assets verified',
    'SPDX': 'both assets equal image-attached documents', 'signature': 'exact v0.9.3 workflow/issuer/root verified',
    'tags': 'full/minor/latest-release match', 'native_arm64_binary': '0.9.3, schema67/66records',
    'provenance': 'both architecture documents present', 'directory': str(OUT)}))
