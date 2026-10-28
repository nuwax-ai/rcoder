"""Isolated UserApp retained-PVC recovery; fault injection never touches baseline apps."""
import concurrent.futures
import json
import re
import subprocess
import time
from k8s_hot_recovery import exact_patch, kube_json_input

REQUIRED = frozenset({
    'cluster_ready', 'concurrent_ensure', 'concurrent_error_contract', 'ensure_retry',
    'builder_identity', 'lifecycle_active', 'cross_replica_files', 'build_A', 'artifact_A',
    'worker_artifact_contract', 'default_recovery_seed_prod', 'default_recovery_seed_data',
    'default_recovery_workloads_removed', 'default_recovery_pg_fault',
    'default_recovery_legacy_pvc', 'default_recovery_dev_restart',
    'default_recovery_identity', 'default_recovery_dev_git', 'build_B', 'artifact_B',
    'default_recovery_prod_traffic', 'default_recovery_single_instance',
    'default_recovery_data', 'default_recovery_pvc_uids', 'default_recovery_legacy_fence_injected',
    'default_recovery_legacy_fence_terminal', 'default_recovery_retry_after_fence', 'cleanup', 'baseline_preserved',
})


def delete_compute(run, kind, name):
    if kind not in ('StatefulSet', 'Deployment') or name not in (
            'rcoder-app-builder-' + run.app, 'rcoder-app-' + run.app):
        raise AssertionError('Refusing deletion outside this fixture')
    row = json.loads(run.kube('get', kind, name, '-o', 'json'))
    if row['metadata']['uid'] in {r['uid'] for r in run.baseline}:
        raise AssertionError('Refusing deletion of baseline UID')
    plural = {'StatefulSet': 'statefulsets', 'Deployment': 'deployments'}[kind]
    path = '/apis/apps/v1/namespaces/' + run.args.namespace + '/' + plural + '/' + name
    kube_json_input(run, 'delete', {'apiVersion': 'v1', 'kind': 'DeleteOptions',
        'propagationPolicy': 'Foreground', 'preconditions': {
            'uid': row['metadata']['uid'], 'resourceVersion': row['metadata']['resourceVersion']}},
        '--raw', path, '-f', '-')


def sql(run, statement):
    if not re.fullmatch(r'rcoder-e2e-[a-z0-9-]+', run.args.namespace):
        raise AssertionError('Database faults require an owned isolated namespace')
    if not re.fullmatch(r'e2e-k8s-[0-9a-f]{14}', run.app):
        raise AssertionError('Database fault application is not this fixture')
    return run.kube('exec', 'postgres-0', '--', 'psql', '-X', '-v', 'ON_ERROR_STOP=1',
                    '-U', 'rcoder', '-d', 'rcoder', '-Atc', statement)


def deletion_order(tables, edges):
    pending = set(tables)
    dependencies = []
    for edge in edges:
        if len(edge) not in (2, 3):
            raise AssertionError('Unexpected foreign key metadata')
        child, parent = edge[:2]
        deferred = len(edge) == 3 and edge[2] is True
        # This declared initially-deferred back-reference is checked at COMMIT.
        # Delete only the fixture sessions, then their exact bindings; never
        # disable constraints or ignore an unknown external dependency.
        if deferred and child in pending and parent in pending and (child, parent) == ('userapp_agent_bindings', 'userapp_agent_sessions'):
            continue
        dependencies.append((child, parent))
    edges = dependencies
    if any(not re.fullmatch(r'userapps|userapp_[a-z_]+', name) for name in pending):
        raise AssertionError('Unexpected fault injection table')
    if any(parent in pending and child not in pending for child, parent in edges):
        raise AssertionError('A non-fixture table references the fault inventory')
    ordered = []
    while pending:
        ready = sorted(table for table in pending if not any(child in pending and parent == table and child != table for child, parent in edges))
        if not ready:
            raise AssertionError('Unexpected cyclic UserApp schema; no fault was applied')
        ordered.extend(ready)
        pending.difference_update(ready)
    return ordered


