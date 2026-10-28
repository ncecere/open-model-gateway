"""Owned PG17 fixture only; cargo runs separately through the managed process tool."""
import os, json, runpy, secrets, signal, uuid
from pathlib import Path
if not __debug__:
    raise RuntimeError('optimized execution refused before external operations')
R = Path('/Users/nicholascecere/projects/rust/open-model-gateway')
helpers = runpy.run_path(str(R / '.local/verifier382-local-e2e.py'))
run, invoke, PG = helpers['run'], helpers['invoke'], helpers['PG']
path = Path('/tmp/omg370-pg.json')
nonce = uuid.uuid4().hex
name = 'omg370-diag-' + nonce
cid, primary, errors, owns_file = None, None, [], False
def stop(*_):
    raise RuntimeError('owned fixture stopped')
for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
    signal.signal(sig, stop)
try:
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    owns_file = True
    password = secrets.token_hex(24)
    cid = run(['docker', 'create', '--name', name, '--label', 'omg.owner=' + nonce,
               '-p', '127.0.0.1:54395:5432', '--memory', '2g', '--memory-swap', '2g',
               '--log-driver', 'none', '--tmpfs', '/var/lib/postgresql/data:rw,nosuid,nodev,noexec,size=1g',
               '-e', 'POSTGRES_PASSWORD', PG], env={**os.environ, 'POSTGRES_PASSWORD': password}).strip()
    if len(cid) != 64 or any(c not in '0123456789abcdef' for c in cid):
        raise RuntimeError('invalid owned container identity')
    run(['docker', 'start', cid])
    run(['docker', 'exec', cid, 'sh', '-c', 'for n in $(seq 1 40); do pg_isready -h127.0.0.1 -Upostgres >/dev/null 2>&1 && exit 0; sleep 1; done; exit 1'], timeout=45)
    with os.fdopen(fd, 'w') as file:
        json.dump({'url': 'postgres://postgres:' + password + '@127.0.0.1:54395/postgres',
                   'cid': cid, 'owner': nonce}, file)
    print('STEERING370_PG_READY; approved local daemon / fresh pinned PG17 / private environment file', flush=True)
    signal.pause()
except BaseException as error:
    primary = error
finally:
    try:
        found = invoke(['docker', 'inspect', name], timeout=15)
        if found.returncode == 0:
            obj = json.loads(found.stdout)[0]
            if obj['Config']['Labels'].get('omg.owner') != nonce or (cid is not None and obj['Id'] != cid):
                raise RuntimeError('owned container identity changed')
            run(['docker', 'rm', '-fv', obj['Id']])
        if run(['docker', 'ps', '-aq', '--no-trunc', '--filter', 'label=omg.owner=' + nonce]).strip():
            raise RuntimeError('owned container remains')
    except BaseException:
        errors.append('owned PostgreSQL cleanup uncertain: ' + name)
    if owns_file:
        path.unlink(missing_ok=True)
if errors:
    raise RuntimeError('; '.join(errors)) from primary
if primary is not None and str(primary) != 'owned fixture stopped':
    raise primary
print('STEERING370_PG_REMOVED; owned container absence verified', flush=True)
