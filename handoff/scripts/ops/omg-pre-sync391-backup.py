"""Read-only production backup; restore only in bounded, isolated tmpfs PostgreSQL."""
import datetime
import json
import os
from pathlib import Path
import re
import shutil
import shlex
import signal
import stat
import subprocess
import tempfile

ROOT = Path('/Users/nicholascecere/projects/rust/omg-rel-093')
KUBE = Path.home() / 'homelab/bitop-talos/kubeconfig'
RECOVERY = Path.home() / '.local/state/open-model-gateway/backups/pre-v070-20261022T021339Z'
IMAGE = 'ghcr.io/ncecere/open-model-gateway@sha256:b345e16645b0dd145bb3c9562d13d1c09b9e782fbf7874fad4af088482256d45'
NAME = 'omg-sync391-production-restore-drill'
PG_IMAGE = 'mirror.gcr.io/library/postgres@sha256:858bb5e98eed02eb8cafbcdc007f9a65d1b613972028944a8ec2133ccaf80d27'
PGDATA = '/var/lib/postgresql/data'
MEMORY = 3 * 1024 ** 3
# Operator-qualified homelab target; endpoint agrees with talos/inventory.yaml.
CONTEXT = 'admin@bitop-talos'
NAMESPACE_UID = '34d599f7-7cdf-4a13-af3a-e128f5cf33fb'
CNPG_UID = 'b55348fe-08b5-46b1-9c21-e2586a70485f'
INSTALLATION_ID = '538d5958-0a67-4955-a920-47d4158a72da'


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def run(args, *, env, step, timeout=120):
    # Client errors can contain restored row values. Never emit raw subprocess
    # stdout/stderr or include exception objects in diagnostics.
    try:
        result = subprocess.run(args, env=env, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, timeout=timeout)
    except (OSError, subprocess.TimeoutExpired):
        raise RuntimeError(step + ' could not complete') from None
    require(result.returncode == 0, step + ' failed')
    return result.stdout


def kubectl_command(*args):
    return ['kubectl', '--kubeconfig', str(KUBE), '--context', CONTEXT, *args]


def validate_context(config):
    require(config.get('current-context') == CONTEXT, 'approved context not selected')
    contexts = config.get('contexts', [])
    clusters = config.get('clusters', [])
    require(len(contexts) == 1 and contexts[0].get('name') == CONTEXT
        and contexts[0].get('context', {}).get('cluster') == 'bitop-talos',
        'approved cluster not selected')
    require(len(clusters) == 1 and clusters[0].get('name') == 'bitop-talos'
        and clusters[0].get('cluster', {}).get('server') == 'https://192.168.100.20:6443',
        'approved API endpoint differs')


def validate_source_identity(namespace, cluster, installation):
    require(namespace['metadata']['uid'] == NAMESPACE_UID, 'source namespace differs')
    require(cluster['metadata']['uid'] == CNPG_UID, 'source CNPG resource differs')
    require(installation == INSTALLATION_ID, 'source installation differs')


def production_wrapper(tool, primary):
    require(tool in ('psql', 'pg_dump'), 'unsupported source tool')
    require(primary in ('open-model-gateway-db-1', 'open-model-gateway-db-2'),
        'unexpected source primary')
    command = kubectl_command('-n', 'open-model-gateway', 'exec', '-i', primary,
        '-c', 'postgres', '--', 'env', 'PGHOST=/controller/run', 'PGUSER=postgres')
    return '#!/bin/sh\nexec ' + shlex.join(command) + ' PGDATABASE="${PGDATABASE:?}" ' + tool + ' "$@"\n'


def validate_restored(result):
    require(result == ['66|67|true', 'enterprise_v1', INSTALLATION_ID],
        'restored schema or installation differs')


def validate_source_manifest(metadata):
    require(type(metadata['latest_version']) is int and metadata['latest_version'] == 67,
        'source schema differs')
    require(isinstance(metadata['migrations'], list) and len(metadata['migrations']) == 66,
        'source migration count or type differs')
    require(metadata['encryption']['tool'] == 'age', 'unencrypted archive refused')


def validate_drill(info, filesystem):
    host = info['HostConfig']
    require(host['NetworkMode'] == 'none', 'drill network is not isolated')
    require(host['ReadonlyRootfs'] is True, 'drill root filesystem is writable')
    require(host['LogConfig']['Type'] == 'none', 'drill logging must be disabled')
    require(host['Memory'] == MEMORY and host['MemorySwap'] == MEMORY,
        'drill memory/swap bounds differ')
    require(info['Config']['Env'].count('PGDATA=' + PGDATA) == 1, 'unexpected PGDATA')
    mounts = info['Mounts']
    # Legacy --tmpfs is recorded in HostConfig.Tmpfs, not Mounts, on this
    # engine. Require both that configuration AND the kernel's actual fs type.
    require(host.get('Tmpfs', {}).get(PGDATA) == 'rw,nosuid,nodev,noexec,size=2g',
        'PGDATA tmpfs configuration differs')
    require(filesystem.strip() == 'tmpfs', 'PGDATA is not actually tmpfs')
    require(not any(m['Type'] in ('bind', 'volume') for m in mounts),
        'persistent drill mount refused')
    require(host['AutoRemove'] is True, 'drill must auto-remove on exit')


