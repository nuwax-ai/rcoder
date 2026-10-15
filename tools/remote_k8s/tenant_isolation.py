"""Load the real chart's ingress policy, then verify existing tenant endpoints.

This is an opt-in acceptance command for an owned remote-k8s namespace. It does
not create listeners, alter shared Gateway/CNI settings, or delete data/workloads.
"""
import hashlib
import ipaddress
import json
from pathlib import Path
import subprocess
import time
import uuid

from common import LABEL, atomic_json


TENANTS = {'rcoder-runtime', 'rcoder-app-manager'}
COMPONENT = 'app.kubernetes.io/component'
MANAGER = 'app.kubernetes.io/managed-by'


def checked_pods(c):
    pods = c.obj('get', 'pods')['items']
    ready = [p for p in pods if not p['metadata'].get('deletionTimestamp')
             and any(x['type'] == 'Ready' and x['status'] == 'True'
                     for x in p.get('status', {}).get('conditions', []))]
    main = [p for p in ready if p['metadata'].get('labels', {}).get(LABEL) == c.id
            and p['metadata']['labels'].get('app') == 'rcoder']
    if not main or any(p['metadata']['labels'].get(COMPONENT) != 'rcoder-main' for p in main):
        raise RuntimeError('Deploy the main Pod template label and wait for rollout before isolation')
    tenants = [p for p in ready if p['metadata'].get('labels', {}).get(MANAGER) in TENANTS]
    if len(tenants) < 2:
        raise RuntimeError('At least two Ready runtime tenant Pods are required; create them via UserApp APIs')
    return main, tenants


def audit(c, path):
    # Audit code is transmitted on stdin; it never reads Secret data.
    wrapper = ('import json,sys; ns={"__name__":"tenant_audit"}; '
               'exec(compile(sys.stdin.read(),"tenant_audit.py","exec"),ns); '
               'print(json.dumps(ns["Audit"](sys.argv[1],sys.argv[2]).run()))')
    return json.loads(c.ssh(['python3', '-c', wrapper, c.context, c.ns], path.read_text(), timeout=120))


def render(c, chart):
    import yaml
    name = 'rcoder-isolation-' + c.id
    output = subprocess.check_output([
        'helm', 'template', name, str(chart), '--namespace', c.ns,
        '--set', 'rcoder.enabled=false', '--set', 'networkPolicy.enabled=false',
        '--set', 'rcoder.networkPolicy.enabled=false', '--set', 'networkPolicy.userappIsolation=true',
        '--show-only', 'templates/rcoder/tenant-ingress.yaml'], text=True)
    resources = [x for x in yaml.safe_load_all(output) if x]
    if len(resources) != 1 or resources[0].get('kind') != 'NetworkPolicy':
        raise RuntimeError('Chart did not render exactly one tenant ingress policy')
    resource = resources[0]
    if resource['metadata'].get('namespace') != c.ns:
        raise RuntimeError('Rendered policy namespace mismatch')
    if resource['spec'].get('policyTypes') != ['Ingress']:
        raise RuntimeError('This acceptance entry only installs the chart ingress fallback')
    # Ownership metadata does not change the policy spec.
    resource['metadata'].setdefault('labels', {})[LABEL] = c.id
    inputs = sorted(chart.rglob('*.yaml')) + sorted(chart.rglob('*.tpl'))
    source = hashlib.sha256()
    for path in inputs:
        source.update(str(path.relative_to(chart)).encode() + b'\0' + path.read_bytes() + b'\0')
    return resource, {'chart': str(chart), 'chart_sha256': source.hexdigest(),
                      'rendered_sha256': hashlib.sha256(output.encode()).hexdigest()}


def connect(c, pod, host, port):
    # A command/tool failure is distinct from a completed TCP attempt.
    code = '''import errno,json,socket,sys
try:
 s=socket.create_connection((sys.argv[1],int(sys.argv[2])),3);s.close()
 print(json.dumps({'connected':True}))
except OSError as e:
 print(json.dumps({'connected':False,'errno':errno.errorcode.get(e.errno),'timeout':isinstance(e,TimeoutError)}))
'''
    name = pod['metadata']['name']
    container = 'rcoder' if pod['metadata'].get('labels', {}).get('app') == 'rcoder' else pod['spec']['containers'][0]['name']
    result = json.loads(c.kube('exec', name, '-c', container, '--', 'python3', '-c', code, host, str(port)))
    # A source with no route, unavailable local address, or exhausted sockets
    # cannot prove destination ingress enforcement. Timeout/refusal still need
    # the caller's same-endpoint positive controls and policy-union audit.
    if result.get('connected') is False:
        if not result.get('timeout') and result.get('errno') not in ('ETIMEDOUT', 'ECONNREFUSED'):
            raise RuntimeError(f'TCP observation inconclusive from {name}: {result}')
    elif result.get('connected') is not True:
        raise RuntimeError(f'TCP observation inconclusive from {name}: {result}')
    return result


