"""Operator-approved production failover drill for #391 (2026-10-28).

Read-only pre-checks, one acknowledged synchronous commit (no application
tables), delete the primary pod, measure promotion/write/readiness recovery,
prove the acknowledged commit survived, verify rejoin. Never prints secrets.
"""
import json
import os
import subprocess
import time
import urllib.request

NS = 'open-model-gateway'
CLUSTER = 'open-model-gateway-db'
NS_UID = '34d599f7-7cdf-4a13-af3a-e128f5cf33fb'
CNPG_UID = 'b55348fe-08b5-46b1-9c21-e2586a70485f'
ENV = {k: v for k, v in os.environ.items() if k.upper() not in ('HTTPS_PROXY', 'HTTP_PROXY', 'ALL_PROXY')}
ENV['KUBECONFIG'] = '/Users/nicholascecere/homelab/bitop-talos/kubeconfig'
K = ['kubectl', '--context', 'admin@bitop-talos', '--request-timeout=20s']
T0 = None


def log(msg):
    stamp = '' if T0 is None else ' t+%.1fs' % (time.monotonic() - T0)
    print(time.strftime('%H:%M:%S', time.gmtime()) + stamp + ' ' + msg, flush=True)


def run(args, input=None, timeout=30, check=True):
    r = subprocess.run(args, input=input, env=ENV, capture_output=True, text=True, timeout=timeout)
    if check and r.returncode:
        raise RuntimeError('command failed: ' + ' '.join(args[:6]) + ' :: ' + r.stderr.strip().splitlines()[-1][:200] if r.stderr.strip() else 'no stderr')
    return r


def get(*a):
    return json.loads(run(K + ['-n', NS, 'get', *a, '-o', 'json']).stdout)


def sql(pod, q, timeout=15, check=True):
    r = run(K + ['-n', NS, 'exec', '-i', pod, '-c', 'postgres', '--', 'psql', '-XAtq', '-U', 'postgres',
                 '-d', 'gateway', '-v', 'ON_ERROR_STOP=1'], input=q, timeout=timeout, check=check)
    return r.stdout.strip() if r.returncode == 0 else None


def ready_public():
    try:
        with urllib.request.urlopen(urllib.request.Request('https://platform.bitop.dev/health/ready'), timeout=3) as r:
            return r.status == 200
    except Exception:
        return False


def require(cond, msg):
    if not cond:
        raise SystemExit('ABORT (no action taken unless stated): ' + msg)


