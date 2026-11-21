use crate::protocol::{self, Failure, Result, Safe};
use idevice::{
    IdeviceService,
    amfi::AmfiClient,
    installation_proxy::InstallationProxyClient,
    lockdown::LockdownClient,
    provider::{IdeviceProvider, UsbmuxdProvider},
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdDevice},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::BTreeMap, time::Duration};

pub fn address() -> UsbmuxdAddr {
    UsbmuxdAddr::UnixSocket("/var/run/usbmuxd".into())
}
pub async fn usb() -> Result<Vec<UsbmuxdDevice>> {
    let mut mux = address()
        .connect(1)
        .await
        .safe("usb", "无法连接 macOS 的 iPhone 服务，请重新连接手机。")?;
    let devices = mux
        .get_devices()
        .await
        .safe("usb", "无法读取已连接的 iPhone。")?;
    Ok(devices
        .into_iter()
        .filter(|d| matches!(d.connection_type, Connection::Usb))
        .collect())
}

// Read-only discovery: never pair, mount a developer image, or prompt for credentials.
pub async fn connected_devices() -> Result<()> {
    let devices = tokio::time::timeout(Duration::from_secs(5), usb())
        .await
        .safe("usb", "设备检测超时，请重新连接数据线。")??;
    let mut entries = Vec::new();
    for device in devices {
        let provider = device.to_provider(address(), "Pallas Agent Installer");
        let details = tokio::time::timeout(Duration::from_secs(2), async {
            let mut lockdown = LockdownClient::connect(&provider).await.ok()?;
            let name = lockdown.get_value(Some("DeviceName"), None).await.ok()?;
            let version = lockdown
                .get_value(Some("ProductVersion"), None)
                .await
                .ok()?;
            Some((
                name.as_string()?.to_owned(),
                version.as_string()?.to_owned(),
            ))
        })
        .await
        .ok()
        .flatten();
        let (name, version) = details.unwrap_or_default();
        entries.push(json!({"id":device.udid, "name":name, "version":version}));
    }
    protocol::emit(json!({"type":"devices", "entries":entries}));
    Ok(())
}

pub struct Phone {
    pub udid: String,
    pub name: String,
    pub version: String,
    pub provider: UsbmuxdProvider,
}