def validate_quiescence(deployment, release, pods, jobs, busy):
    require(release['spec'].get('suspend') is True, 'HelmRelease is not suspended')
    require(release['spec']['values']['replicaCount'] == 0, 'Git-backed replicas not zero')
    require(deployment['spec'].get('replicas') == 0, 'live replicas not zero')
    require(not pods['items'], 'old gateway pods still exist')
    require(not any(job.get('status', {}).get('active', 0) for job in jobs['items']),
        'jobs still active')
    require(busy == '0', 'runtime sessions are not quiescent')


def main():
    os.umask(0o077)
    base = os.environ.copy()
    base['KUBECONFIG'] = str(KUBE)
    stage = os.environ.get('BACKUP_STAGE', 'active')
    require(stage in ('active', 'quiescent'), 'invalid backup stage')
    def execute(args, step, **kwargs):
        return run(args, env=base, step=step, **kwargs)

    def get(kind, name):
        return json.loads(execute(kubectl_command('-n', 'open-model-gateway', 'get', kind,
            name, '-o', 'json'), 'read production metadata'))

    validate_context(json.loads(execute(kubectl_command('config', 'view', '--minify',
        '-o', 'json'), 'verify approved Kubernetes target')))
    namespace = get('namespace', 'open-model-gateway')
    cluster = get('cluster', 'open-model-gateway-db')
    require(namespace['metadata']['uid'] == NAMESPACE_UID, 'source namespace differs')
    require(cluster['metadata']['uid'] == CNPG_UID, 'source CNPG resource differs')
    deployment = get('deployment', 'open-model-gateway')
    require(IMAGE in [c['image'] for c in deployment['spec']['template']['spec']['containers']],
        'production image differs from approved schema-67 source')
    primary = cluster['status']['currentPrimary']
    require(primary in ('open-model-gateway-db-1', 'open-model-gateway-db-2'),
        'unexpected source primary')
    installation = execute(kubectl_command('-n', 'open-model-gateway', 'exec', primary,
        '-c', 'postgres', '--', 'env', 'PGHOST=/controller/run', 'PGUSER=postgres',
        'PGDATABASE=gateway', 'psql', '-X', '-Atqc', 'SELECT id FROM installation'),
        'verify production installation identity').strip()
    validate_source_identity(namespace, cluster, installation)
    identity = Path((RECOVERY / 'identity-path.txt').read_text().strip())
    require(stat.S_IMODE(identity.stat().st_mode) == 0o600, 'identity permissions differ')
    if stage == 'quiescent':
        selector = ','.join(f'{k}={v}' for k, v in
            sorted(deployment['spec']['selector']['matchLabels'].items()))
        pods = json.loads(execute(kubectl_command('-n', 'open-model-gateway', 'get', 'pods',
            '-l', selector, '-o', 'json'), 'read gateway pods'))
        jobs = json.loads(execute(kubectl_command('-n', 'open-model-gateway', 'get', 'jobs',
            '-o', 'json'), 'read jobs'))
        busy = execute(kubectl_command('-n', 'open-model-gateway', 'exec', primary, '-c',
            'postgres', '--', 'env', 'PGHOST=/controller/run', 'PGUSER=postgres',
            'PGDATABASE=gateway', 'psql', '-X', '-Atqc',
            "SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND usename='gateway_runtime' AND state IS DISTINCT FROM 'idle'"),
            'check runtime quiescence').strip()
        validate_quiescence(deployment, get('helmrelease', 'omg'), pods, jobs, busy)

    helpers = Path(tempfile.mkdtemp(prefix='omg-pre-sync391-backup-exec-'))
    container = None
    out = None
    completed = False
    def terminated(signum, frame):
        raise RuntimeError('backup operation interrupted')
    for signum in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, terminated)
    try:
        # No disk-backed PGDATA/logs. --rm plus the in-container watchdog bounds
        # residue even if this host process receives uncatchable SIGKILL.
        container = execute(['docker', 'create', '--rm', '--name', NAME, '--network',
            'none', '--log-driver', 'none', '--read-only', '--memory', '3g',
            '--memory-swap', '3g', '--tmpfs', PGDATA + ':rw,nosuid,nodev,noexec,size=2g',
            '--tmpfs', '/var/run/postgresql:rw,nosuid,nodev,size=16m', '--tmpfs',
            '/tmp:rw,nosuid,nodev,noexec,size=64m', '--shm-size', '128m', '--label',
            'omg.owner=v093-production-drill', '-e', 'POSTGRES_HOST_AUTH_METHOD=trust',
            '-e', 'PGDATA=' + PGDATA, '--entrypoint', '/usr/bin/timeout', PG_IMAGE,
            '--signal=TERM', '--kill-after=15s', '900s',
            '/usr/local/bin/docker-entrypoint.sh', 'postgres', '-c', 'log_statement=none',
            '-c', 'log_min_error_statement=panic', '-c', 'log_error_verbosity=terse'],
            'create isolated drill').strip()
        require(re.fullmatch(r'[a-f0-9]{64}', container) is not None, 'invalid drill ID')
        execute(['docker', 'start', container], 'start isolated drill')
        info = json.loads(execute(['docker', 'inspect', container], 'inspect drill'))[0]
        filesystem = execute(['docker', 'exec', container, '/usr/bin/stat', '-f',
            '-c', '%T', PGDATA], 'check actual PGDATA filesystem')
        validate_drill(info, filesystem)
        execute(['docker', 'exec', container, 'sh', '-c',
            'for n in $(seq 1 40); do pg_isready -U postgres >/dev/null 2>&1 && exit 0; sleep 1; done; exit 1'],
            'wait for isolated PostgreSQL', timeout=50)

        for tool in ('psql', 'pg_dump'):
            wrapper = helpers / tool
            wrapper.write_text(production_wrapper(tool, primary))
            wrapper.chmod(0o700)
        # encrypted_dump calls pg_restore --list during streaming. It must never
        # use a host client or a client in the production pod.
        wrapper = helpers / 'pg_restore'
        wrapper.write_text('#!/bin/sh\n[ "$#" -eq 1 ] && [ "$1" = "--list" ] || exit 64\n'
            'exec docker exec -i ' + container + ' pg_restore --list\n')
        wrapper.chmod(0o700)
        stamp = datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')
        out = RECOVERY.parent / ('pre-sync391-' + stage + '-' + stamp)
        out.mkdir(mode=0o700)
        source = base.copy()
        source['PATH'] = str(helpers) + os.pathsep + base['PATH']
        source['DATABASE_URL'] = 'postgresql://postgres@127.0.0.1/gateway'
        run(['python3', str(ROOT / 'scripts/backup.py'), 'backup', '--out-dir', str(out),
            '--encrypt-to-file', str(RECOVERY / 'recipients.txt')], env=source,
            step='stream encrypted production backup', timeout=600)
        manifests = list(out.glob('*.manifest.json'))
        require(len(manifests) == 1, 'unexpected manifest count')
        manifest = manifests[0]
        metadata = json.loads(manifest.read_text())
        validate_source_manifest(metadata)
        execute(['python3', str(ROOT / 'scripts/backup.py'), 'verify', str(manifest),
            '--identity', str(identity), '--pg-container', container],
            'verify encrypted archive', timeout=600)
        binary = helpers / 'gateway-v090-lineage'
        binary.write_text('#!/bin/sh\nexec docker run --rm --network none --read-only '
            '--cap-drop ALL --security-opt no-new-privileges ' + IMAGE + ' "$@"\n')
        binary.chmod(0o700)
        restore = base.copy()
        restore['RESTORE_DATABASE_URL'] = 'postgresql://postgres@127.0.0.1/postgres'
        run(['python3', str(ROOT / 'scripts/backup.py'), 'restore', str(manifest),
            '--identity', str(identity), '--pg-container', container, '--url-env',
            'RESTORE_DATABASE_URL', '--target-db', 'omg_v093_preupgrade_restore',
            '--create', '--gateway-binary', str(binary)], env=restore,
            step='restore only into isolated drill', timeout=600)
        result = execute(['docker', 'exec', container, 'psql', '-U', 'postgres', '-d',
            'omg_v093_preupgrade_restore', '-Atqc',
            "SELECT count(*)||'|'||max(version)||'|'||bool_and(success) FROM _sqlx_migrations; SELECT schema_family FROM installation; SELECT id FROM installation;"],
            'check restored schema').strip().splitlines()
        validate_restored(result)
        completed = True
    finally:
        try:
            if container:
                # Remove by the captured ID, never by a replaceable name. If it
                # already auto-removed, there is no data left to clean up.
                exists = subprocess.run(['docker', 'inspect', container], env=base,
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)
                if exists.returncode == 0:
                    execute(['docker', 'rm', '-fv', container], 'remove isolated drill')
                else:
                    # A daemon failure is not evidence of successful cleanup.
                    ids = execute(['docker', 'ps', '-aq', '--no-trunc'], 'verify drill absence')
                    require(container not in ids.splitlines(), 'drill cleanup incomplete')
        finally:
            shutil.rmtree(helpers)
    require(completed, 'backup drill did not complete')
    print('Fresh encrypted production backup VERIFIED and RESTORED offline at schema67; '
        'deployed-image lineage validated and isolated tmpfs drill removed. '
        'Recovery identity separate. Production was read only. Archive: ' + str(out), flush=True)


if __name__ == '__main__':
    main()
