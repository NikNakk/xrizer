#!/usr/bin/env python3
"""Build the release-1 contract from staged prefixes, with no machine-local paths."""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile

REPOSITORIES = {
    'monado': 'NikNakk/monado', 'kosmickrisp': 'NikNakk/mesa',
    'xrizer': 'NikNakk/xrizer', 'klepton': 'NikNakk/Klepton',
    'dxmt': 'NikNakk/dxmt', 'wine': 'NikNakk/wine-crossover',
    'macos-wine-xr': 'NikNakk/macos-wine-xr',
}
TOKEN = re.compile(r'[A-Za-z0-9][A-Za-z0-9._-]{0,159}\Z')
PLATFORMS = {'macos-arm64', 'macos-x86_64', 'windows-x86_64'}


def run(*args):
    return subprocess.check_output(args, text=True).strip()


def digest(path):
    value = hashlib.sha256()
    with open(path, 'rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


def validate_tree(root):
    root = root.resolve()
    for path in root.rglob('*'):
        relative = path.relative_to(root)
        if any(ord(c) < 32 or c in '\\' for c in str(relative)):
            raise ValueError(f'Unsafe filename: {relative}')
        if path.is_symlink():
            target = os.readlink(path)
            if os.path.isabs(target) or not path.resolve().is_relative_to(root):
                raise ValueError(f'Non-relocatable symlink: {relative} -> {target}')
        elif not path.is_file() and not path.is_dir():
            raise ValueError(f'Unsupported archive entry: {relative}')


def macho(path):
    if not path.is_file() or path.is_symlink():
        return False
    with path.open('rb') as stream:
        return stream.read(4) in (b'\xcf\xfa\xed\xfe', b'\xce\xfa\xed\xfe',
                                 b'\xca\xfe\xba\xbe', b'\xbe\xba\xfe\xca')


def relocate(root):
    """Copy linked non-system dylibs transitively and use loader-relative references."""
    root = root.resolve()
    queue = [p for p in root.rglob('*') if macho(p)]
    processed = set()
    originals = {}
    for path in queue:
        originals.setdefault(path.name, []).append((path, digest(path)))
    while queue:
        binary = queue.pop()
        if binary in processed:
            continue
        processed.add(binary)
        arch = os.environ.get('PLATFORM', '').removeprefix('macos-')
        if arch in ('arm64', 'x86_64'):
            run('lipo', str(binary), '-verify_arch', arch)
        binary.chmod(binary.stat().st_mode | 0o200)
        load_commands = run('otool', '-l', str(binary)).splitlines()
        rpaths = [load_commands[i + 2].strip().split('path ', 1)[1].rsplit(' (offset', 1)[0]
                  for i, line in enumerate(load_commands) if line.strip() == 'cmd LC_RPATH']
        dependencies = [line.strip().split(' (compatibility', 1)[0]
                        for line in run('otool', '-L', str(binary)).splitlines()[1:]]
        ids = run('otool', '-D', str(binary)).splitlines()[1:]
        for dependency in dependencies:
            if dependency in ids or dependency.startswith(('/usr/lib/', '/System/Library/')):
                continue
            expanded = dependency.replace('@loader_path', str(binary.parent))
            candidates = [Path(expanded)]
            if dependency.startswith('@rpath/'):
                suffix = dependency[len('@rpath/'):]
                candidates = [Path(r.replace('@loader_path', str(binary.parent))) / suffix for r in rpaths]
                candidates += list(root.rglob(Path(suffix).name))
            source = next((p.resolve() for p in candidates if p.is_file()), None)
            if source is None:
                raise ValueError(f'Unresolved dependency {dependency} in {binary}')
            if source.is_relative_to(root):
                target = source
            else:
                if '.framework/' in str(source):
                    raise ValueError(f'External framework needs explicit packaging: {source}')
                matches = [p for p, sha in originals.get(source.name, []) if sha == digest(source)]
                target = matches[0] if len(matches) == 1 else root / 'lib' / source.name
                target.parent.mkdir(parents=True, exist_ok=True)
                if not matches and target.exists() and digest(target) != digest(source):
                    raise ValueError(f'Dylib name collision: {source}')
                if not target.exists():
                    shutil.copy2(source, target)
                    queue.append(target)
            relative = '@loader_path/' + os.path.relpath(target, binary.parent)
            run('install_name_tool', '-change', dependency, relative, str(binary))
        if ids:
            run('install_name_tool', '-id', '@rpath/' + binary.name, str(binary))
        for rpath in set(rpaths):
            if rpath.startswith('/'):
                run('install_name_tool', '-delete_rpath', rpath, str(binary))
    # Patching invalidates linker/ad-hoc signatures; sign only after all mutations.
    for binary in sorted(processed):
        run('codesign', '--force', '--sign', '-', str(binary))
        run('codesign', '--verify', str(binary))


def relative_manifests(root):
    for path in root.rglob('*.json'):
        try:
            value = json.loads(path.read_text())
        except (ValueError, UnicodeError):
            continue
        changed = False
        def visit(node):
            nonlocal changed
            if isinstance(node, dict):
                for key, val in node.items():
                    if key in ('library_path', 'MND_libmonado_path') and isinstance(val, str):
                        if os.path.isabs(val):
                            matches = [p for p in root.rglob(Path(val).name) if p.is_file()]
                            if len(matches) != 1:
                                raise ValueError(f'Cannot relocate {path}: {val}')
                            node[key] = os.path.relpath(matches[0], path.parent)
                            changed = True
                        resolved = (path.parent / node[key]).resolve()
                        if not resolved.is_relative_to(root.resolve()) or not resolved.is_file():
                            raise ValueError(f'Broken or external library path in {path}: {node[key]}')
                    else:
                        visit(val)
            elif isinstance(node, list):
                for child in node:
                    visit(child)
        visit(value)
        if changed:
            path.write_text(json.dumps(value, indent=2) + '\n')


def public_distribution_check(root):
    policy_path = Path(__file__).with_name('distribution-policy.json')
    if not policy_path.is_file():
        raise ValueError('Missing public distribution policy')
    policy = json.loads(policy_path.read_text())
    denied = {name.lower() for name in policy['excluded_names']}
    for path in Path(root).rglob('*'):
        parts = [part.lower() for part in path.relative_to(root).parts]
        if path.name.lower() in denied or any(any(part.startswith(name.lower()) for name in policy['excluded_directories']) for part in parts):
            raise ValueError(f'Excluded from public release pending distribution review: {path.relative_to(root)}')


def package(component, version, platform, role, prefix, output):
    if component not in REPOSITORIES or not TOKEN.fullmatch(version) or platform not in PLATFORMS:
        raise ValueError('Unsupported component, version or platform')
    prefix, output = Path(prefix).resolve(), Path(output).resolve()
    if not prefix.is_dir() or output.is_relative_to(prefix):
        raise ValueError('Prefix must exist and output must be outside it')
    public_distribution_check(prefix)
    validate_tree(prefix)
    if not any(p.is_file() and not p.is_symlink() for p in prefix.rglob('*')):
        raise ValueError('Empty artifact')
    if platform.startswith('macos-'):
        relocate(prefix)
    relative_manifests(prefix)
    public_distribution_check(prefix)
    validate_tree(prefix)
    output.mkdir(parents=True, exist_ok=True)
    name = f'{component}-{version}-{platform}.tar.xz'
    def normalize(info):
        if not (info.isdir() or info.isfile() or info.issym()):
            raise ValueError(f'Unsupported tar entry: {info.name}')
        info.uid = info.gid = 0
        info.uname = info.gname = ''
        info.mode = 0o755 if info.isdir() or info.mode & 0o111 else 0o644
        return info
    with tarfile.open(output / name, 'w:xz', dereference=False) as archive:
        archive.add(prefix, arcname=component, filter=normalize)
    artifact = dict(name=name, platform=platform, role=role, sha256=digest(output / name), size=(output / name).stat().st_size)
    (output / f'{platform}.json').write_text(json.dumps(artifact, indent=2) + '\n')
    return artifact


def manifest(component, version, commit, workflow_run, artifacts, output, build_tag=None):
    if component not in REPOSITORIES or not TOKEN.fullmatch(version) or not re.fullmatch('[0-9a-f]{40}', commit):
        raise ValueError('Invalid release identity')
    if not workflow_run.startswith(f'https://github.com/{REPOSITORIES[component]}/actions/runs/'):
        raise ValueError('Wrong workflow repository')
    entries = [json.loads(Path(p).read_text()) for p in artifacts]
    if len({e['platform'] for e in entries}) != len(entries) or not entries:
        raise ValueError('Missing or duplicate platforms')
    required = {
        'monado': {'macos-arm64', 'macos-x86_64'}, 'kosmickrisp': {'macos-arm64'},
        'xrizer': {'macos-arm64', 'windows-x86_64'}, 'klepton': {'macos-arm64'},
        'dxmt': {'macos-x86_64', 'windows-x86_64'}, 'wine': {'macos-x86_64'},
        'macos-wine-xr': {'macos-arm64', 'windows-x86_64'},
    }[component]
    if {e['platform'] for e in entries} != required or (component == 'monado' and not build_tag):
        raise ValueError('Incomplete component release')
    for entry in entries:
        path = Path(output) / entry['name']
        if entry['name'] != f"{component}-{version}-{entry['platform']}.tar.xz" or digest(path) != entry['sha256'] or path.stat().st_size != entry['size']:
            raise ValueError('Artifact metadata does not match archive')
    value = dict(schema='threshold-xr/release-1', component=component, version=version,
                 repository=REPOSITORIES[component], commit=commit,
                 built=datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ'),
                 workflow_run=workflow_run, provides={'build_tag': build_tag} if build_tag else {},
                 artifacts=sorted(entries, key=lambda e: e['platform']))
    path = Path(output) / 'threshold-release.json'
    path.write_text(json.dumps(value, indent=2) + '\n')
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    pack = sub.add_parser('package')
    for flag in ('component', 'version', 'platform', 'role', 'prefix', 'output'):
        pack.add_argument('--' + flag, required=True)
    create = sub.add_parser('manifest')
    for flag in ('component', 'version', 'commit', 'workflow-run', 'output'):
        create.add_argument('--' + flag, required=True)
    create.add_argument('--build-tag')
    create.add_argument('artifacts', nargs='+')
    args = vars(parser.parse_args())
    command = args.pop('command')
    (package if command == 'package' else manifest)(**args)


if __name__ == '__main__':
    main()