def target_routes(pod, services, node_ips):
    """Service targetPort is valid even when the Pod declares no containerPort."""
    labels = pod['metadata'].get('labels', {})
    declared = [p for ct in pod['spec']['containers'] for p in ct.get('ports', [])
                if p.get('protocol', 'TCP') == 'TCP']
    named = {p['name']: p['containerPort'] for p in declared if p.get('name')}
    ports = {p['containerPort'] for p in declared}
    matches = []
    for service in services:
        spec = service['spec']
        selector = spec.get('selector')
        if not selector or not all(labels.get(k) == v for k, v in selector.items()):
            continue
        for port in spec.get('ports', []):
            if port.get('protocol', 'TCP') != 'TCP':
                continue
            target = port.get('targetPort', port['port'])
            if isinstance(target, str):
                if target not in named:
                    raise RuntimeError('Unresolved Service targetPort: ' + service['metadata']['name'] + '/' + target)
                target = named[target]
            ports.add(target)
            matches.append((spec, port, target))
    ips = [entry['ip'] for entry in pod['status'].get('podIPs', [])]
    if not ips:
        ips = [pod['status']['podIP']]
    for ip in ips:
        family = ipaddress.ip_address(ip).version
        for port in sorted(ports):
            routes = {('pod', ip, port)}
            for spec, service_port, target in matches:
                if target != port:
                    continue
                for address in spec.get('clusterIPs', [spec.get('clusterIP')]):
                    if address and address != 'None' and ipaddress.ip_address(address).version == family:
                        routes.add(('service', address, service_port['port']))
                if service_port.get('nodePort'):
                    routes.update(('nodeport', node, service_port['nodePort']) for node in node_ips
                                  if ipaddress.ip_address(node).version == family)
            yield ip, port, family, sorted(routes)


