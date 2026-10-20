#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Select one ICD for an isolated native application run; MoltenVK is default."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--driver', choices=['moltenvk', 'kosmickrisp'], default='moltenvk')
    parser.add_argument('--icd', type=Path, help='Required for KosmicKrisp; optional MoltenVK override')
    parser.add_argument('--loader', type=Path, default=Path('/opt/homebrew/lib/libvulkan.1.dylib'))
    parser.add_argument('--probe', type=Path, default=Path(__file__).resolve().parents[2]/'target/vulkan_submit_probe')
    parser.add_argument('--output', type=Path, required=True, help='New directory for this run')
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    if not command:
        parser.error('supply -- command [args]')
    icd = args.icd
    if icd is None:
        if args.driver == 'kosmickrisp':
            parser.error('--driver kosmickrisp requires --icd (no automatic fallback)')
        icd = Path('/opt/homebrew/etc/vulkan/icd.d/MoltenVK_icd.json')
    icd, loader = icd.resolve(strict=True), args.loader.resolve(strict=True)
    data = json.loads(icd.read_text())
    library = Path(data['ICD']['library_path'])
    if not library.is_absolute():
        library = icd.parent / library
    # Bare names use dyld search paths; avoid guessing in a reproducibility tool.
    library = library.resolve(strict=True)
    probe = args.probe.resolve(strict=True)
    args.output.mkdir(parents=True, exist_ok=False)
    env = dict(os.environ)
    env.pop('VK_ICD_FILENAMES', None)
    env.update(VK_DRIVER_FILES=str(icd), XRIZER_VULKAN_LIBRARY=str(loader),
               KL_VULKAN_LIBRARY=str(loader))
    manifest = dict(requested_driver=args.driver, icd=str(icd), icd_contents=data,
                    library=str(library), library_sha256=hashlib.sha256(library.read_bytes()).hexdigest(), loader=str(loader), command=command,
                    selection={k: env[k] for k in ['VK_DRIVER_FILES', 'XRIZER_VULKAN_LIBRARY', 'KL_VULKAN_LIBRARY']},
                    started_unix=time.time())
    (args.output/'manifest.json').write_text(json.dumps(manifest, indent=2)+'\n')
    census = subprocess.run([str(probe), '--driver-only'], env=env, capture_output=True, text=True)
    (args.output/'driver.log').write_text(census.stdout+census.stderr)
    identity = 'driverID=14 ' if args.driver == 'moltenvk' else 'driverID=28 '
    devices = [line for line in census.stdout.splitlines() if line.startswith('device=')]
    if census.returncode or len(devices) != 1 or identity not in devices[0]:
        manifest.update(exit_code=1, error='ICD census failed or selected driver differs from request')
        (args.output/'manifest.json').write_text(json.dumps(manifest, indent=2)+'\n')
        print((args.output/'driver.log').read_text(), file=sys.stderr)
        return 1
    manifest['observed_device'] = devices[0]
    with (args.output/'run.log').open('w') as log:
        result = subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT)
    manifest.update(exit_code=result.returncode, elapsed_seconds=time.time()-manifest['started_unix'])
    (args.output/'manifest.json').write_text(json.dumps(manifest, indent=2)+'\n')
    print((args.output/'run.log').read_text(encoding='utf-8', errors='replace'), end='')
    return result.returncode


if __name__ == '__main__':
    sys.exit(main())