def main():
    global T0
    # ---- Pre-checks (read-only) ----
    require(json.loads(run(K + ['get', 'namespace', NS, '-o', 'json']).stdout)['metadata']['uid'] == NS_UID, 'namespace UID')
    c = get('cluster', CLUSTER)
    require(c['metadata']['uid'] == CNPG_UID, 'cluster UID')
    st = c['status']
    require(st.get('phase') == 'Cluster in healthy state' and st.get('readyInstances') == 3 == st.get('instances'), 'cluster not healthy 3/3')
    require(st.get('currentPrimary') == st.get('targetPrimary'), 'switchover already pending')
    sync = c['spec']['postgresql'].get('synchronous') or {}
    require(sync.get('dataDurability') == 'required' and sync.get('failoverQuorum') is True and sync.get('number') == 1, 'durability policy not in force')
    old = st['currentPrimary']
    rep = sql(old, "select string_agg(application_name||':'||state||':'||sync_state||':'||pg_wal_lsn_diff(pg_current_wal_lsn(),replay_lsn), ',' order by 1) from pg_stat_replication")
    log('pre: primary=%s replication=%s' % (old, rep))
    parts = rep.split(',')
    require(len(parts) == 2 and all(p.split(':')[1:3] == ['streaming', 'quorum'] for p in parts), 'both standbys must be streaming/quorum')
    fq = get('failoverquorums.postgresql.cnpg.io', CLUSTER)['status']
    require(fq.get('primary') == old and len(fq.get('standbyNames', [])) == 2 and fq.get('standbyNumber') == 1, 'FailoverQuorum status')
    require(sql(old, 'show synchronous_commit') == 'on', 'synchronous_commit')
    busy = sql(old, "select count(*) from inference_executions where started_at > now() - interval '10 minutes'")
    require(busy == '0', 'traffic in last 10 minutes; not a quiet window')
    d = get('deployment', 'open-model-gateway')
    require(d['status'].get('readyReplicas') == 2, 'gateway not 2/2')
    gw_restarts = {p['metadata']['name']: p['status']['containerStatuses'][0]['restartCount']
                   for p in get('pods')['items']
                   if p['metadata']['name'].startswith('open-model-gateway-') and '-db-' not in p['metadata']['name'] and 'pooler' not in p['metadata']['name']}
    require(len(gw_restarts) == 2, 'expected two gateway pods')
    require(ready_public(), 'public readiness not 200 before drill')
    log('pre-checks passed; gateway pods %s' % gw_restarts)
    if os.environ.get('DRILL_DRY_RUN') == '1':
        log('DRY RUN: stopping before any write or fault')
        return

    # ---- Acknowledged synchronous commit (no application tables) ----
    out = sql(old, 'BEGIN; SELECT txid_current(); COMMIT; SELECT pg_current_wal_flush_lsn();')
    xid, ack_lsn = out.split('\n')[0], out.split('\n')[-1]
    require(xid.isdigit(), 'probe xid')
    log('acknowledged synchronous commit xid=%s flush_lsn=%s on %s' % (xid, ack_lsn, old))

    # ---- Fault: delete the primary pod ----
    T0 = time.monotonic()
    run(K + ['-n', NS, 'delete', 'pod', old, '--wait=false'])
    log('DELETED primary pod %s' % old)
    promoted = write_ok = None
    not_ready_since = None
    readiness_gaps = []
    new = None
    while time.monotonic() - T0 < 300:
        r = ready_public()
        now = time.monotonic() - T0
        if not r and not_ready_since is None:
            not_ready_since = now
        if r and not_ready_since is not None:
            readiness_gaps.append((round(not_ready_since, 1), round(now, 1)))
            not_ready_since = None
        try:
            cur = get('cluster', CLUSTER)['status'].get('currentPrimary')
        except Exception:
            cur = None
        if cur and cur != old and promoted is None:
            if sql(cur, 'select pg_is_in_recovery()', check=False) == 'f':
                promoted, new = now, cur
                log('PROMOTED %s' % new)
        if promoted is not None and write_ok is None:
            if sql(new, "SET statement_timeout='5s'; BEGIN; SELECT txid_current(); COMMIT;", check=False):
                write_ok = time.monotonic() - T0
                log('first acknowledged write on new primary')
        if write_ok is not None and not_ready_since is None and now > write_ok + 5:
            break
        time.sleep(1)
    require(promoted is not None and write_ok is not None, 'no promotion/write recovery within 300s; investigate manually (backup pre-drill391 available)')

    # ---- Durability proof ----
    status = sql(new, 'select txid_status(%s)' % xid)
    log('txid_status(%s) on new primary %s = %s' % (xid, new, status))
    require(status == 'committed', 'ACKNOWLEDGED COMMIT LOST')

    # ---- Rejoin and policy ----
    for _ in range(120):
        c = get('cluster', CLUSTER)['status']
        rep = sql(new, "select string_agg(application_name||':'||state||':'||sync_state, ',' order by 1) from pg_stat_replication", check=False) or ''
        if c.get('phase') == 'Cluster in healthy state' and c.get('readyInstances') == 3 and rep.count('streaming:quorum') == 2:
            break
        time.sleep(5)
    log('post: phase=%s ready=%s replication=%s' % (c.get('phase'), c.get('readyInstances'), rep))
    require(rep.count('streaming:quorum') == 2, 'standbys did not return to quorum')
    log('post: synchronous_standby_names=%s' % sql(new, 'show synchronous_standby_names'))
    fq = get('failoverquorums.postgresql.cnpg.io', CLUSTER)['status']
    log('post: FailoverQuorum primary=%s standbys=%s number=%s' % (fq.get('primary'), fq.get('standbyNames'), fq.get('standbyNumber')))
    after = {p['metadata']['name']: p['status']['containerStatuses'][0]['restartCount']
             for p in get('pods')['items']
             if p['metadata']['name'] in gw_restarts}
    log('post: gateway restarts before=%s after=%s; public ready=%s' % (gw_restarts, after, ready_public()))
    print(json.dumps({'drill': 'PASSED', 'old_primary': old, 'new_primary': new,
                      'promotion_s': round(promoted, 1), 'first_write_s': round(write_ok, 1),
                      'public_readiness_gaps_s': readiness_gaps, 'ack_commit_survived': True}), flush=True)


if __name__ == '__main__':
    main()
