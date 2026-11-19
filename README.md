# Pallas iPhone Runner

Runner 是 Pallas Agent 的 iPhone 操控组件，负责读屏、点按、输入和启动 App。它作为 `ScreenRunner.ipa` 独立发布。Pallas Agent 在手机上授权一轮操作，Runner 在本机执行。

[下载 Runner v1.0.0 和 macOS 安装套件](https://github.com/deemo0666/pallas-iphone-runner/releases/tag/v1.0.0)

## 安装

在 iPhone 上先安装支持「允许电脑操控」的 Pallas Agent。Runner `1.0 (1)` 对应 Pallas Agent `0.5.1`。同团队侧载安装需要 `0.5.1 (18)` 和匹配的共享权限。通过 [DSH 插件](https://github.com/deemo0666/pallas-iphone-dsh-plugin) 的「安装与修复」可以给已装好 Pallas Agent 的 iPhone 签名安装 Runner。

## 源码与打包

- [`Runner`](Tools/ScreenRunner/Runner) 是 iOS XCTest Runner。[`Shared`](Tools/ScreenRunner/Shared) 是 Pallas Agent 与 Runner 共用的本机通信代码。
- [`NativeInstaller`](Tools/ScreenRunner/NativeInstaller) 是 Mac 的 USB 安装服务，使用 Cargo.lock 锁定依赖。使用 Swift 的设备端代码在完整的 Pallas Agent 开发工程里通过 `DSHScreen` 的 `build-for-testing` 构建。
- [`RunnerHost.json`](Tools/ScreenRunner/RunnerHost.json) 是当前构建所需的 Pallas Agent 兼容信息。

已有 `ScreenRunner.ipa` 时，可以在带 Xcode 的 Mac 上生成供 DSH 插件使用的安装套件。打包会先运行签名密钥的保存与重复读取回归检查，再构建通用的 Mac 安装服务。

```sh
mkdir -p dist
rustup target add aarch64-apple-darwin x86_64-apple-darwin
bash Tools/ScreenRunner/package-runner-kit.sh ./ScreenRunner.ipa dist/Pallas-Runner-macos-kit.zip
```

安装套件内的 `InstallerServiceSource.zip` 附有重新链接说明和相关第三方许可。
