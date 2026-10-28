"""Exact-head gates on a descriptor-validated, explicitly owned local PG17 fixture."""
import argparse,json,os,re,runpy,signal,stat,subprocess,tempfile
from pathlib import Path
if not __debug__:
    raise RuntimeError('optimized execution refused before external operations')
R=Path('/Users/nicholascecere/projects/rust/open-model-gateway')
W=Path('/Users/nicholascecere/projects/rust/omg-wt-370-diag')
PG='mirror.gcr.io/library/postgres@sha256:858bb5e98eed02eb8cafbcdc007f9a65d1b613972028944a8ec2133ccaf80d27'

def load_fixture(path):
    fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
    with os.fdopen(fd,'r') as file:
        info=os.fstat(file.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid!=os.getuid() or stat.S_IMODE(info.st_mode)!=0o600 or info.st_nlink!=1 or not 0<info.st_size<=4096:
            raise RuntimeError('fixture descriptor ownership/type/mode/size refused')
        data=json.loads(file.read(4097))
    if not isinstance(data,dict) or set(data)!={'url','cid','owner'} or not all(isinstance(v,str) for v in data.values()):
        raise RuntimeError('fixture structured handoff refused')
    if not re.fullmatch('[a-f0-9]{64}',data['cid']) or not re.fullmatch('[a-f0-9]{32}',data['owner']):
        raise RuntimeError('fixture identity refused')
    match=re.fullmatch(r'postgres://postgres:([a-f0-9]{48})@127\.0\.0\.1:54395/postgres',data['url'])
    if match is None:raise RuntimeError('fixture database target refused')
    return data,match.group(1)

def validate_container(data,password,obj):
    bindings=obj.get('NetworkSettings',{}).get('Ports',{}).get('5432/tcp')
    if obj.get('Id')!=data['cid'] or obj.get('Name')!='/omg370-diag-'+data['owner'] or obj.get('Config',{}).get('Labels',{}).get('omg.owner')!=data['owner'] or obj.get('Config',{}).get('Image')!=PG or obj.get('State',{}).get('Running') is not True:
        raise RuntimeError('fixture container identity/image/state refused')
    if bindings!=[{'HostIp':'127.0.0.1','HostPort':'54395'}] or 'POSTGRES_PASSWORD='+password not in obj['Config'].get('Env',[]):
        raise RuntimeError('fixture endpoint/credential binding refused')

def interrupted(*_):raise RuntimeError('owned exact-head gates interrupted')

def audited(operation,verify_cleanup):
    try:
        result=operation()
    except BaseException as primary:
        try:verify_cleanup()
        except BaseException as cleanup:
            raise RuntimeError('gate failure plus cleanup uncertainty: '+str(cleanup)) from primary
        raise
    verify_cleanup()
    return result

def main():
    parser=argparse.ArgumentParser();parser.add_argument('head');parser.add_argument('--filter');args=parser.parse_args()
    if not re.fullmatch('[a-f0-9]{40}',args.head):raise RuntimeError('exact source commit required')
    data,password=load_fixture('/tmp/omg370-pg.json')
    helpers=runpy.run_path(str(R/'.local/verifier382-local-e2e.py'))
    info=json.loads(helpers['run'](['docker','inspect',data['cid']]))
    if len(info)!=1:raise RuntimeError('ambiguous fixture identity')
    validate_container(data,password,info[0])
    for sig in (signal.SIGTERM,signal.SIGHUP):signal.signal(sig,interrupted)
    env={**os.environ,'PATH':str(R/'.local/steering370-bin')+':'+str(R/'.local/toolchains/1.99.1/bin')+':/Users/nicholascecere/.local/share/pi-node/node-v22.23.0-darwin-arm64/bin:/run/current-system/sw/bin:/Users/nicholascecere/.local/bin:/etc/profiles/per-user/nicholascecere/bin:'+os.environ['PATH']+':/usr/sbin:/sbin',
         'DATABASE_URL':data['url'],'CARGO_TARGET_DIR':str(R/'target-wt'),'CARGO_BUILD_JOBS':'1','CARGO_INCREMENTAL':'0','CARGO_PROFILE_DEV_DEBUG':'0','CARGO_PROFILE_TEST_DEBUG':'0','CARGO_TERM_COLOR':'never','FILESTORE_REQUIRE_DOCKER':'1'}
    for key in ('GATEWAY_ADMISSION_MODE','BROWSER_PROJECT'):env.pop(key,None)
    def source_is_frozen():
        head=subprocess.check_output(['git','rev-parse','HEAD'],cwd=W,text=True).strip()
        status=subprocess.check_output(['git','status','--porcelain'],cwd=W,text=True)
        if head!=args.head or status:raise RuntimeError('source changed or dirty during exact-head gates')
    source_is_frozen()
    before_containers=set(helpers['run'](['docker','ps','-aq','--no-trunc']).split())
    def no_unclassified_containers():
        remaining=set(helpers['run'](['docker','ps','-aq','--no-trunc']).split())-before_containers
        if remaining:
            raise RuntimeError('new local containers remain; ownership/cleanup uncertain, do not auto-delete: '+','.join(sorted(remaining)))
    logs=audited(lambda:run_gates(args,data,password,env,source_is_frozen),no_unclassified_containers)
    print(('FOCUSED' if args.filter is not None else 'FULL')+' EXACT-HEAD GATES PASSED '+args.head+'; private logs '+str(logs)+'; fixture cleanup remains owned by managed PG process',flush=True)

def run_gates(args,data,password,env,source_is_frozen):
    logs=Path(tempfile.mkdtemp(prefix='omg370-full-'));print('EXACT HEAD '+args.head+'; private logs '+str(logs),flush=True)
    def step(name,command):
        print('== '+name,flush=True);path=logs/(name+'.log')
        with path.open('w') as file:
            # Children remain in the managed process group; process.stop owns cancellation.
            result=subprocess.run(command,cwd=W,env=env,stdout=file,stderr=subprocess.STDOUT)
        if result.returncode:raise RuntimeError('gate failed: '+name+'; private log '+str(path))
        return path
    cargo=['cargo','test','-p','open-model-gateway','--all-features']
    if args.filter is not None:
        step('focused',cargo+['--lib',args.filter]);source_is_frozen();return logs
    step('npm-ci',['npm','ci','--no-audit','--no-fund'])
    step('fmt',['cargo','fmt','--all','--','--check'])
    step('clippy',['cargo','clippy','--workspace','--all-targets','--all-features','--','-D','warnings'])
    step('rust-default',['cargo','test','--workspace'])
    step('rust-all-features',['cargo','test','--workspace','--all-features'])
    step('ci-inventory',['node','.github/scripts/rust-shards.mjs','--shard','0','--count','4'])
    probes=runpy.run_path(str(R/'.local/steering370-probes.py'))
    for name,target in [('runtime',['--test','runtime_privileges']),('contention',['--lib',next(iter(probes['PROBES']['contention']))]),('terminal',['--lib',next(iter(probes['PROBES']['terminal']))])]:
        selection=['--ignored']+(['--exact'] if name!='runtime' else [])
        listing=step(name+'-list',cargo+target+['--']+selection+['--list'])
        probes['verify'](listing.read_text(),name,'list')
        command=cargo+target+['--']+selection+['--test-threads=1']
        if name=='contention':command=['env','RUST_TEST_NOCAPTURE=1',*command]
        result=step(name+'-run',command)
        probes['verify'](result.read_text(),name,'run')
    for name,command in [
        ('typecheck',['npm','run','typecheck:web']),('web-tests',['npm','run','test:web']),
        ('supply-chain',['npm','run','-s','test:supply-chain']),('build-web',['npm','run','build:web']),
        ('gateway-build',['cargo','build','-p','open-model-gateway'])]:step(name,command)
    env.update(OMG_BROWSER_GATEWAY_BIN=str(R/'target-wt/debug/open-model-gateway'),OMG_BROWSER_PORT_BASE='19400',OMG_BROWSER_ADMIN_DATABASE_URL=data['url'])
    for name,command in [
        ('browser',['npm','run','test:browser']),('ci',['npm','run','-s','test:ci']),
        ('oci-tool',['node','--test','tests/oci-tool.test.mjs']),('oci-python',['python3','-B','-m','unittest','discover','-s','tests','-p','oci_*test.py']),
        ('conflict-markers',['npm','run','-s','test:conflict-markers']),('sdk-bases',['npm','run','-s','test:sdk-bases']),
        ('demo',['npm','run','-s','test:demo']),('helm',['npm','run','-s','test:helm']),('staging',['npm','run','-s','test:staging']),
        ('audit',['npm','audit','--omit=dev','--audit-level=low']),('container',['npm','run','-s','test:container'])]:step(name,command)
    env.update(BACKUP_TEST_DATABASE_URL='postgres://postgres:'+password+'@127.0.0.1:5432/postgres',BACKUP_TEST_PG_CONTAINER=data['cid'],BACKUP_TEST_GATEWAY_BINARY=str(R/'target-wt/debug/open-model-gateway'))
    step('backup-build',['cargo','build','-p','open-model-gateway','--all-features','--bin','open-model-gateway'])
    step('backup-real-postgres',['python3','-B','tests/backup.test.py'])
    source_is_frozen();return logs

if __name__=='__main__':main()
