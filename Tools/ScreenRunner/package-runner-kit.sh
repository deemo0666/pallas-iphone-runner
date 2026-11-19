#!/bin/bash
set -euo pipefail
scripts=$(cd "$(dirname "$0")" && pwd)
runner=${1:?Usage: package-runner-kit.sh ScreenRunner.ipa output.zip}
output=${2:?Provide an output ZIP path}
archive="$(cd "$(dirname "$output")" && pwd)/$(basename "$output")"
work=$(mktemp -d)
build_target=${CARGO_TARGET_DIR:-${TMPDIR:-/tmp}/dsh-local-installer-target}
trap 'rm -rf "$work"' EXIT

source "$scripts/mac-installer-steps.sh"
stage="$work/Pallas-Runner"
mkdir -p "$stage/Kit"
cp "$scripts/../../LICENSE.md" "$stage/"
cp "$runner" "$stage/Kit/ScreenRunner.ipa"
cp "$scripts/RunnerHost.json" "$stage/Kit/"
/usr/bin/python3 - "$stage" <<'PY'
import json
from pathlib import Path
import plistlib
import sys
from zipfile import ZipFile

stage = Path(sys.argv[1])
kit = stage / 'Kit'
host = json.loads((kit / 'RunnerHost.json').read_text())
with ZipFile(kit / 'ScreenRunner.ipa') as ipa:
    files = ipa.namelist()
    info_path, = (name for name in files if name.startswith('Payload/')
                  and name.endswith('.app/Info.plist') and name.count('/') == 2)
    info = plistlib.loads(ipa.read(info_path))
    if info['CFBundleIdentifier'] != host['bundle_id'] + '.screen.xctrunner':
        sys.exit('ScreenRunner.ipa does not match RunnerHost.json.')
    if not any(name.endswith('/DSHScreen.xctest/DSHScreen') for name in files):
        sys.exit('ScreenRunner.ipa is missing the DSHScreen executable.')
(stage / 'metadata.json').write_text(json.dumps({
    'version': info['CFBundleShortVersionString'],
    'build': info['CFBundleVersion'],
}) + '\n')
PY
(cd "$stage/Kit" && /usr/bin/shasum -a 256 ScreenRunner.ipa RunnerHost.json > SHA256SUMS)

installer_service "$stage/dsh-installer-service"
installer_notices "$stage"
codesign --force --sign - "$stage/dsh-installer-service"
codesign --verify --strict "$stage/dsh-installer-service"
ditto -c -k --norsrc --noextattr --keepParent "$stage" "$work/runner-kit.zip"
unzip -tq "$work/runner-kit.zip"
mv "$work/runner-kit.zip" "$archive"
echo "$archive"
