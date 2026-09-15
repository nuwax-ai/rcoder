"""Self-contained Kubernetes resources. JSON is valid kubectl input and config YAML."""
import json
from urllib.parse import quote
from common import LABEL, digest


def render(c, images, password, registry_auth=None):
    import secrets as _secrets
    preview_token = _secrets.token_hex(24)
    labels = {LABEL: c.id}
    out = []

    def obj(kind, name, spec=None, api='v1', cluster=False, **fields):
        meta = {'name': name, 'labels': dict(labels)}
        if not cluster:
            meta['namespace'] = c.ns
        row = {'apiVersion': api, 'kind': kind, 'metadata': meta, **fields}
        if spec is not None:
            row['spec'] = spec
        out.append(row)
        return row

    obj('Namespace', c.ns, cluster=True)
    for name in ['rcoder', 'rcoder-pods-sa'] + (['default'] if registry_auth else []):
        obj('ServiceAccount', name, **({'imagePullSecrets': [{'name': 'registry'}]} if registry_auth else {}))
    if registry_auth:
        obj('Secret', 'registry', type='kubernetes.io/dockerconfigjson', stringData={'.dockerconfigjson': json.dumps(registry_auth)})
    rules = [
        {'apiGroups': [''], 'resources': ['pods', 'pods/log', 'pods/exec', 'services', 'endpoints', 'persistentvolumeclaims', 'events', 'configmaps', 'secrets'], 'verbs': ['get', 'list', 'watch', 'create', 'update', 'patch', 'delete', 'deletecollection']},
        {'apiGroups': ['apps'], 'resources': ['deployments', 'replicasets', 'statefulsets'], 'verbs': ['get', 'list', 'watch', 'create', 'update', 'patch', 'delete']},
        {'apiGroups': ['gateway.networking.k8s.io'], 'resources': ['httproutes'], 'verbs': ['get', 'list', 'watch', 'create', 'update', 'patch', 'delete', 'deletecollection']},
    ]
    obj('Role', 'rcoder', api='rbac.authorization.k8s.io/v1', rules=rules)
    obj('RoleBinding', 'rcoder', api='rbac.authorization.k8s.io/v1',
        roleRef={'apiGroup': 'rbac.authorization.k8s.io', 'kind': 'Role', 'name': 'rcoder'},
        subjects=[{'kind': 'ServiceAccount', 'name': 'rcoder', 'namespace': c.ns}])
    obj('ClusterRole', c.ns + '-pv-reader', api='rbac.authorization.k8s.io/v1', cluster=True,
        rules=[{'apiGroups': [''], 'resources': ['persistentvolumes', 'namespaces', 'nodes'], 'verbs': ['get', 'list', 'watch']}])
    obj('ClusterRoleBinding', c.ns + '-pv-reader', api='rbac.authorization.k8s.io/v1', cluster=True,
        roleRef={'apiGroup': 'rbac.authorization.k8s.io', 'kind': 'ClusterRole', 'name': c.ns + '-pv-reader'},
        subjects=[{'kind': 'ServiceAccount', 'name': 'rcoder', 'namespace': c.ns}])

    def pvc(name, sc, access='ReadWriteMany', size='10Gi', **extra):
        obj('PersistentVolumeClaim', name, {'storageClassName': sc, 'accessModes': [access],
             'resources': {'requests': {'storage': size}}, **extra})

    for name in ['workspace', 'computer-workspace']:
        pvc(name, c.get('STORAGE_CLASS', 'cephfs'))
    pvc('postgres-data', c.get('PG_STORAGE_CLASS', 'ceph-rbd'), 'ReadWriteOnce')
    root_name = c.ns + '-cephfs-root'
    obj('PersistentVolume', root_name, {
        'capacity': {'storage': '1Mi'}, 'accessModes': ['ReadWriteMany'],
        'persistentVolumeReclaimPolicy': 'Retain', 'storageClassName': '',
        'claimRef': {'name': 'cephfs-root', 'namespace': c.ns},
        'csi': {'driver': c.get('CEPH_DRIVER', 'rook-ceph.cephfs.csi.ceph.com'),
                'volumeHandle': root_name, 'volumeAttributes': {'clusterID': c.get('CEPH_CLUSTER', 'rook-ceph'),
                    'fsName': c.get('CEPH_FS', 'myfs'), 'staticVolume': 'true', 'rootPath': '/'},
                'nodeStageSecretRef': {'name': c.get('CEPH_SECRET', 'rook-csi-cephfs-node'),
                                       'namespace': c.get('CEPH_SECRET_NS', 'rook-ceph')}}}, cluster=True)
    pvc('cephfs-root', '', size='1Mi', volumeName=root_name)
    obj('Secret', 'postgres', type='Opaque', stringData={'password': password,
        'url': 'postgresql://rcoder:' + quote(password, safe='') + '@postgres:5432/rcoder'})
    obj('Service', 'postgres', {'selector': {'app': 'postgres', **labels}, 'ports': [{'port': 5432}]})
    obj('StatefulSet', 'postgres', {'serviceName': 'postgres', 'replicas': 1,
        'selector': {'matchLabels': {'app': 'postgres', **labels}},
        'template': {'metadata': {'labels': {'app': 'postgres', **labels}}, 'spec': {
            'containers': [{'name': 'postgres', 'image': c.get('POSTGRES_IMAGE', 'postgres:16-alpine'),
                'env': [{'name': 'POSTGRES_USER', 'value': 'rcoder'}, {'name': 'POSTGRES_DB', 'value': 'rcoder'},
                        {'name': 'PGDATA', 'value': '/var/lib/postgresql/data/pgdata'},
                        {'name': 'POSTGRES_PASSWORD', 'valueFrom': {'secretKeyRef': {'name': 'postgres', 'key': 'password'}}}],
                'readinessProbe': {'exec': {'command': ['pg_isready', '-U', 'rcoder']}, 'periodSeconds': 5},
                'resources': {'requests': {'cpu': '100m', 'memory': '256Mi'}, 'limits': {'cpu': '1', 'memory': '1Gi'}},
                'volumeMounts': [{'name': 'data', 'mountPath': '/var/lib/postgresql/data'}]}],
            'volumes': [{'name': 'data', 'persistentVolumeClaim': {'claimName': 'postgres-data'}}]}}}, api='apps/v1')

    services = {}
    for service, image, path in [('web-agent-runner', images['rcoder'], '/app/project_workspace'),
                                  ('computer-agent-runner', images['computer'], '/app/computer-project-workspace')]:
        value = {'service_type': service, 'image': image, 'enabled': True, 'workspace_resolution_path': path,
            'environment': {'SERVICE_MODE': 'full' if service.startswith('web') else 'agent-only',
                            'PROJECT_WORKSPACE_BASE': '/app/project_workspace' if service.startswith('web') else '/home/user',
                            'API_PORT': '8086', 'AGENT_PORT': '8086', 'RUST_LOG': 'info'},
            'resource_limits': {'memory_limit': 2147483648, 'cpu_limit': 2.0, 'storage_size': '10Gi'}}
        if service.startswith('web'):
            value['command'] = ['/bin/bash', '/app/agent-runner-start.sh', '/app/bin/agent_runner', '--port', '8086']
        services[service] = value
    services['user-app-builder'] = {
        **services['computer-agent-runner'], 'service_type': 'user-app-builder',
        'environment': {**services['computer-agent-runner']['environment'], 'SERVICE_MODE': 'full'},
    }
    services['user-app'] = {'service_type': 'user-app', 'image': images['runtime'], 'enabled': True}
    config = {'default_agent': 'Claude', 'port': 8086, 'projects_dir': '/app/project_workspace',
              'kubernetes_config': {'global_defaults': {}, 'services': services},
              # Startup prefix resolution still reads this fallback in Kubernetes mode.
              'docker_config': {'multi_image_config': {'global_defaults': {}, 'services': services,
                  'selection_strategy': 'ServiceOnly', 'cache_config': {'enabled': True, 'ttl_seconds': 3600, 'max_entries': 50}}},
              'api_key_auth': {'enabled': False, 'api_key': ''},
              'proxy_config': {'listen_port': 8088, 'default_backend_port': 8086, 'backend_host': '127.0.0.1', 'port_param': 'port', 'health_check': {'enabled': True, 'interval_seconds': 5, 'timeout_seconds': 1, 'healthy_threshold': 2, 'unhealthy_threshold': 3}},
              # Custom Page 多副本预览协调器（K8s=平台 PG；令牌经 env 注入不入 configmap）
              'preview_coordinator': {'enabled': True, 'internal_token_env': 'RCODER_PREVIEW_INTERNAL_TOKEN', 'peer_api_port': 8086},
              # 60000 分流代理（dev 生命周期 7 端点全策略导向 Rust 上游）
              'file_server_proxy': {'listen_port': 60000, 'rust_upstream_port': 8086, 'ts_upstream_port': 60001, 'policy': 'ts_first', 'coordinated_dev_lifecycle': True}}
    config_name = 'rcoder-config-' + digest(config)[:12]
    obj('ConfigMap', config_name, data={'config.yml': json.dumps(config)}, immutable=True)
    env = {'CONTAINER_RUNTIME': 'kubernetes', 'RCODER_PORT': '8086', 'RCODER_K8S_NAMESPACE': c.ns,
           'RCODER_K8S_STORAGE_CLASS': c.get('STORAGE_CLASS', 'cephfs'), 'RCODER_K8S_PVC_ACCESS_MODE': 'ReadWriteMany',
           'RCODER_USERAPP_STORAGE_CLASS': c.get('USERAPP_STORAGE_CLASS', 'ceph-rbd'),
           'RCODER_API_KEY_ENABLED': 'false', 'RCODER_STORAGE_BACKEND': 'postgres', 'RCODER_USERAPP_STORAGE_BACKEND': 'postgres',
           'RCODER_PG_HOST': 'postgres', 'RCODER_PG_USERNAME': 'rcoder', 'RCODER_PG_DATABASE': 'rcoder',
           'RCODER_WORKSPACE_PVC_NAME': 'workspace', 'RCODER_WORKSPACE_SUBPATH': 'workspace',
           'RCODER_COMPUTER_WORKSPACE_PVC_NAME': 'computer-workspace',
           'RCODER_WORKSPACE_ROOT': '/app/project_workspace/apps', 'RCODER_CEPHFS_ROOT': '/app/cephfs-root',
           'RCODER_K8S_GATEWAY_NAME': 'rcoder', 'RCODER_K8S_GATEWAY_NAMESPACE': c.ns,
           'RCODER_RUNTIME_IMAGE_DIGEST': images['runtime'],
           'RCODER_PINGAP_VERSION': c.get('PINGAP_VERSION', '0.14.1'),
           'RCODER_PINGAP_COMMIT': c.get('PINGAP_COMMIT', 'c74e4eaa44e64958cffa18c33e8bbf5995b6844f'), 'ENABLE_TTYD': 'false', 'RUST_LOG': 'info'}
    if registry_auth:
        env['RCODER_K8S_IMAGE_PULL_SECRET'] = 'registry'
    mounts = [{'name': 'config', 'mountPath': '/app/config.yml', 'subPath': 'config.yml', 'readOnly': True},
              {'name': 'workspace', 'mountPath': '/app/project_workspace', 'subPath': 'workspace'},
              {'name': 'computer-workspace', 'mountPath': '/app/computer-project-workspace'},
              {'name': 'cephfs-root', 'mountPath': '/app/cephfs-root'}, {'name': 'logs', 'mountPath': '/app/logs'}]
    obj('Deployment', 'rcoder', {'replicas': 2, 'selector': {'matchLabels': {'app': 'rcoder', **labels}},
        'template': {'metadata': {'labels': {'app': 'rcoder', **labels}}, 'spec': {
            'serviceAccountName': 'rcoder', 'nodeSelector': {'kubernetes.io/arch': 'amd64'},
            'initContainers': [{'name': 'workspace-init', 'image': c.get('POSTGRES_IMAGE', 'postgres:16-alpine'),
                'command': ['sh', '-ec', 'mkdir -p /workspace/workspace/apps'],
                'volumeMounts': [{'name': 'workspace', 'mountPath': '/workspace'}]}],
            'containers': [{'name': 'rcoder', 'image': images['rcoder'],
                'command': ['/app/bin/rcoder'], 'args': ['--port', '8086'], 'workingDir': '/app',
                'env': [{'name': k, 'value': v} for k, v in env.items()] + [
                    {'name': 'RCODER_PG_PASSWORD', 'valueFrom': {'secretKeyRef': {'name': 'postgres', 'key': 'password'}}},
                    {'name': 'RCODER_PG_URL', 'valueFrom': {'secretKeyRef': {'name': 'postgres', 'key': 'url'}}},
                    {'name': 'RCODER_USERAPP_PG_URL', 'valueFrom': {'secretKeyRef': {'name': 'postgres', 'key': 'url'}}},
                    # 预览协调内部令牌（每次部署随机生成；跨 Pod 派发/转发入口鉴权）
                    {'name': 'RCODER_PREVIEW_INTERNAL_TOKEN', 'value': preview_token},
                    # 宿主身份（Downward API）：POD_UID=Unknown 恢复证据锚点，POD_IP=跨 Pod 转发寻址
                    {'name': 'POD_NAME', 'valueFrom': {'fieldRef': {'fieldPath': 'metadata.name'}}},
                    {'name': 'POD_UID', 'valueFrom': {'fieldRef': {'fieldPath': 'metadata.uid'}}},
                    {'name': 'POD_IP', 'valueFrom': {'fieldRef': {'fieldPath': 'status.podIP'}}}],
                'ports': [{'name': 'http', 'containerPort': 8086}, {'name': 'proxy', 'containerPort': 8088}],
                'startupProbe': {'httpGet': {'path': '/health', 'port': 'http'}, 'periodSeconds': 5, 'failureThreshold': 60},
                'readinessProbe': {'httpGet': {'path': '/health', 'port': 'http'}, 'periodSeconds': 5},
                'resources': {'requests': {'cpu': '250m', 'memory': '512Mi'}, 'limits': {'cpu': '2', 'memory': '2Gi'}},
                'volumeMounts': mounts}],
            'volumes': [{'name': 'config', 'configMap': {'name': config_name}}, {'name': 'logs', 'emptyDir': {}}] +
                       [{'name': n, 'persistentVolumeClaim': {'claimName': n}} for n in ['workspace', 'computer-workspace', 'cephfs-root']]}}}, api='apps/v1')
    obj('Service', 'rcoder', {'type': 'NodePort', 'selector': {'app': 'rcoder', **labels},
         'ports': [{'name': 'http', 'port': 8086, 'targetPort': 'http', 'nodePort': c.nodeport}]})
    obj('CiliumGatewayClassConfig', 'rcoder', {'service': {'type': 'NodePort'}}, api='cilium.io/v2alpha1')
    obj('GatewayClass', c.ns, {'controllerName': 'io.cilium/gateway-controller',
         'parametersRef': {'group': 'cilium.io', 'kind': 'CiliumGatewayClassConfig', 'name': 'rcoder', 'namespace': c.ns}},
         api='gateway.networking.k8s.io/v1', cluster=True)
    obj('Gateway', 'rcoder', {'gatewayClassName': c.ns, 'listeners': [
        {'name': 'http', 'port': 80, 'protocol': 'HTTP', 'allowedRoutes': {'namespaces': {'from': 'Same'}}}]}, api='gateway.networking.k8s.io/v1')
    obj('HTTPRoute', 'rcoder', {'parentRefs': [{'name': 'rcoder'}], 'rules': [
        {'matches': [{'path': {'type': 'PathPrefix', 'value': '/'}}], 'backendRefs': [{'name': 'rcoder', 'port': 8086}]}]}, api='gateway.networking.k8s.io/v1')
    return out