def dev_restart(run):
    deadline = time.monotonic() + 240
    while True:
        status, envelope = run.request('/api/v1/userapp/dev/restart',
            {'app_id':run.app,'user_id':run.user,'pg':{'username':'dev','password':'dev'}}, timeout=180)
        if status == 200 and envelope.get('success') is True:
            break
        if (envelope.get('code') not in ('ERR_USERAPP_WAIT_TIMEOUT','ERR_OPERATION_IN_PROGRESS')
                or (envelope.get('data') or {}).get('task_id') or time.monotonic() >= deadline):
            raise AssertionError('Original developer restart failed: '+str(envelope))
        time.sleep(1)
    task_id = (envelope.get('data') or {}).get('task_id')
    if not task_id:
        raise AssertionError('Developer restart did not return its real task identity')
    task_path = '/api/v1/userapp/tasks/'+task_id+'?app_id='+run.app+'&user_id='+run.user
    task = run.poll(lambda: run.api(task_path,base=run.entries[-1]),lambda row:row.get('status') in ('completed','failed','cancelled'),180)
    if task.get('status') != 'completed':
        raise AssertionError('Developer restart task failed: '+str(task))
    return task


def delete_fixture_metadata(run):
    """Delete only this fixture's rows in FK order; never disable constraints."""
    tables = json.loads(sql(run, "SELECT json_agg(table_name ORDER BY table_name) FROM information_schema.columns WHERE table_schema='public' AND ((column_name='app_id' AND (table_name='userapps' OR table_name LIKE 'userapp\\_%' ESCAPE '\\')) OR (table_name='userapp_agent_sessions' AND column_name='binding_generation'))"))
    edges = json.loads(sql(run, "SELECT coalesce(json_agg(json_build_array(c.relname,p.relname,f.condeferrable AND f.condeferred)),'[]'::json) FROM pg_constraint f JOIN pg_class c ON c.oid=f.conrelid JOIN pg_class p ON p.oid=f.confrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE f.contype='f' AND n.nspname='public'"))
    ordered = deletion_order(tables, edges)
    run.save('default-recovery-pg-fault.json', {'app_id': run.app, 'tables': ordered})
    statements = ['BEGIN']
    statements += fixture_deletions(ordered, run.app)
    statements += ['COMMIT', f"SELECT count(*) FROM userapps WHERE app_id='{run.app}'"]
    result = sql(run, ';'.join(statements))
    if result.strip().splitlines()[-1] != '0':
        raise AssertionError('Fixture identity was not removed')



def fixture_deletions(tables, app_id):
    if not re.fullmatch(r'e2e-k8s-[0-9a-f]{14}', app_id):
        raise AssertionError('Refusing non-fixture deletion')
    statements = []
    for table in tables:
        if not re.fullmatch(r'userapps|userapp_[a-z_]+', table):
            raise AssertionError('Refusing non-UserApp table deletion')
        if table == 'userapp_agent_sessions':
            statements.append("DELETE FROM userapp_agent_sessions WHERE binding_generation IN "
                              "(SELECT binding_generation FROM userapp_agent_bindings WHERE app_id='"+app_id+"')")
        else:
            statements.append(f'DELETE FROM "{table}" WHERE app_id=\'{app_id}\'')
    return statements