impl Phone {
    pub fn install_lock(&self) -> Result<std::fs::File> {
        use fs2::FileExt;
        let directory = crate::state::support()?.join("DeviceLocks");
        crate::state::private_dir(&directory)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join(crate::state::hash(self.udid.as_bytes())))
            .safe("lock", "无法取得安装锁。")?;
        file.try_lock_exclusive()
            .safe("busy", "这台 iPhone 的安装或续签正在运行。")?;
        Ok(file)
    }

    pub async fn connect(device: &UsbmuxdDevice, interactive: bool) -> Result<Self> {
        let provider = device.to_provider(address(), "Pallas Agent Installer");
        let mut lockdown = LockdownClient::connect(&provider)
            .await
            .safe("connection", "无法连接 iPhone，请解锁手机并重新插线。")?;
        let name = lockdown
            .get_value(Some("DeviceName"), None)
            .await
            .ok()
            .and_then(|v| v.as_string().map(str::to_owned))
            .unwrap_or_else(|| "iPhone".into());
        let version = lockdown
            .get_value(Some("ProductVersion"), None)
            .await
            .ok()
            .and_then(|v| v.as_string().map(str::to_owned))
            .unwrap_or_default();
        let paired = match provider.get_pairing_file().await {
            Ok(record) => lockdown.start_session(&record).await.is_ok(),
            Err(_) => false,
        };
        if !paired {
            if !interactive {
                return Err(Failure::new(
                    "trust",
                    "请解锁 iPhone，并在安装器中重新连接。",
                ));
            }
            protocol::emit(
                json!({"type":"waiting", "reason":"trust", "message":"解锁 iPhone，在手机上点信任这台电脑并输入手机密码。"}),
            );
            let mut mux = address()
                .connect(2)
                .await
                .safe("usb", "无法连接 iPhone 服务。")?;
            let buid = mux
                .get_buid()
                .await
                .safe("pair", "无法取得此 Mac 的配对标识。")?;
            let mut lockdown = LockdownClient::connect(&provider)
                .await
                .safe("pair", "请重新连接 iPhone。")?;
            let record = tokio::time::timeout(
                Duration::from_secs(300),
                lockdown.pair(
                    uuid::Uuid::new_v4().to_string(),
                    buid,
                    Some("Pallas Agent Installer"),
                ),
            )
            .await
            .safe("trust", "等候信任超时，请解锁手机后重试。")?
            .safe("trust", "未完成 iPhone 信任，请解锁手机后重试。")?;
            mux.save_pair_record(
                &device.udid,
                record.serialize().safe("pair", "无法保存设备配对。")?,
            )
            .await
            .safe("pair", "macOS 未能保存设备配对，请重试。")?;
        }
        Ok(Self {
            udid: device.udid.clone(),
            name,
            version,
            provider,
        })
    }

    pub async fn select(wanted: Option<&str>, interactive: bool) -> Result<Self> {
        for attempt in 0..150 {
            let devices = usb().await?;
            let selected: Vec<_> = devices
                .iter()
                .filter(|d| wanted.is_none_or(|id| id == d.udid))
                .collect();
            if !selected.is_empty() {
                let mut names = Vec::new();
                for d in &selected {
                    let p = d.to_provider(address(), "Pallas Agent Installer");
                    let name = if let Ok(mut l) = LockdownClient::connect(&p).await {
                        l.get_value(Some("DeviceName"), None)
                            .await
                            .ok()
                            .and_then(|v| v.as_string().map(str::to_owned))
                    } else {
                        None
                    };
                    names.push(format!(
                        "{} ({})",
                        name.unwrap_or_else(|| "iPhone".into()),
                        &d.udid[d.udid.len().saturating_sub(6)..]
                    ));
                }
                let index = if interactive {
                    protocol::choose("选择要安装的 iPhone", &names)?
                } else {
                    0
                };
                let phone = Self::connect(selected[index], interactive).await?;
                protocol::emit(json!({"type":"device", "name":phone.name}));
                return Ok(phone);
            }
            if !interactive {
                break;
            }
            if attempt == 0 {
                protocol::emit(
                    json!({"type":"waiting","reason":"connection","message":"用数据线连接并解锁 iPhone，连接后会自动继续。"}),
                );
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Err(Failure::new(
            "disconnected",
            "没有检测到通过 USB 连接的 iPhone。",
        ))
    }

    pub async fn developer_mode_status(&self) -> Option<bool> {
        if let Ok(mut amfi) = AmfiClient::connect(&self.provider).await
            && let Ok(enabled) = amfi.get_developer_mode_status().await
        {
            return Some(enabled);
        }
        // Older iOS versions expose this through the image mounter instead of AMFI.
        if let Ok(mut mounter) =
            idevice::mobile_image_mounter::ImageMounter::connect(&self.provider).await
        {
            return mounter.query_developer_mode_status().await.ok();
        }
        None
    }

    pub async fn developer_mode(&mut self, interactive: bool) -> Result<()> {
        let mut last_state = None;
        for attempt in 0..150 {
            let state = self.developer_mode_status().await;
            if state == Some(true) {
                return Ok(());
            }
            if state == Some(false)
                && interactive
                && attempt == 0
                && let Ok(mut amfi) = AmfiClient::connect(&self.provider).await
            {
                let _ = amfi.reveal_developer_mode_option_in_ui().await;
            }
            if !interactive {
                last_state = state;
                break;
            }
            if attempt == 0 || state != last_state {
                if state == Some(false) {
                    protocol::emit(json!({"type":"waiting","reason":"developerMode"}));
                } else {
                    protocol::emit(
                        json!({"type":"waiting","reason":"connection","message":"无法读取开发者模式状态，请解锁并重新连接 iPhone。"}),
                    );
                }
            }
            last_state = state;
            tokio::time::sleep(Duration::from_secs(3)).await;
            if let Ok(devices) = usb().await
                && let Some(d) = devices.iter().find(|d| d.udid == self.udid)
            {
                self.provider = d.to_provider(address(), "Pallas Agent Installer");
            }
        }
        if last_state.is_none() {
            return Err(Failure::new(
                "connection",
                "无法读取开发者模式状态，请解锁并重新连接 iPhone。",
            ));
        }
        Err(Failure::new(
            "developer_mode",
            "开发者模式尚未就绪，请在手机重启后解锁并确认开启，再重试。",
        ))
    }

    pub async fn apps(&self, ids: &[String]) -> Result<BTreeMap<String, AppVersion>> {
        let mut proxy = InstallationProxyClient::connect(&self.provider)
            .await
            .safe("locked", "无法读取手机上的 App，请解锁 iPhone。")?;
        let values = proxy
            .get_apps(Some("User"), Some(ids.to_vec()))
            .await
            .safe("apps", "无法确认手机上的 App，请解锁 iPhone 后重试。")?;
        Ok(values
            .into_iter()
            .filter_map(|(id, v)| {
                let dict = v.as_dictionary()?;
                Some((
                    id,
                    AppVersion {
                        version: dict
                            .get("CFBundleShortVersionString")?
                            .as_string()?
                            .to_owned(),
                        build: dict.get("CFBundleVersion")?.as_string()?.to_owned(),
                    },
                ))
            })
            .collect())
    }

    /// Whether the host from TestFlight is installed and can be given the runner's key.
    pub async fn store_host(&self, id: &str, version: &str) -> Result<bool> {
        let mut proxy = InstallationProxyClient::connect(&self.provider)
            .await
            .safe("locked", "无法读取手机上的 App，请解锁 iPhone。")?;
        let apps = proxy
            .get_apps(Some("User"), Some(vec![id.to_owned()]))
            .await
            .safe("apps", "无法确认手机上的 App，请解锁 iPhone 后重试。")?;
        let Some(host) = apps.get(id).and_then(plist::Value::as_dictionary) else {
            return Ok(false);
        };
        check_store_host(host, version)?;
        Ok(true)
    }

    /// Leaves the key where only the host and a computer this phone trusts can read it.
    pub async fn pair_store_host(&self, pairing: &crate::signing::Pairing) -> Result<()> {
        let failure = || {
            Failure::new(
                "store_pairing",
                "手机控制已安装，但未能与主 App 配对。请解锁 iPhone 后重新安装操控组件。",
            )
        };
        let mut documents = idevice::house_arrest::HouseArrestClient::connect(&self.provider)
            .await
            .map_err(|_| failure())?
            .vend_documents(pairing.host.clone())
            .await
            .map_err(|_| failure())?;
        let mut file = documents
            .open(
                crate::signing::Pairing::KEY_FILE,
                idevice::afc::opcode::AfcFopenMode::WrOnly,
            )
            .await
            .map_err(|_| failure())?;
        file.write_entire(pairing.key.as_bytes())
            .await
            .map_err(|_| failure())?;
        file.close().await.map_err(|_| failure())
    }

    pub async fn require_control_pair(&self) -> Result<()> {
        let mut proxy = InstallationProxyClient::connect(&self.provider)
            .await
            .safe("locked", "无法读取手机上的 App，请解锁 iPhone。")?;
        let apps = proxy
            .get_apps(Some("User"), None)
            .await
            .safe("apps", "无法确认手机上的 App，请解锁 iPhone 后重试。")?;
        // A runner paired with the host from TestFlight names that host itself.
        let stored = |app: &plist::Value| {
            let host = app.as_dictionary()?.get("DSHScreenHost")?.as_string()?;
            apps.contains_key(host).then_some(())
        };
        if apps.values().any(|app| stored(app).is_some()) {
            return Ok(());
        }
        check_control_pair(&apps.keys().map(String::as_str).collect::<Vec<_>>())
    }

    pub async fn require_matching_host(
        &self,
        identity: &crate::signing::Identity,
        version: &AppVersion,
        team: &str,
    ) -> Result<()> {
        let mut proxy = InstallationProxyClient::connect(&self.provider)
            .await
            .safe("locked", "无法读取手机上的 App，请解锁 iPhone。")?;
        let apps = proxy
            .get_apps(Some("User"), Some(vec![identity.host.clone()]))
            .await
            .safe("apps", "无法确认手机上的 App，请解锁 iPhone 后重试。")?;
        let host = apps
            .get(&identity.host)
            .and_then(plist::Value::as_dictionary)
            .ok_or(Failure::new(
                "repair_host_missing",
                "未找到此账号安装的主 App。请使用原安装账号，或先安装主 App。",
            ))?;
        validate_host(host, identity, version, team)
    }
}

fn validate_host(
    host: &plist::Dictionary,
    identity: &crate::signing::Identity,
    version: &AppVersion,
    team: &str,
) -> Result<()> {
    // The package names the oldest host it works with; newer hosts are fine.
    if !host_not_older(
        host.get("CFBundleShortVersionString").and_then(plist::Value::as_string),
        host.get("CFBundleVersion").and_then(plist::Value::as_string),
        version,
    ) {
        return Err(Failure::new(
            "repair_host_version",
            "主 App 版本太旧，无法单独补装组件。请先更新主 App。",
        ));
    }
    let entitlements = host
        .get("Entitlements")
        .and_then(plist::Value::as_dictionary)
        .ok_or(Failure::new(
            "repair_host_identity",
            "无法确认主 App 的共享权限，已停止补装组件。",
        ))?;
    crate::signing::validate_entitlements(entitlements, team, &identity.host, &identity.group)?;
    if host
        .get("DSHScreenRunnerScheme")
        .and_then(plist::Value::as_string)
        != Some(&identity.scheme)
    {
        return Err(Failure::new(
            "repair_host_identity",
            "主 App 的操控组件标识不匹配，请使用原安装器更新。",
        ));
    }
    Ok(())
}

fn check_store_host(host: &plist::Dictionary, version: &str) -> Result<()> {
    // `version` is the oldest host the runner works with; any later one is fine.
    if !host
        .get("CFBundleShortVersionString")
        .and_then(plist::Value::as_string)
        .is_some_and(|have| version_at_least(have, version))
    {
        return Err(Failure::new(
            "store_host_version",
            "主 App 版本太旧，请先更新主 App。",
        ));
    }
    if host
        .get("UIFileSharingEnabled")
        .and_then(plist::Value::as_boolean)
        != Some(true)
    {
        return Err(Failure::new(
            "store_host_sharing",
            "此版本的主 App 不能与单独安装的操控组件配对，请更新主 App。",
        ));
    }
    Ok(())
}

fn version_parts(value: &str) -> Option<Vec<u64>> {
    value.split('.').map(|part| part.parse().ok()).collect()
}

/// Whether dotted version `have` is `want` or later (missing components count as 0).
fn version_at_least(have: &str, want: &str) -> bool {
    let (Some(mut have), Some(mut want)) = (version_parts(have), version_parts(want)) else {
        return false;
    };
    let len = have.len().max(want.len());
    have.resize(len, 0);
    want.resize(len, 0);
    have >= want
}

fn host_not_older(version: Option<&str>, build: Option<&str>, oldest: &AppVersion) -> bool {
    let Some(version) = version else { return false };
    if !version_at_least(version, &oldest.version) {
        return false;
    }
    // A later version is enough; the same version needs the same build or a later one.
    !version_at_least(&oldest.version, version)
        || build.is_some_and(|build| version_at_least(build, &oldest.build))
}

fn check_control_pair(ids: &[&str]) -> Result<()> {
    let mut hosts = 0;
    for host in ids {
        let Some(team) = host
            .strip_prefix("io.dshlocal.sideload.t")
            .or_else(|| host.strip_prefix("io.dshgo.sideload.t"))
        else {
            continue;
        };
        if team.len() != 10 || !team.bytes().all(|b| b.is_ascii_alphanumeric()) {
            continue;
        }
        hosts += 1;
        if !ids.contains(&format!("{host}.screen.xctrunner").as_str()) {
            return Err(Failure::new(
                "repair_runner_missing",
                "未找到与主 App 配套的 Pallas 手机控制。请点击“安装操控组件”；修复只恢复运行环境。",
            ));
        }
    }
    if hosts == 0 {
        return Err(Failure::new(
            "repair_host_missing",
            "未找到与操控组件配套的 Pallas Agent，请先安装主 App。",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppVersion {
    pub version: String,
    pub build: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_host_must_be_new_enough_and_accept_a_key_file() {
        let mut host = plist::Dictionary::new();
        host.insert("CFBundleShortVersionString".into(), "0.2.0".into());
        assert_eq!(
            check_store_host(&host, "0.2.0").unwrap_err().code,
            "store_host_sharing"
        );
        host.insert("UIFileSharingEnabled".into(), true.into());
        assert!(check_store_host(&host, "0.2.0").is_ok());
        assert!(check_store_host(&host, "0.1.9").is_ok());
        assert!(check_store_host(&host, "0.2").is_ok());
        assert_eq!(
            check_store_host(&host, "0.3.0").unwrap_err().code,
            "store_host_version"
        );
        assert_eq!(
            check_store_host(&host, "0.2.1").unwrap_err().code,
            "store_host_version"
        );
    }

    #[test]
    fn component_repair_refuses_an_unmatched_host_before_signing() {
        let identity = crate::signing::Identity::new("ABCDE12345", false).unwrap();
        let version = AppVersion {
            version: "0.2.0".into(),
            build: "2".into(),
        };
        let mut entitlements = plist::Dictionary::new();
        entitlements.insert(
            "application-identifier".into(),
            format!("ABCDE12345.{}", identity.host).into(),
        );
        entitlements.insert(
            "com.apple.developer.team-identifier".into(),
            "ABCDE12345".into(),
        );
        entitlements.insert(
            "com.apple.security.application-groups".into(),
            plist::Value::Array(vec![identity.group.clone().into()]),
        );
        entitlements.insert("get-task-allow".into(), true.into());
        let mut host = plist::Dictionary::new();
        host.insert(
            "CFBundleShortVersionString".into(),
            version.version.clone().into(),
        );
        host.insert("CFBundleVersion".into(), version.build.clone().into());
        host.insert(
            "DSHScreenRunnerScheme".into(),
            identity.scheme.clone().into(),
        );
        host.insert("Entitlements".into(), entitlements.into());
        validate_host(&host, &identity, &version, "ABCDE12345").unwrap();
        assert_eq!(
            validate_host(&host, &identity, &version, "OTHER12345")
                .unwrap_err()
                .code,
            "profile_mismatch"
        );
        host.insert("DSHScreenRunnerScheme".into(), "old-runner".into());
        assert_eq!(
            validate_host(&host, &identity, &version, "ABCDE12345")
                .unwrap_err()
                .code,
            "repair_host_identity"
        );
        host.insert(
            "DSHScreenRunnerScheme".into(),
            identity.scheme.clone().into(),
        );
        host.insert("CFBundleVersion".into(), "1".into());
        assert_eq!(
            validate_host(&host, &identity, &version, "ABCDE12345")
                .unwrap_err()
                .code,
            "repair_host_version"
        );
        host.insert("CFBundleShortVersionString".into(), "0.1.9".into());
        host.insert("CFBundleVersion".into(), "9".into());
        assert_eq!(
            validate_host(&host, &identity, &version, "ABCDE12345")
                .unwrap_err()
                .code,
            "repair_host_version"
        );
        host.insert("CFBundleVersion".into(), "3".into());
        host.insert("CFBundleShortVersionString".into(), "0.2.0".into());
        validate_host(&host, &identity, &version, "ABCDE12345").unwrap();
        host.insert("CFBundleShortVersionString".into(), "0.5.5".into());
        host.insert("CFBundleVersion".into(), "1".into());
        validate_host(&host, &identity, &version, "ABCDE12345").unwrap();
    }

    #[test]
    fn runtime_repair_requires_the_matching_installed_runner() {
        let host = "io.dshlocal.sideload.tabcde12345";
        let runner = "io.dshlocal.sideload.tabcde12345.screen.xctrunner";
        let old_setup = "io.dshlocal.setup.tabcde12345.screen.xctrunner";
        let other_team = "io.dshlocal.sideload.tother12345.screen.xctrunner";
        assert_eq!(
            check_control_pair(&[host, old_setup, other_team])
                .unwrap_err()
                .code,
            "repair_runner_missing"
        );
        assert_eq!(
            check_control_pair(&[old_setup, runner]).unwrap_err().code,
            "repair_host_missing"
        );
        check_control_pair(&[host, runner, old_setup]).unwrap();
        check_control_pair(&[
            "io.dshgo.sideload.tabcde12345",
            "io.dshgo.sideload.tabcde12345.screen.xctrunner",
        ])
        .unwrap();
    }
}
