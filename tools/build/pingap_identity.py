#!/usr/bin/env python3
"""Read Pingap's paired source identity; parse actual executable versions exactly."""
import json
from pathlib import Path
import re

_PRERELEASE_ID = r'(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)'
_SEMVER = (r'(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)'
           + r'(?:-' + _PRERELEASE_ID + r'(?:\.' + _PRERELEASE_ID + r')*)?'
           + r'(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?')


def parse_pingap_version(output):
    match = re.fullmatch(r'pingap (' + _SEMVER + r')', output.strip())
    if not match:
        raise ValueError('unexpected Pingap version output: ' + repr(output))
    return match.group(1)


def source_identity(root=None):
    root = Path(root) if root is not None else Path(__file__).resolve().parents[2]
    cargo = (root / 'crates/app-cli/Cargo.toml').read_text()
    declarations = re.findall(r'^\s*pingap-config\s*=\s*\{([^\n]*)\}\s*(?:#.*)?$', cargo, re.M)
    if len(declarations) != 1:
        raise ValueError('expected exactly one supported inline pingap-config dependency')
    def dependency_field(name):
        values = re.findall(r'(?:^|,)\s*' + name + r'\s*=\s*"([^"]+)"\s*(?=,|$)', declarations[0])
        if len(values) != 1:
            raise ValueError('expected exactly one pingap-config ' + name + ' field')
        return values[0]
    commit = dependency_field('rev')
    if dependency_field('git') != 'https://github.com/vicanso/pingap' or not re.fullmatch(r'[0-9a-f]{40}', commit):
        raise ValueError('app-cli pingap-config must pin the official repository and exact commit')
    path = root / 'crates/app-cli/src/build_deploy/devtool.rs'
    content = path.read_text()
    def constant(name):
        matches = re.findall(r'const\s+' + name + r'\s*:\s*&str\s*=\s*"([^"]+)"\s*;', content)
        if len(matches) != 1:
            raise ValueError('expected exactly one ' + name + ' source constant')
        return matches[0]
    version = constant('DEFAULT_PINGAP_VERSION')
    if not re.fullmatch(_SEMVER, version) or constant('DEFAULT_PINGAP_COMMIT') != commit:
        raise ValueError('app-cli Pingap constants disagree with its dependency pin')
    catalog_path = root / 'tools/build/pingap-assets.json'
    if catalog_path.exists():
        catalog = json.loads(catalog_path.read_text())
        release = catalog.get('releases', {}).get(version, {})
        if catalog.get('repository') != 'vicanso/pingap' or release.get('tag') != 'v' + version or release.get('commit') != commit:
            raise ValueError('app-cli Pingap identity disagrees with trusted official asset catalog')
    return {'version': version, 'commit': commit}


if __name__ == '__main__':
    print(json.dumps(source_identity(), sort_keys=True))