def run(c):
    from main import owned, alive
    owned(c, 'namespace', c.ns)
    main, tenants = checked_pods(c)
    chart = Path(c.get('ISOLATION_CHART', required=True)).expanduser().resolve()
    audit_path = chart.parents[1] / 'scripts' / 'audit_tenant_network.py'
    if not audit_path.is_file():
        raise RuntimeError('ISOLATION_CHART must point to build-agent-docker/k8s/helm/nuwax-platform')
    before = audit(c, audit_path)
    directory = c.state / 'isolation' / uuid.uuid4().hex
    directory.mkdir(parents=True)
    atomic_json(directory / 'audit-before.json', before)
    print('Tenant preflight audit:', directory / 'audit-before.json', flush=True)
    if any(not x.get('optional', False) for x in before.get('unverified', [])):
        raise RuntimeError('Network audit has unresolved evidence; inspect audit before applying policy')
    permissions = before.get('service_account_permissions', [])
    if not permissions or any(value != 'no' for row in permissions for value in row['checks'].values()):
        raise RuntimeError('Tenant service account privileges must be corrected before network isolation')
    if any(row.get('privileged_bindings') for row in permissions):
        raise RuntimeError('Additional tenant RBAC grants require review before network isolation')
    if before['policy_union']['cilium_policy_names']:
        raise RuntimeError('Existing Cilium-specific policy rules need independent review before this matrix')
    resource, receipt = render(c, chart)
    name = resource['metadata']['name']
    owned(c, 'networkpolicy', name, optional=True)
    receipt.update(environment=c.id, context=c.context, namespace=c.ns, status='running')
    report = {'receipt': receipt, 'checks': [], 'not_covered': [], 'errors': []}
    atomic_json(directory / 'policy.json', resource)
    atomic_json(directory / 'report.json', report)
    try:
        alive(c)
        c.kube('apply', '-f', '-', data=json.dumps(resource))
        policy = owned(c, 'networkpolicy', name)
        receipt.update(policy_uid=policy['metadata']['uid'], policy_resource_version=policy['metadata']['resourceVersion'])
        # CNI convergence is asynchronous. Negative assertions below retry only
        # during this bounded convergence period; later leakage fails immediately.
        converge_until = time.monotonic() + 20
        after = audit(c, audit_path)
        atomic_json(directory / 'audit-after.json', after)
        if after.get('findings') or any(not x.get('optional', False) for x in after.get('unverified', [])):
            raise RuntimeError('Effective policy/RBAC audit did not pass; see audit-after.json')
        policies = c.obj('get', 'networkpolicy')['items']
        # An existing source Egress restriction could independently cause a drop.
        # Do not claim this matrix proves target ingress in that topology.
        from importlib.util import spec_from_file_location, module_from_spec
        spec = spec_from_file_location('tenant_audit', audit_path)
        module = module_from_spec(spec)
        spec.loader.exec_module(module)
        for source in tenants:
            for policy in policies:
                if 'Egress' in policy['spec'].get('policyTypes', []) and module.matches(policy['spec'].get('podSelector', {}), source['metadata'].get('labels', {})):
                    raise RuntimeError('Source Egress restriction needs independent CNI evidence; matrix cannot prove ingress alone')
        services = c.obj('get', 'services')['items']
        nodes = c.obj('get', 'nodes')['items']
        nodes_ips = [x['address'] for n in nodes for x in n.get('status', {}).get('addresses', []) if x['type'] == 'InternalIP']
        for target in tenants:
            for ip, port, family, endpoints in target_routes(target, services, nodes_ips):
                if not connect(c, main[0], ip, port)['connected']:
                    report['not_covered'].append({'target': target['metadata']['name'], 'port': port, 'reason': 'no RCoder positive control'})
                    continue
                for source in tenants:
                    if source['metadata']['uid'] == target['metadata']['uid']:
                        continue
                    source_ips = source['status'].get('podIPs', [{'ip': source['status']['podIP']}])
                    if not any(ipaddress.ip_address(entry['ip']).version == family for entry in source_ips):
                        report['not_covered'].append({'source': source['metadata']['name'], 'family': family,
                                                      'reason': 'source Pod has no address in target family'})
                        continue
                    for route, host, destination in endpoints:
                        positive = connect(c, main[0], host, destination)
                        if not positive['connected']:
                            raise RuntimeError('RCoder cannot reach live target via ' + route)
                        negative = connect(c, source, host, destination)
                        while negative['connected'] and time.monotonic() < converge_until:
                            time.sleep(1)
                            negative = connect(c, source, host, destination)
                        positive_after = connect(c, main[0], host, destination)
                        row = {'source': source['metadata']['name'], 'source_uid': source['metadata']['uid'],
                               'target': target['metadata']['name'], 'target_uid': target['metadata']['uid'],
                               'source_node': source['spec']['nodeName'], 'target_node': target['spec']['nodeName'],
                               'route': route, 'host': host, 'port': destination, 'family': family,
                               'blocked': not negative['connected'], 'negative': negative,
                               'positive_before': positive, 'positive_after': positive_after}
                        row['passed'] = row['blocked'] and positive_after['connected']
                        report['checks'].append(row)
                        atomic_json(directory / 'report.json', report)
        if not report['checks'] or not all(r['passed'] for r in report['checks']):
            raise RuntimeError('Tenant ingress matrix failed or was empty')
        latest = c.obj('get', 'pods')['items']
        ready_uids = {p['metadata']['uid'] for p in latest if not p['metadata'].get('deletionTimestamp')
                      and any(x['type'] == 'Ready' and x['status'] == 'True' for x in p.get('status', {}).get('conditions', []))}
        if any(p['metadata']['uid'] not in ready_uids for p in main + tenants):
            raise RuntimeError('Observed Pod changed or became NotReady during verification')
        for category, condition in [('cross-node', any(r['source_node'] != r['target_node'] for r in report['checks'])),
                                    ('same-node', any(r['source_node'] == r['target_node'] for r in report['checks'])),
                                    ('IPv6', any(r['family'] == 6 for r in report['checks'])),
                                    ('NodePort', any(r['route'] == 'nodeport' for r in report['checks']))]:
            if not condition:
                report['not_covered'].append(category)
        if {p['metadata']['labels'][MANAGER] for p in tenants} != TENANTS:
            report['not_covered'].append('both runtime tenant families')
        receipt['status'] = 'observed-matrix-passed'
    except BaseException as error:
        receipt['status'] = 'failed'
        report['errors'].append(str(error))
        raise
    finally:
        atomic_json(directory / 'report.json', report)
        print('Tenant network report:', directory, flush=True)
