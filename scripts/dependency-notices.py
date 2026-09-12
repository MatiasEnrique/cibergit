#!/usr/bin/env python3
"""Retain license metadata and license/notice texts from the locked target graph."""
import json
import pathlib
import subprocess

root = pathlib.Path(__file__).resolve().parent.parent
metadata = json.loads(subprocess.check_output([
    'cargo', 'metadata', '--locked', '--format-version', '1',
    '--filter-platform', 'aarch64-apple-darwin',
], cwd=root))
packages = sorted((p for p in metadata['packages'] if p['name'] != 'cibergit'),
                  key=lambda p: (p['name'], p['version']))
lines = ['# Third-party notices', '',
         'Generated from Cargo.lock for aarch64-apple-darwin. Includes build dependencies.',
         'Original cibergit code is MIT; dependencies retain their own terms.',
         'The GPUI framework and GPUI Kit base are Apache-2.0. Zed’s editor is not used.', '',
         '| Package | Version | Declared license |', '| --- | --- | --- |']
texts = []
for package in packages:
    license_id = package.get('license') or 'See package license file'
    lines.append(f"| {package['name']} | {package['version']} | {license_id} |")
    directory = pathlib.Path(package['manifest_path']).parent
    for path in sorted(directory.iterdir()):
        if path.is_file() and path.name.upper().startswith(('LICENSE', 'LICENCE', 'COPYING', 'NOTICE', 'UNLICENSE')):
            content = path.read_text(errors='replace')
            texts.extend([f"\n## {package['name']} {package['version']} — {path.name}\n", '```text', content, '```'])
(root / 'THIRD_PARTY_NOTICES.md').write_text('\n'.join(lines + texts) + '\n')
print(f'Retained metadata for {len(packages)} packages and {len(texts)//4} license/notice texts')