def strip_legacy_identity(run, name, expected_uid):
    if (not re.fullmatch(r'rcoder-e2e-[a-z0-9-]+', run.args.namespace)
            or not re.fullmatch(r'e2e-k8s-[0-9a-f]{14}', run.app)) or name not in (
            'rcoder-app-'+run.app+'-workspace', 'rcoder-app-builder-'+run.app+'-workspace'):
        raise AssertionError('Refusing metadata fault outside this fixture')
    keys = ('rcoder.io/application-id', 'rcoder.io/lifecycle-id', 'rcoder.io/operation-id',
            'rcoder.io/executor-id', 'rcoder.io/request-fingerprint')
    last_error = None
    for _attempt in range(4):
        row = json.loads(run.kube('get', 'pvc', name, '-o', 'json'))
        if row['metadata']['uid'] != expected_uid:
            raise AssertionError('Original PVC was replaced before metadata fault')
        if (row['metadata'].get('labels') or {}).get('app.kubernetes.io/managed-by') != 'rcoder-runtime':
            raise AssertionError('Refusing an unmanaged PVC metadata fault')
        annotations = row['metadata'].get('annotations') or {}
        if annotations.get('rcoder.io/application-id') not in (None, run.app):
            raise AssertionError('Refusing another application metadata fault')
        if annotations.get('rcoder.io/lifecycle-id') not in (None, run.lifecycle_id):
            raise AssertionError('Refusing another lifecycle metadata fault')
        changes = [{'op':'remove', 'path':'/metadata/annotations/'+key.replace('~','~0').replace('/','~1')}
                   for key in keys if key in annotations]
        if not changes:
            return row
        try:
            # kubectl patch --patch-file=- does not read stdin on supported
            # clients. This body contains only UID/RV and annotation names;
            # Run.kube passes its structured argument through shlex.join.
            result = json.loads(run.kube('patch', 'pvc', name, '--type=json',
                '--patch', json.dumps(exact_patch(row, changes)), '-o', 'json'))
            if result['metadata']['uid'] != expected_uid:
                raise AssertionError('Metadata fault returned a different PVC')
            if any(key in (result['metadata'].get('annotations') or {}) for key in keys):
                raise AssertionError('Metadata fault did not remove captured identity keys')
            return result
        except subprocess.CalledProcessError as error:
            # Reobserve before retry: an unknown acknowledgement may already
            # have removed the keys. New CAS never targets a replaced volume.
            last_error = error
    raise AssertionError('PVC metadata fault could not be confirmed') from last_error


def legacy_fence_round(run):
    """Reproduce the acknowledged old identity-check failure on this fixture only."""
    delete_compute(run,'StatefulSet','rcoder-app-builder-'+run.app)
    run.poll(lambda:[r for r in run.inventory() if r['kind']=='Pod' and r['name'].startswith('rcoder-app-builder-'+run.app+'-')],lambda rows:not rows,180)
    claim_name='rcoder-app-builder-'+run.app+'-workspace'
    claim=json.loads(run.kube('get','pvc',claim_name,'-o','json'))
    strip_legacy_identity(run,claim_name,claim['metadata']['uid'])
    operation='legacy-pref-'+run.id
    message='ensure UserappBuilder failed: Concurrent resource update: Application resource identity mismatch: rcoder.io/application-id'
    # All compute is confirmed gone and no script/database migration is issued.
    # This is an explicit fixture fault, not a fabricated production outcome.
    statement=f"""BEGIN;
INSERT INTO userapp_operations(operation_id,app_id,lifecycle_id,kind,scope,state,revision,request_fingerprint,executor_id,step,error_code,error_message,payload_version,checkpoint_json,created_at_us,updated_at_us)
SELECT '{operation}',app_id,lifecycle_id,'ensure_builder','dev','recovery_required',2,'{'a'*64}','legacy-exited','creation_result','ERR_BACKEND_ERROR','{message}',1,'null',CAST(extract(epoch from now())*1000000 AS BIGINT),CAST(extract(epoch from now())*1000000 AS BIGINT)
FROM userapps WHERE app_id='{run.app}' AND lifecycle_state='active';
UPDATE userapp_active_operations SET dev_operation_id='{operation}' WHERE app_id='{run.app}' AND dev_operation_id IS NULL AND application_operation_id IS NULL;
COMMIT; SELECT operation_id FROM userapp_operations WHERE operation_id='{operation}';"""
    output=sql(run,statement)
    run.check('default_recovery_legacy_fence_injected',output.strip().splitlines()[-1]==operation,{'operation_id':operation})
    task=dev_restart(run)
    run.check('default_recovery_retry_after_fence',task.get('status')=='completed',task)
    state=sql(run,f"SELECT state FROM userapp_operations WHERE app_id='{run.app}' AND operation_id='{operation}'").strip()
    run.check('default_recovery_legacy_fence_terminal',state=='failed',{'operation_id':operation,'state':state})


