mod account;
mod certificate;
mod developer_image;
mod device;
mod protocol;
mod renewal;
mod signing;
mod state;

#[tokio::main]
async fn main() {
    unsafe {
        libc::setpgid(0, 0);
        libc::umask(0o077);
    }
    // Neither tracing nor upstream panic details may expose authentication state.
    std::panic::set_hook(Box::new(|_| {
        protocol::failure(&protocol::Failure::new(
            "internal",
            "安装器遇到内部错误，请重试。",
        ))
    }));
    initialize_tls();
    let result = run().await;
    if let Err(error) = result {
        protocol::failure(&error);
        std::process::exit(1);
    }
}

fn initialize_tls() {
    // Login can run before a USB connection installs idevice's TLS provider.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    #[test]
    fn explicit_device_is_preserved_and_missing_values_are_rejected() {
        let args = |values: &[&str]| values.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            super::selected_device(&args(&[
                "service",
                "install",
                "kit",
                "--device",
                "phone-b",
                "--enable-auto-renew"
            ]))
            .unwrap(),
            Some("phone-b")
        );
        assert_eq!(
            super::selected_device(&args(&["service", "repair-runtime", "--device", "phone-a"]))
                .unwrap(),
            Some("phone-a")
        );
        assert!(super::selected_device(&args(&["service", "install", "kit", "--device"])).is_err());
        assert!(
            super::selected_device(&args(&[
                "service",
                "install",
                "kit",
                "--device",
                "--enable-auto-renew"
            ]))
            .is_err()
        );
    }
    #[test]
    fn http_client_initializes_without_a_usb_connection() {
        super::initialize_tls();
        reqwest::Client::builder().build().unwrap();
    }
}

fn selected_device(args: &[String]) -> protocol::Result<Option<&str>> {
    let Some(index) = args.iter().position(|arg| arg == "--device") else {
        return Ok(None);
    };
    let value = args
        .get(index + 1)
        .filter(|value| !value.is_empty() && !value.starts_with("--"))
        .ok_or(protocol::Failure::new("device", "请先选择目标设备。"))?;
    Ok(Some(value.as_str()))
}

