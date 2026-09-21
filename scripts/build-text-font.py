#!/usr/bin/env python3
"""Cut the bundled static DM Sans text faces from the pinned upstream variable font.

Google publishes DM Sans only as a variable font, and the font-kit matcher behind
GPUI selects a loaded face by weight rather than instancing a `wght` axis: loading
the variable file directly would render every weight in the ladder at its 400
default. So the interface text faces are derived here, once, and committed.

The optical size is pinned to 12, not to the 14 of the upstream named instances,
because the text face only ever sets 11-13px roles (Body, Label, Caption); the
headings stay on IBM Plex Sans.

Run from the repository:  python3 scripts/build-text-font.py
Requires fontTools.  The upstream checksum is verified before anything is cut,
and `assets/fonts/manifest.json` is rewritten with the derived checksums.
"""
import hashlib
import io
import json
import pathlib
import urllib.request

from fontTools import ttLib
from fontTools.varLib import instancer

REVISION = 'db50662ac42c361aa77afeef89ae2e6e2298e2ab'
BASE = f'https://raw.githubusercontent.com/google/fonts/{REVISION}/ofl/dmsans'
SOURCE = f'{BASE}/DMSans%5Bopsz,wght%5D.ttf'
SOURCE_SHA256 = '8cd08d97e89c24d0aa92edd2f0f4c8ee6195eee9b7c9f154865a58b02f0c1c0d'
LICENSE = f'{BASE}/OFL.txt'
LICENSE_SHA256 = '9af36190332437f5ecd09974de43c1f7c77a310a996cdd8ceb25628b458840e1'

OPTICAL_SIZE = 12
# (weight, subfamily, macStyle bit) — the ladder uses Medium for running text and
# Bold for emphasis; Regular covers anything that renders at the stock weight.
CUTS = [(400, 'Regular', 0), (500, 'Medium', 0), (700, 'Bold', 1)]

root = pathlib.Path(__file__).resolve().parent.parent
fonts = root / 'assets' / 'fonts'


def fetch(url, expected):
    data = urllib.request.urlopen(url).read()
    actual = hashlib.sha256(data).hexdigest()
    if actual != expected:
        raise SystemExit(f'{url}\n  expected sha256 {expected}\n  got      sha256 {actual}')
    return data


def name(font, string, name_id):
    """Set a name record across the Mac and Windows platforms the record uses."""
    for record in font['name'].names:
        if record.nameID == name_id:
            font['name'].setName(string, name_id, record.platformID,
                                 record.platEncID, record.langID)


variable = fetch(SOURCE, SOURCE_SHA256)
(fonts / 'DM-Sans-LICENSE.txt').write_bytes(fetch(LICENSE, LICENSE_SHA256))

written = []
for weight, subfamily, bold_bit in CUTS:
    font = ttLib.TTFont(io.BytesIO(variable))
    # `updateFontNames` is off: it derives names from the STAT table, which only
    # declares axis values for the upstream optical sizes, so pinning opsz to 12
    # fails there. The names this font needs are written below instead.
    instancer.instantiateVariableFont(
        font, {'opsz': OPTICAL_SIZE, 'wght': weight}, inplace=True)

    # GPUI resolves a family by its typographic name, so every cut has to answer
    # to the one family the app asks for.
    full = 'DM Sans' if subfamily == 'Regular' else f'DM Sans {subfamily}'
    name(font, 'DM Sans', 16)
    name(font, subfamily, 17)
    # RIBBI: only Regular and Bold may share a legacy family; Medium takes its
    # own so a naive matcher cannot mistake it for the regular face.
    name(font, 'DM Sans' if subfamily in ('Regular', 'Bold') else full, 1)
    name(font, subfamily if subfamily in ('Regular', 'Bold') else 'Regular', 2)
    name(font, full, 4)
    name(font, f'DMSans-{subfamily}', 6)
    # A shared unique identifier across three files collides in the font cache.
    name(font, f'{full}; cut at opsz {OPTICAL_SIZE} for cibergit', 3)

    font['OS/2'].usWeightClass = weight
    font['OS/2'].fsSelection = (font['OS/2'].fsSelection & ~0b1100000) | (
        0b0100000 if bold_bit else 0b1000000)
    font['head'].macStyle = (font['head'].macStyle & ~0b11) | bold_bit

    path = fonts / f'DMSans-{subfamily}.ttf'
    font.save(path)
    written.append((path, weight))

manifest = json.loads((fonts / 'manifest.json').read_text())
manifest['derived'] = {
    'note': ('DM Sans is published upstream only as a variable font; the static text '
             'faces below are cut from it by scripts/build-text-font.py. Rerun that '
             'script to reproduce them.'),
    'repository': 'https://github.com/google/fonts',
    'revision': REVISION,
    'source': SOURCE,
    'source_sha256': SOURCE_SHA256,
    'instance': {'opsz': OPTICAL_SIZE},
    'files': [
        {'path': path.name, 'wght': weight, 'size': path.stat().st_size,
         'sha256': hashlib.sha256(path.read_bytes()).hexdigest()}
        for path, weight in written
    ] + [
        {'path': 'DM-Sans-LICENSE.txt', 'source': LICENSE,
         'size': (fonts / 'DM-Sans-LICENSE.txt').stat().st_size,
         'sha256': LICENSE_SHA256},
    ],
}
(fonts / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')

for path, weight in written:
    print(f'Cut {path.name} at wght {weight}, opsz {OPTICAL_SIZE} '
          f'({path.stat().st_size} bytes)')