def exercise(run):
    artifact, release = run.build('A')
    result = run.api('/api/v1/userapp/' + run.app + '/start',
        {'user_id': run.user, **artifact, 'release_id': release, 'request_id': 'recovery-seed-' + run.id}, timeout=360)
    run.check('default_recovery_seed_prod', run.content(run.id + '-A'), result)
    pod = run.prod_pod()
    sentinel = 'default-recovery-' + run.id
    run.kube('exec', pod['name'], '-c', 'app', '--', 'sh', '-c',
             'printf %s ' + sentinel + ' > /home/user/data/default-recovery-proof')
    run.check('default_recovery_seed_data', run.kube('exec', pod['name'], '-c', 'app', '--',
              'cat', '/home/user/data/default-recovery-proof') == sentinel)
    claims = {r['name']: r['uid'] for r in run.inventory() if run.owned(r) and r['kind'] == 'PersistentVolumeClaim'}
    if len(claims) != 2:
        raise AssertionError('Expected exactly this fixture dev/prod claims')
    delete_compute(run, 'Deployment', 'rcoder-app-' + run.app)
    delete_compute(run, 'StatefulSet', 'rcoder-app-builder-' + run.app)
    absent = run.poll(lambda: [r for r in run.inventory() if run.owned(r) and r['kind'] in ('Pod','Deployment','StatefulSet')], lambda rows: not rows, 180)
    run.check('default_recovery_workloads_removed', not absent)
    delete_fixture_metadata(run)
    run.check('default_recovery_pg_fault', True)
    for name, uid in claims.items():
        strip_legacy_identity(run, name, uid)
    run.check('default_recovery_legacy_pvc', True)
    # A real business restart must first recreate the developer container.
    task = dev_restart(run)
    run.check('default_recovery_dev_restart', task.get('status') == 'completed', task)
    view = run.lifecycle()
    run.lifecycle_id = view['lifecycle_id']
    run.check('default_recovery_identity', view['state']=='Active' and bool(view['lifecycle_id']), view)
    legacy_fence_round(run)
    git = run.dev_git('status', query=True)
    run.check('default_recovery_dev_git', git is not None, git)
    run.build('B')
    # Concurrent first prod traffic must recover original A, never publish the newly built B.
    path = f'/api/v1/userapp/proxy/app/prod/{run.user}/{run.app}/'
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        replies = list(pool.map(lambda entry: run.request(path,base=entry,raw=True,timeout=180), run.proxy_entries[:2]))
    run.check('default_recovery_prod_traffic', all(status==200 and body.decode()==run.id+'-A' for status,body in replies),
              [{'status':s,'body':b.decode(errors='replace')[:200]} for s,b in replies])
    resources = run.inventory()
    prod = [r for r in resources if r['kind']=='Pod' and r['name'].startswith('rcoder-app-'+run.app+'-')]
    dev = [r for r in resources if r['kind']=='Pod' and r['name'].startswith('rcoder-app-builder-'+run.app+'-')]
    run.check('default_recovery_single_instance', len(prod)==1 and len(dev)==1, {'prod':prod,'dev':dev})
    run.check('default_recovery_data', run.kube('exec',prod[0]['name'],'-c','app','--','cat','/home/user/data/default-recovery-proof')==sentinel)
    after = {r['name']:r['uid'] for r in resources if run.owned(r) and r['kind']=='PersistentVolumeClaim'}
    run.check('default_recovery_pvc_uids', after==claims, {'before':claims,'after':after})


def cleanup(run):
    """Retain this scenario's volumes as recovery evidence; delete compute only."""
    resources = run.inventory()
    for kind, name in [('Deployment','rcoder-app-'+run.app),('StatefulSet','rcoder-app-builder-'+run.app)]:
        if any(r['kind']==kind and r['name']==name for r in resources):
            delete_compute(run,kind,name)
    rows = run.poll(lambda: [r for r in run.inventory() if run.owned(r) and r['kind'] in ('Pod','Deployment','StatefulSet')],lambda rows:not rows,180)
    run.check('cleanup', not rows, rows)
    run.save('retained-recovery-pvcs.json',[r for r in run.inventory() if run.owned(r) and r['kind']=='PersistentVolumeClaim'])
