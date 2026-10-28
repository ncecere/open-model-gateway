import os,subprocess,json
E={k:v for k,v in os.environ.items() if k.upper() not in ('HTTPS_PROXY','HTTP_PROXY','ALL_PROXY')}
E.update({'KUBECONFIG':'/Users/nicholascecere/homelab/bitop-talos/kubeconfig','KUBECTL_REMOTE_COMMAND_WEBSOCKETS':'false'});NS='open-model-gateway'
def run(a,input=None):
 r=subprocess.run(a,input=input,env=E,capture_output=True,text=True)
 if r.returncode:
  # Only the database error heading, never SQL context, values or environment.
  lines=[x for x in r.stderr.splitlines() if x.startswith('ERROR:') or x.startswith('psql:') and 'ERROR:' in x]
  if lines:print('Verifier failure: '+lines[0],flush=True)
  raise RuntimeError('Operator command failed')
 return r.stdout
K=['kubectl','--context','admin@bitop-talos','--request-timeout=60s']
def g(*a):return json.loads(run(K+['-n',NS,'get',*a,'-o','json']))
assert g('namespace',NS) if False else True
assert json.loads(run(K+['get','namespace',NS,'-o','json']))['metadata']['uid']=='34d599f7-7cdf-4a13-af3a-e128f5cf33fb'
c=g('cluster','open-model-gateway-db');assert c['metadata']['uid']=='b55348fe-08b5-46b1-9c21-e2586a70485f'
h=g('helmrelease','omg');hist=h['status']['history'][0];assert hist['status']=='deployed' and hist['chartVersion']=='0.9.3+1ea748655844'
primary=c['status']['currentPrimary']
def sql(q):return run(K+['-n',NS,'exec','-i',primary,'-c','postgres','--','psql','-X','-U','postgres','-d','gateway','-v','ON_ERROR_STOP=1','-At'],q)
assert sql("SELECT count(*)||'|'||max(version)||'|'||bool_and(success) FROM _sqlx_migrations;").strip()=='66|67|true'
root='/Users/nicholascecere/projects/rust/open-model-gateway'
assert run(['git','-C',root,'rev-parse','v0.9.3^{commit}']).strip()=='1ea74865584429892e61313f3c944ca1e6ed39a9'
script=run(['git','-C',root,'show','v0.9.3:deploy/staging/verify-privileges.sql']);assert script.rstrip().endswith('ROLLBACK;')
counts="SELECT (SELECT count(*) FROM users)||'|'||(SELECT count(*) FROM workspaces)||'|'||(SELECT count(*) FROM api_keys)||'|'||(SELECT count(*) FROM admin_api_tokens)||'|'||(SELECT count(*) FROM governance_reservations)||'|'||(SELECT count(*) FROM monetary_ledger)||'|'||(SELECT count(*) FROM inference_executions);"
before=sql(counts);sql(script);after=sql(counts);assert after==before,'probe counts changed'
print('STRICT TAGGED PRIVILEGE VERIFIER PASSED on production: complete unchanged verify-privileges.sql from tag v0.9.3 (1ea74865), postgres peer operator on primary %s, ends in ROLLBACK; lineage 66 records / max 67 all successful; probe row counts unchanged; HelmRelease omg deployed chart 0.9.3+1ea748655844.'%primary,flush=True)
