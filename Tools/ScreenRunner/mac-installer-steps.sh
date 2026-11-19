# Shared build steps for the macOS installer pieces. Sourced, not executed, by
# package-mac-installer.sh (standalone app) and stage-mac-phone-kit.sh (kit for PallasMac).
# Callers set: scripts (this directory), work (scratch dir), build_target (CARGO_TARGET_DIR).

# installer_kit [--current-build] products destination-Kit
# Two IPAs + SHA256SUMS from package-sideload.sh, nothing else.
installer_kit() {
    local options=()
    if [[ "$1" == --current-build ]]; then options+=(--current-build); shift; fi
    local products=$1 kit=$2
    "$scripts/package-sideload.sh" ${options[@]+"${options[@]}"} "$products" "$work/kit.zip"
    ditto -x -k "$work/kit.zip" "$work/unpacked"
    mv "$work/unpacked/"* "$kit"
    find "$kit" -mindepth 1 -maxdepth 1 ! -name '*.ipa' ! -name SHA256SUMS -exec rm -rf {} +
}

# installer_package_plist Kit output.plist — name/version/build of the host IPA.
installer_package_plist() {
    /usr/bin/python3 - "$1" "$2" <<'PY'
from pathlib import Path
import plistlib
import sys
from zipfile import ZipFile
kit, output = Path(sys.argv[1]), Path(sys.argv[2])
host, = [p for p in kit.glob("*.ipa") if p.name != "ScreenRunner.ipa"]
with ZipFile(host) as ipa:
    info_path, = [p for p in ipa.namelist() if p.startswith("Payload/") and p.endswith(".app/Info.plist") and p.count("/") == 2]
    info = plistlib.loads(ipa.read(info_path))
output.write_bytes(plistlib.dumps({
    "name": info.get("CFBundleDisplayName") or info["CFBundleName"],
    "version": info["CFBundleShortVersionString"], "build": info["CFBundleVersion"],
}))
PY
}

# installer_service output — universal dsh-installer-service.
installer_service() {
    # A fresh install alone cannot catch saved-key format regressions.
    CARGO_TARGET_DIR="$build_target" cargo test --locked \
        --manifest-path "$scripts/NativeInstaller/Cargo.toml" \
        saved_key_round_trip_survives_repeat_installs_without_replacing_storage

    local target
    for target in aarch64-apple-darwin x86_64-apple-darwin; do
        CARGO_TARGET_DIR="$build_target" MACOSX_DEPLOYMENT_TARGET=13.0 cargo build --release --locked \
            --manifest-path "$scripts/NativeInstaller/Cargo.toml" --target "$target"
    done
    xcrun lipo -create \
        "$build_target/aarch64-apple-darwin/release/dsh-installer-service" \
        "$build_target/x86_64-apple-darwin/release/dsh-installer-service" \
        -output "$1"
}

# installer_resources directory — compiled strings, third-party notices, dependency lock.
installer_resources() {
    xcrun xcstringstool compile "$scripts/MacInstaller/Localizable.xcstrings" --output-directory "$1"
    installer_notices "$1"
}

installer_notices() {
    python3 "$scripts/NativeInstaller/notices.py" "$1/ThirdPartyNotices.txt"
    cp "$scripts/NativeInstaller/Cargo.lock" "$1/DependencyVersions.lock"
}
