"""Optional registry-side base-layer reuse; no image tags are changed."""
import json


MOUNT = r'''
import json,pathlib,re,sys,urllib.error,urllib.parse,urllib.request
host,source,target,reference=json.load(sys.stdin)
origin='https://'+host
try:
 urllib.request.urlopen(origin+'/v2/',timeout=15)
 raise RuntimeError('Expected authenticated registry')
except urllib.error.HTTPError as e:
 if e.code!=401: raise
 fields=dict(re.findall(r'(\w+)="([^"]+)"',e.headers.get('WWW-Authenticate','')))
realm=fields.get('realm','')
if not realm.startswith('https://'): raise RuntimeError('Registry token endpoint must use HTTPS')
auth=json.loads((pathlib.Path.home()/'.docker/config.json').read_text()).get('auths',{}).get(host,{}).get('auth')
if not auth: raise RuntimeError('Registry requires inline Docker auth')
url=realm+'?'+urllib.parse.urlencode({'service':fields['service'],'scope':['repository:'+source+':pull','repository:'+target+':pull,push']},doseq=True)
token=json.load(urllib.request.urlopen(urllib.request.Request(url,headers={'Authorization':'Basic '+auth}),timeout=20))
headers={'Authorization':'Bearer '+token.get('token',token.get('access_token','')),
 'Accept':', '.join(['application/vnd.oci.image.manifest.v1+json','application/vnd.oci.image.index.v1+json','application/vnd.docker.distribution.manifest.v2+json','application/vnd.docker.distribution.manifest.list.v2+json'])}
def manifest(ref):
 return json.load(urllib.request.urlopen(urllib.request.Request(origin+'/v2/'+source+'/manifests/'+ref,headers=headers),timeout=20))
image=manifest(reference)
if 'manifests' in image:
 ref=next(x['digest'] for x in image['manifests'] if x.get('platform',{}).get('architecture')=='amd64' and x.get('platform',{}).get('os')=='linux')
 image=manifest(ref)
layers=[image['config'],*image['layers']]
mounted=0
for layer in layers:
 url=origin+'/v2/'+target+'/blobs/uploads/?'+urllib.parse.urlencode({'mount':layer['digest'],'from':source})
 with urllib.request.urlopen(urllib.request.Request(url,data=b'',headers=headers,method='POST'),timeout=30) as response:
  if response.status!=201: raise RuntimeError('Registry does not support cross-repository mounting; disable MOUNT_BASE_LAYERS')
 mounted+=1
print(json.dumps({'source':source,'target':target,'mounted':mounted,'bytes':sum(x['size'] for x in layers)}))
'''


def mount(c, base, destination):
    if c.get('REGISTRY_AUTH', 'none') != 'docker' or c.get('REGISTRY_HTTP', 'false') == 'true':
        raise ValueError('MOUNT_BASE_LAYERS requires HTTPS and REGISTRY_AUTH=docker')
    host, target = destination.split('/', 1)
    base_host, source = base.split('/', 1)
    if base_host != host:
        raise ValueError('MOUNT_BASE_LAYERS requires base and output in the same registry')
    source, reference = source.split('@', 1)
    source = source.split(':', 1)[0]
    result = json.loads(c.ssh(['python3', '-c', MOUNT], json.dumps([host, source, target, reference]), timeout=600))
    print('Reused registry base layers:', result['mounted'], flush=True)
    return result
