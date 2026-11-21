"""Bundle dependency license texts from Cargo's locked source cache."""
import json
from pathlib import Path
import subprocess
import sys
import zipfile

root = Path(__file__).resolve().parent
metadata = json.loads(subprocess.check_output([
    "cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", "aarch64-apple-darwin", "--manifest-path", str(root / "Cargo.toml")
]))
parts = ["Pallas Agent Installer — third-party notices\n\n"
         "Apple authentication and signing: isideload (MIT), nab138.\n"
         "USB device communication: idevice (MIT), jkcoxson and contributors.\n"
         "Bundle signing: apple-codesign-quick; see its license below.\n"
         "Authentication device-data service: https://ani.iloader.app\n"
         "Developer image source: https://github.com/doronz88/DeveloperDiskImage\n"
         "Image snapshot: e2aaeac89ca8fa56c2fa1f144d182994e07a525b (Apple-signed DDI 27A5228h).\n"
         "Neither this app nor its branding is affiliated with Apple or iloader.\n"]
used = {node["id"] for node in metadata["resolve"]["nodes"]}
for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"])):
    if package["id"] not in used or package["name"] == "dsh-installer-service":
        continue
    directory = Path(package["manifest_path"]).parent
    parts.append(f"\n{'=' * 72}\n{package['name']} {package['version']}\n"
                 f"License: {package.get('license') or ''}\n{package.get('repository') or ''}\n"
                 f"Authors: {', '.join(package.get('authors', []))}\n")
    files = set()
    for pattern in ("LICENSE*", "LICENCE*", "COPYING*", "NOTICE*", "license*", "licenses/*"):
        files.update(p for p in directory.glob(pattern) if p.is_file())
    if package.get("license_file"):
        files.add(directory / package["license_file"])
    if not files:
        files.update(p for p in directory.parent.glob("LICENSE*") if p.is_file())
    if not files:
        override = root / "licenses" / f"{package['name']}-MIT.txt"
        if override.is_file():
            files.add(override)
        elif package.get("license") == "LGPL-2.1-or-later":
            files.add(root / "licenses/LGPL-2.1.txt")
        elif "Apache-2.0" in (package.get("license") or ""):
            files.add(root / "licenses/Apache-2.0.txt")
        else:
            raise SystemExit(f"Missing license text: {package['name']}")
    for path in sorted(files):
        parts.append(f"\n{path.name}\n{path.read_text(errors='replace')}\n")
Path(sys.argv[1]).write_text("".join(parts))

# Supply the helper's source and the statically linked LGPL library's complete
# source so recipients can rebuild/relink the helper with a modified library.
source_zip = Path(sys.argv[1]).with_name("InstallerServiceSource.zip")
with zipfile.ZipFile(source_zip, "w", compression=zipfile.ZIP_DEFLATED) as archive:
    for path in sorted(root.rglob("*")):
        if path.is_file() and not {"target", "__pycache__", ".git"}.intersection(path.relative_to(root).parts):
            archive.write(path, "NativeInstaller/" + path.relative_to(root).as_posix())
    for package in metadata["packages"]:
        if package["id"] in used and package["license"] == "LGPL-2.1-or-later":
            directory = Path(package["manifest_path"]).parent
            prefix = f"NativeInstaller/lgpl-source/{package['name']}-{package['version']}"
            for path in sorted(directory.rglob("*")):
                if path.is_file():
                    archive.write(path, prefix + "/" + path.relative_to(directory).as_posix())
            archive.write(root / "licenses/LGPL-2.1.txt", prefix + "/COPYING")
    archive.writestr("NativeInstaller/REBUILD.txt", """Build the installer service with Rust and the macOS SDK:
  cargo build --release --locked --target aarch64-apple-darwin
  cargo build --release --locked --target x86_64-apple-darwin

Dependencies are pinned in Cargo.lock. The patched idevice source is in vendor/idevice.
The complete apple-codesign-quick source is in lgpl-source. To rebuild with a modified
copy, add this entry to the existing [patch.crates-io] section of Cargo.toml:
  apple-codesign-quick = { path = "lgpl-source/apple-codesign-quick-0.1.0" }
Then build without --locked once to update the lockfile for that local source.
Combine the two output executables with lipo -create, replace Contents/MacOS/
dsh-installer-service in a copy of the installer, and ad-hoc sign the helper and app.
The source is provided for modifying and relinking the service. The iOS apps are
separate works and are not needed to rebuild the service.
""")
