<p align="center">
  <img src="assets/pallas.png" alt="Pallas cat mascot" width="144">
</p>

# Pallas iPhone Runner

English · [简体中文](README.zh-CN.md)

Pallas iPhone Runner is the standalone phone-control component of [Pallas Agent](https://github.com/deemo0666/pallas-agent). It reads the screen, taps, types and opens apps. Pallas Agent authorizes one round of actions on the phone, and the Runner carries it out on the same device.

[Download Pallas iPhone Runner v1.1.0 and the macOS installer kit](https://github.com/deemo0666/pallas-iphone-runner/releases/tag/v1.1.0)

## Installation

First install a Pallas Agent build that has **Allow Computer Control** on the iPhone. Runner `1.1 (2)` works with Pallas Agent `0.5.1` or later. Sideloading under the same team needs `0.5.1 (18)` or later, with the matching shared entitlements. The **Install and repair** page of the [DSH plugin](https://github.com/deemo0666/pallas-iphone-dsh-plugin) can sign and install the Runner on an iPhone that already has Pallas Agent.

## Source and packaging

- [`Runner`](Tools/ScreenRunner/Runner) is the iOS XCTest runner. [`Shared`](Tools/ScreenRunner/Shared) is the on-device communication code shared by Pallas Agent and the Runner.
- [`NativeInstaller`](Tools/ScreenRunner/NativeInstaller) is the Mac USB installer service; its dependencies are pinned by Cargo.lock. The Swift device code is built with `build-for-testing` of the `DSHScreen` scheme in the full Pallas Agent project.
- [`RunnerHost.json`](Tools/ScreenRunner/RunnerHost.json) holds the Pallas Agent compatibility data for the current build.

With a `ScreenRunner.ipa` in hand, a Mac with Xcode can build the installer kit used by the DSH plugin. Packaging first runs the regression check that saves and re-reads the signing key, then builds the universal Mac installer service.

```sh
mkdir -p dist
rustup target add aarch64-apple-darwin x86_64-apple-darwin
bash Tools/ScreenRunner/package-runner-kit.sh ./ScreenRunner.ipa dist/Pallas-Runner-macos-kit.zip
```

## Copyright and license

Copyright © 2026 deemo0666. All rights reserved.

The project's own source code is available for review by people granted access; it is not open source. Use of the official packages and your LGPL rights in the installer service are set out in [LICENSE.md](LICENSE.md). `InstallerServiceSource.zip` in the installer kit includes instructions for rebuilding and relinking.