async fn run() -> protocol::Result<()> {
    use protocol::Safe;
    let args: Vec<_> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("devices") => device::connected_devices().await,
        Some("diagnose") => {
            let phone = device::Phone::select(selected_device(&args)?, false).await?;
            let mounted = developer_image::mounted(&phone).await?;
            let developer_mode = phone.developer_mode_status().await;
            protocol::emit(
                serde_json::json!({"type":"diagnostic","usb":true,"name":phone.name,"ios":phone.version,"developer_image":mounted,"developer_mode":developer_mode}),
            );
            Ok(())
        }
        Some("repair-runtime") => {
            protocol::step("device");
            let mut phone = device::Phone::select(selected_device(&args)?, true).await?;
            let _phone_lock = phone.install_lock()?;
            phone.require_control_pair().await?;
            phone.developer_mode(true).await?;
            developer_image::prepare(&phone).await?;
            // Preparing a DDI does not prove that the signed runner can launch or connect.
            protocol::emit(serde_json::json!({"type":"runtime_ready"}));
            Ok(())
        }
        Some("install")
        | Some("install-host")
        | Some("install-runner")
        | Some("repair-control") => {
            let target = match args[1].as_str() {
                "install-host" => signing::InstallTarget::Host,
                "install-runner" | "repair-control" => signing::InstallTarget::Runner,
                _ => signing::InstallTarget::All,
            };
            let root = args
                .get(2)
                .ok_or(protocol::Failure::new("package", "缺少安装包路径。"))?;
            let auto = target == signing::InstallTarget::All
                && args.iter().any(|a| a == "--enable-auto-renew");
            protocol::step("checking");
            let package = signing::Package::open_for_install(std::path::Path::new(root), target)?;
            protocol::step("device");
            let mut phone = device::Phone::select(selected_device(&args)?, true).await?;
            package.check_ios(&phone.version)?;
            let _phone_lock = phone.install_lock()?;
            phone.developer_mode(true).await?;
            if target == signing::InstallTarget::All {
                developer_image::prepare(&phone).await?;
            }
            protocol::step("account");
            let mut account = account::Account::open(None, true, auto).await?;
            let team = account.team(None)?;
            let mut identity = signing::Identity::resolve(&phone, &team.team_id).await?;
            let pairing = if target == signing::InstallTarget::Runner {
                let host = package.host_requirement()?;
                let pairing = signing::Pairing::find(&host, &phone, &mut identity).await?;
                if pairing.is_none() {
                    phone
                        .require_matching_host(&identity, &host.version, &team.team_id)
                        .await?;
                }
                pairing
            } else {
                None
            };
            let binding = renewal::Binding::for_phone(&identity, &phone.udid, &team.team_id)?;
            let _lock = binding.lock()?;
            let work = tempfile::tempdir().safe("storage", "无法建立签名临时目录。")?;
            protocol::step("signing");
            let pair = signing::sign(
                &package,
                work.path(),
                &phone,
                &mut account,
                &team,
                &identity,
                target,
                pairing.as_ref(),
            )
            .await?;
            if target == signing::InstallTarget::Runner {
                protocol::step("device");
                developer_image::prepare(&phone).await?;
            }
            // The old agent must not restore an obsolete IPA after a partial foreground update.
            let pause_renewal = target != signing::InstallTarget::Runner
                || pairing.is_some()
                || package.is_runner_only();
            let renewal_paused =
                target != signing::InstallTarget::All && pause_renewal && binding.current.exists();
            if pause_renewal {
                binding.pause_existing()?;
            }
            renewal::install_apps(&phone, &pair, &identity).await?;
            if let Some(pairing) = &pairing {
                phone.pair_store_host(pairing).await?;
            }
            protocol::step("opening");
            if target != signing::InstallTarget::All {
                protocol::emit(
                    serde_json::json!({"type":if args[1] == "repair-control" {"control_repaired"} else {"single_installed"}, "component":if target == signing::InstallTarget::Host {"host"} else {"runner"}, "expires":renewal::date(pair.expires), "renewal_paused":renewal_paused, "store_host":pairing.is_some()}),
                );
                return Ok(());
            }
            if auto {
                let receipt = renewal::Receipt {
                    backend: "native-v1".into(),
                    device: phone.udid,
                    phone: phone.name,
                    team: team.team_id,
                    account: account.info.id,
                    identity,
                    expires: pair.expires,
                    versions: pair.versions,
                    next_check: 0,
                    paused: false,
                    status: "已开启，临近到期时连接 USB 会自动续签。".into(),
                };
                binding.enable(&package, &receipt)?;
            } else {
                binding.disable()?;
            }
            protocol::emit(
                serde_json::json!({"type":"complete","launched":false,"renewal":auto,"expires":renewal::date(pair.expires)}),
            );
            Ok(())
        }
        Some("status") => renewal::status(),
        Some("disable") => {
            let id = args
                .get(2)
                .ok_or(protocol::Failure::new("binding", "缺少续签配置标识。"))?;
            let binding = renewal::Binding::new(id)?;
            let _lock = binding.lock()?;
            binding.disable()?;
            renewal::status()
        }
        Some("renew") | Some("renew-now") => {
            let id = args
                .get(2)
                .ok_or(protocol::Failure::new("binding", "缺少续签配置标识。"))?;
            let manual = args[1] == "renew-now";
            if manual {
                renewal::Binding::new(id)?.use_cached_worker()?;
            }
            renewal::tick(id, manual).await?;
            if manual {
                renewal::status()?;
            }
            Ok(())
        }
        Some("login") => {
            protocol::step("account");
            let account = account::Account::open(None, true, true).await?;
            protocol::emit(serde_json::json!({"type":"account_ready","name":account.info.email}));
            Ok(())
        }
        Some("verify-package") => {
            let root = args
                .get(2)
                .ok_or(protocol::Failure::new("package", "缺少安装包路径。"))?;
            let root = std::path::Path::new(root);
            let (package, target) = if args.iter().any(|arg| arg == "--runner-only") {
                (
                    signing::Package::open_runner(root)?,
                    signing::InstallTarget::Runner,
                )
            } else {
                (signing::Package::open(root)?, signing::InstallTarget::All)
            };
            let work = tempfile::tempdir().safe("storage", "无法建立测试目录。")?;
            let identity = signing::Identity::new("TEST123456", false)?;
            let apps = package.extract(work.path(), &identity, target)?;
            let cert_dir = args.get(3).ok_or(protocol::Failure::new(
                "test_identity",
                "缺少测试签名证书。",
            ))?;
            let cert_dir = std::path::Path::new(cert_dir);
            let key = zeroize::Zeroizing::new(
                std::fs::read(cert_dir.join("key.der"))
                    .safe("test_identity", "无法读取测试签名证书。")?,
            );
            let cert = std::fs::read(cert_dir.join("cert.der"))
                .safe("test_identity", "无法读取测试签名证书。")?;
            let signer = apple_codesign_quick::RustCryptoCmsSigner::from_der(&key, &cert, [&cert])
                .safe("test_identity", "测试签名证书无效。")?;
            for (_, app) in &apps {
                let settings = apple_codesign_quick::BundleSigningSettings::new(
                    "TEST123456",
                    plist::Dictionary::new(),
                    Some(&signer),
                );
                apple_codesign_quick::sign_bundle(app, &settings)
                    .safe("signing", "独立签名器未通过安装包验证。")?;
                let check = std::process::Command::new("/usr/bin/codesign")
                    .args(["--verify", "--deep", "--strict", "--verbose=4"])
                    .arg(app)
                    .output()
                    .safe("signature", "无法检查 App 签名。")?;
                if !check.status.success() {
                    eprintln!("{}", String::from_utf8_lossy(&check.stderr));
                }
                signing::verify_signature(app)?;
            }
            protocol::emit(serde_json::json!({"type":"package_verified","apps":apps.len()}));
            Ok(())
        }
        Some("logout") => account::logout(),
        _ => Err(protocol::Failure::new("command", "安装器命令无效。")),
    }
}
