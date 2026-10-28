"""Real owner OIDC sign-in; credentials remain in memory/stdin, never argv/logs."""
import json
import os
import subprocess

SESSION = 'omg-v090-production'


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def browser(*args, code=None):
    result = subprocess.run(['agent-browser', '--session', SESSION, '--allowed-domains',
        'platform.bitop.dev,login.bitop.dev', '--json', *args],
        input=code, capture_output=True, text=True, timeout=40)
    require(result.returncode == 0, 'browser operation failed; private details withheld')
    data = json.loads(result.stdout)
    require(data.get('success') is True, 'browser operation refused; private details withheld')
    data = data.get('data', {})
    return data.get('result', data)


def main(expected_email='nick@bitop.dev', expected_username=None):
    browser('open', 'https://platform.bitop.dev/')
    preflight = browser('eval', '--stdin', code="(async()=>{if(location.origin!=='https://platform.bitop.dev')return 0;return (await fetch('/api/v1/me',{redirect:'error',cache:'no-store'})).status;})()")
    require(preflight == 401, 'fresh unauthenticated gateway session required')
    browser('open', 'https://platform.bitop.dev/api/v1/auth/login')
    browser('wait', '--fn', "(()=>{if(location.origin!=='https://login.bitop.dev')return false;const f=(r,s)=>!!r.querySelector(s)||[...r.querySelectorAll('*')].some(e=>e.shadowRoot&&f(e.shadowRoot,s));return ['input[name=uidField]','input[type=password]','button[type=submit]'].every(s=>f(document,s))})()")
    origin = browser('eval', '--stdin', code='location.origin')
    require(origin == 'https://login.bitop.dev', 'unexpected authentication origin')
    if origin == 'https://login.bitop.dev':
        # Existing operator-authorized acceptance identity, already used for v0.8.
        # Never print vault output or forward it to a shell argument.
        result = subprocess.run(['bao', 'kv', 'get', '-format=json',
            'secret/kubernetes/open-model-gateway/acceptance'],
            env={**os.environ, 'BAO_ADDR': 'https://secrets.bitop.dev'},
            capture_output=True, text=True, timeout=20)
        require(result.returncode == 0, 'acceptance credential unavailable')
        credentials = json.loads(result.stdout)['data']['data']
        if expected_username is not None:
            require(credentials.get('username') == expected_username,
                'stored login does not match the explicitly approved acceptance account')
        require(all(isinstance(credentials.get(k), str) and credentials[k]
            for k in ('username', 'password')), 'acceptance credential incomplete')
        code = '''(()=>{if(location.origin!=='https://login.bitop.dev')return {ok:false};const credentials=CREDENTIAL_BUNDLE;
const deep=(r,s)=>{let e=r.querySelector(s);if(e)return e;for(const n of r.querySelectorAll('*'))if(n.shadowRoot){e=deep(n.shadowRoot,s);if(e)return e;}return null};
const u=deep(document,'input[name=uidField]'),p=deep(document,'input[type=password]'),b=deep(document,'button[type=submit]');if(!u||!p||!b)return {ok:false};
const fill=(i,v)=>{Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(i,v);i.dispatchEvent(new Event('input',{bubbles:true,composed:true}));i.dispatchEvent(new Event('change',{bubbles:true,composed:true}));};
fill(u,credentials.username);fill(p,credentials.password);b.click();return {ok:true};})()'''
        code = code.replace('CREDENTIAL_BUNDLE', json.dumps({
            'username': credentials['username'], 'password': credentials['password']}))
        result = browser('eval', '--stdin', code=code)
        del code, credentials
        require(result.get('ok') is True, 'login form could not be submitted')
        browser('wait', '--fn', "location.origin==='https://platform.bitop.dev'&&!location.pathname.startsWith('/api/v1/auth/')")
    result = browser('eval', '--stdin', code="""(async()=>{if(location.origin!=='https://platform.bitop.dev')return {status:0};const r=await fetch('/api/v1/me',{cache:'no-store',redirect:'error'});const j=await r.json();const role=j.platform_role||j.user?.platform_role||j.platform?.role;return {status:r.status,expectedOwner:(j.user?.email||j.email)===EXPECTED_APPROVED_EMAIL,platformAdmin:role==='admin',platformRole:['admin','auditor','user'].includes(role)?role:'unavailable',platformWrite:j.capabilities?.platform_write===true,noAccessPage:document.body.innerText.includes('No access yet'),authErrorPresent:new URL(location.href).searchParams.has('auth_error')};})()""".replace('EXPECTED_APPROVED_EMAIL', json.dumps(expected_email)))
    print(json.dumps({'safe_oidc_result': result}), flush=True)
    require(result.get('status') == 200 and result.get('expectedOwner') is True,
        'explicitly approved OIDC identity not established')
    print(json.dumps({'real_oidc_approved_identity_signin': True, 'management_me_status': 200,
        'credentials_printed': False}), flush=True)


if __name__ == '__main__':
    main()
