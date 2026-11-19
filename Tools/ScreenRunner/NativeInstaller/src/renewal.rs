use crate::{
    account::Account,
    developer_image,
    device::{AppVersion, Phone},
    protocol::{self, Failure, Result, Safe},
    signing::{self, Identity, Package, SignedApps},
    state,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    os::unix::fs::symlink,
    path::PathBuf,
    process::{Command, Stdio},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub backend: String,
    pub device: String,
    pub phone: String,
    pub team: String,
    pub account: String,
    pub identity: Identity,
    pub expires: i64,
    pub versions: BTreeMap<String, AppVersion>,
    pub next_check: i64,
    pub paused: bool,
    pub status: String,
}

pub struct Binding {
    pub id: String,
    pub base: PathBuf,
    pub current: PathBuf,
    label: String,
    job: PathBuf,
}
impl Binding {
    pub fn new(id: &str) -> Result<Self> {
        if id.len() != 24
            || !id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(Failure::new("binding", "自动续签配置标识无效。"));
        }
        let base = state::support()?.join("Renewals").join(id);
        let label = format!("io.dsh.sideload.renew.{id}");
        let home = PathBuf::from(
            std::env::var_os("HOME").ok_or(Failure::new("home", "无法读取用户目录。"))?,
        );
        Ok(Self {
            id: id.into(),
            current: base.join("current"),
            base,
            job: home
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist")),
            label,
        })
    }
    pub fn for_phone(identity: &Identity, phone: &str, team: &str) -> Result<Self> {
        Self::new(
            &state::hash(format!("{}/{phone}/{team}", identity.cache_name()).as_bytes())[..24],
        )
    }
    fn domain(&self) -> String {
        format!("gui/{}", unsafe { libc::getuid() })
    }
    pub fn lock(&self) -> Result<File> {
        let parent = self.base.parent().unwrap();
        state::private_dir(parent)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(parent.join(format!("{}.lock", self.label)))
            .safe("lock", "无法取得安装锁。")?;
        file.try_lock_exclusive()
            .safe("busy", "这台 iPhone 的安装或续签正在运行。")?;
        Ok(file)
    }
    fn loaded(&self) -> bool {
        Command::new("/bin/launchctl")
            .args(["print", &format!("{}/{}", self.domain(), self.label)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
    fn started(&self) -> bool {
        let Ok(worker) = self.current.join("dsh-installer-service").canonicalize() else {
            return false;
        };
        fs::read(self.current.join("worker-started.json"))
            .ok()
            .and_then(|data| serde_json::from_slice::<PathBuf>(&data).ok())
            .is_some_and(|started| started == worker)
    }
    fn record_start(&self) -> Result<()> {
        // A foreground copy must not certify that launchd's cached executable ran.
        self.receipt()?;
        let worker = self
            .current
            .join("dsh-installer-service")
            .canonicalize()
            .safe("cache", "无法定位自动续签程序。")?;
        let running = std::env::current_exe()
            .and_then(fs::canonicalize)
            .safe("cache", "无法定位自动续签程序。")?;
        if running == worker {
            state::write_json(&self.current.join("worker-started.json"), &worker)?;
        }
        Ok(())
    }
    fn wait_for_start(&self, timeout: std::time::Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.started() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(Failure::new(
                    "agent_start",
                    "自动续签程序未能启动。请在系统设置 → 隐私与安全性检查 dsh-installer-service 的拦截提示，再重新检查自动续签。",
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    pub fn use_cached_worker(&self) -> Result<()> {
        use std::os::unix::process::CommandExt;
        self.receipt()?;
        let worker = self
            .current
            .join("dsh-installer-service")
            .canonicalize()
            .safe("cache", "无法定位自动续签程序。")?;
        let running = std::env::current_exe()
            .and_then(fs::canonicalize)
            .safe("cache", "无法定位自动续签程序。")?;
        if worker != running {
            // Replace, rather than spawn, so cancellation still reaches the same PID/group.
            // Manual verification must use the exact executable used by launchd.
            let _ = Command::new(worker).args(["renew-now", &self.id]).exec();
            return Err(Failure::new(
                "agent_start",
                "自动续签程序未能启动。请在系统设置 → 隐私与安全性检查 dsh-installer-service 的拦截提示，再重新检查自动续签。",
            ));
        }
        Ok(())
    }
    pub fn receipt(&self) -> Result<Receipt> {
        let path = self
            .current
            .canonicalize()
            .safe("receipt", "自动续签缓存已丢失，请重新安装并开启续签。")?;
        let base = self
            .base
            .canonicalize()
            .safe("receipt", "自动续签缓存无效。")?;
        if path.parent() != Some(&base)
            || !path
                .file_name()
                .is_some_and(|s| s.to_string_lossy().starts_with("kit-"))
        {
            return Err(Failure::new("receipt", "自动续签缓存路径无效。"));
        }
        let data = fs::read(path.join("renewal.json")).safe("receipt", "无法读取自动续签配置。")?;
        let receipt: Receipt = serde_json::from_slice(&data).safe(
            "migration",
            "此配置来自旧版安装器，请重新安装以升级免 Xcode 续签。",
        )?;
        if receipt.backend != "native-v1"
            || Self::for_phone(&receipt.identity, &receipt.device, &receipt.team)?.id != self.id
        {
            return Err(Failure::new("receipt", "自动续签配置不匹配，请重新安装。"));
        }
        Ok(receipt)
    }
    pub fn save(&self, receipt: &Receipt) -> Result<()> {
        state::write_json(&self.current.join("renewal.json"), receipt)
    }
    pub fn pause_existing(&self) -> Result<()> {
        if !self.current.exists() {
            return Ok(());
        }
        let path = self.current.join("renewal.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).safe("receipt", "无法暂停原有续签配置。")?)
                .safe("receipt", "原有续签配置无效。")?;
        value["paused"] = true.into();
        value["status"] = "自动续签已暂停。请使用“全部安装”并开启自动续签以恢复。".into();
        state::write_json(&path, &value)
    }
    pub fn enable(&self, package: &Package, receipt: &Receipt) -> Result<()> {
        state::private_dir(&self.base)?;
        state::private_dir(self.job.parent().unwrap())?;
        let stage = tempfile::Builder::new()
            .prefix("kit-")
            .tempdir_in(&self.base)
            .safe("cache", "无法建立续签缓存。")?;
        for file in [package.host_file()?, "ScreenRunner.ipa", "SHA256SUMS"] {
            fs::copy(package.root.join(file), stage.path().join(file))
                .safe("cache", "无法保存续签安装包。")?;
        }
        let worker = std::env::current_exe().safe("cache", "无法定位自动续签程序。")?;
        fs::copy(worker, stage.path().join("dsh-installer-service"))
            .safe("cache", "无法保存自动续签程序。")?;
        state::write_json(&stage.path().join("renewal.json"), receipt)?;
        Package::open(stage.path())?;
        let old_target = fs::read_link(&self.current).ok();
        let old_job = fs::read(&self.job).ok();
        let was_loaded = self.loaded();
        if was_loaded {
            self.unload()?;
        }
        let new_path = stage.keep();
        let next = self.base.join(".next");
        let _ = fs::remove_file(&next);
        let publish = (|| -> Result<()> {
            symlink(new_path.file_name().unwrap(), &next).safe("cache", "无法启用续签缓存。")?;
            fs::rename(&next, &self.current).safe("cache", "无法切换续签缓存。")?;
            let job = plist::Value::Dictionary(
                [
                    (String::from("Label"), self.label.clone().into()),
                    (
                        "ProgramArguments".into(),
                        plist::Value::Array(vec![
                            self.current
                                .join("dsh-installer-service")
                                .to_string_lossy()
                                .into_owned()
                                .into(),
                            "renew".into(),
                            self.id.clone().into(),
                        ]),
                    ),
                    ("StartInterval".into(), 60.into()),
                    ("RunAtLoad".into(), true.into()),
                    ("LimitLoadToSessionType".into(), "Aqua".into()),
                    ("ProcessType".into(), "Background".into()),
                    ("StandardOutPath".into(), "/dev/null".into()),
                    ("StandardErrorPath".into(), "/dev/null".into()),
                ]
                .into_iter()
                .collect(),
            );
            plist::to_file_xml(&self.job, &job).safe("agent", "无法保存自动续签后台项目。")?;
            self.load()?;
            Ok(())
        })();
        if publish.is_err() {
            let _ = self.unload();
            let _ = fs::remove_file(&self.current);
            if let Some(old) = old_target.as_ref() {
                let _ = symlink(old, &self.current);
            }
            if let Some(bytes) = old_job {
                let _ = fs::write(&self.job, bytes);
            } else {
                let _ = fs::remove_file(&self.job);
            }
            if was_loaded {
                let _ = self.load();
            }
            let _ = fs::remove_dir_all(new_path);
            return publish;
        }
        if let Some(old) = old_target
            && old.components().count() == 1
            && old.to_string_lossy().starts_with("kit-")
        {
            let _ = fs::remove_dir_all(self.base.join(old));
        }
        // Registration is not execution: Gatekeeper may reject the cached worker.
        // Keep the new cache/job on failure so the user can approve that exact file.
        self.wait_for_start(std::time::Duration::from_secs(5))
    }
    fn load(&self) -> Result<()> {
        let status = Command::new("/bin/launchctl")
            .args(["bootstrap", &self.domain()])
            .arg(&self.job)
            .output()
            .safe("agent", "无法启动自动续签后台项目。")?;
        if !status.status.success() || !self.loaded() {
            return Err(Failure::new(
                "agent",
                "自动续签后台项目未运行，请在系统设置中允许此安装器的后台活动后重试。",
            ));
        }
        Ok(())
    }
    fn unload(&self) -> Result<()> {
        if !self.loaded() {
            return Ok(());
        }
        let result = Command::new("/bin/launchctl")
            .args(["bootout", &format!("{}/{}", self.domain(), self.label)])
            .output()
            .safe("agent", "无法停止自动续签后台项目。")?;
        if !result.status.success() {
            return Err(Failure::new("agent", "自动续签后台项目尚未停止。"));
        }
        Ok(())
    }
    pub fn disable(&self) -> Result<()> {
        self.unload()?;
        if self.job.exists() {
            fs::remove_file(&self.job).safe("agent", "无法移除自动续签后台项目。")?;
        }
        if self.base.exists() {
            fs::remove_dir_all(&self.base).safe("cache", "无法移除续签缓存。")?;
        }
        Ok(())
    }
}

pub fn date(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}
pub fn status() -> Result<()> {
    let root = state::support()?.join("Renewals");
    let mut entries = vec![];
    if root.exists() {
        for entry in fs::read_dir(root)
            .safe("storage", "无法读取自动续签状态。")?
            .flatten()
        {
            if let Ok(binding) = Binding::new(&entry.file_name().to_string_lossy()) {
                if !binding.current.exists() {
                    continue;
                }
                match binding.receipt() {
                    Ok(r) => entries.push(json!({"id":binding.id,"phone":r.phone,"expires":date(r.expires),"message":r.status,"loaded":binding.loaded() && binding.started()})),
                    Err(e) => entries.push(json!({"id":binding.id,"phone":"iPhone","expires":"","message":e.message,"loaded":false})),
                }
            }
        }
    }
    protocol::emit(json!({"type":"renewals","entries":entries}));
    Ok(())
}

pub fn due(receipt: &Receipt, now: i64) -> bool {
    !receipt.paused && now >= receipt.next_check && now >= receipt.expires - 3 * 24 * 60 * 60
}

pub async fn install_apps(phone: &Phone, pair: &SignedApps, identity: &Identity) -> Result<()> {
    protocol::step("installing");
    for (id, app) in &pair.apps {
        let host = id == &identity.host;
        let component = if host { "host" } else { "runner" };
        protocol::activity(if host {
            "正在安装主 App…"
        } else {
            "正在安装 Pallas 手机控制…"
        });
        let last = std::sync::atomic::AtomicU64::new(u64::MAX);
        let result = isideload::sideload::install::install_app(&phone.provider, app, |percent| {
            if last.swap(percent, std::sync::atomic::Ordering::Relaxed) != percent {
                protocol::emit(json!({"type":"transfer","component":component,"percent":percent}));
            }
        })
        .await;
        // Only clean this operation's randomly named staging directory.
        use idevice::IdeviceService;
        if let Ok(mut afc) = idevice::afc::AfcClient::connect(&phone.provider).await {
            let _ = afc
                .remove_all(format!(
                    "PublicStaging/{}",
                    app.file_name().unwrap().to_string_lossy()
                ))
                .await;
        }
        if let Err(report) = &result {
            protocol::emit(installation_diagnostic(component, report));
        }
        result.map_err(|error| installation_error(error, host))?;
    }
    let actual = phone
        .apps(&pair.versions.keys().cloned().collect::<Vec<_>>())
        .await?;
    if actual != pair.versions {
        return Err(Failure::new(
            "install_verification",
            "设备未确认本次安装的所有 App 均已更新，本次不记为安装成功。",
        ));
    }
    if pair.versions.contains_key(&identity.host) && pair.versions.contains_key(&identity.runner) {
        protocol::emit(json!({"type":"installed"}));
    }
    Ok(())
}

fn installation_error(report: rootcause::Report, host: bool) -> Failure {
    use idevice::{IdeviceError, installation_proxy::InstallationProxyError};
    // Inspect only the installation service's typed rejection. Never format a
    // whole upstream report, which may carry unrelated authentication context.
    for node in report.iter_reports() {
        if let Some(isideload::SideloadError::IdeviceError(IdeviceError::InstallationProxy(
            error,
        ))) = node.downcast_current_context::<isideload::SideloadError>()
        {
            let (description, code) = match error {
                InstallationProxyError::OperationFailed(description) => (description.as_str(), ""),
                InstallationProxyError::DeviceReported { reason, code, .. } => {
                    (reason.as_str(), code.as_str())
                }
                _ => continue,
            };
            let reason = description.to_ascii_lowercase();
            if code.parse::<i64>() == Ok(-402620383)
                || code.eq_ignore_ascii_case("0xe8008021")
                || reason
                    .contains("maximum number of installed apps using a free developer profile")
                || reason.contains("maximum number of apps for free development profiles")
                || reason.contains("0xe8008021")
            {
                return Failure::new(
                    if host { "install_host_limit" } else { "install_runner_limit" },
                    if host {
                        "设备的免费签名 App 名额已满。请移除一个不再需要的旧侧载 App，再重新安装。"
                    } else {
                        "主 App 已安装，但免费签名 App 名额已满，Pallas 手机控制未安装。请移除一个不再需要的旧侧载 App，再重试安装。"
                    },
                ).detail("Installation", -402620383);
            }
        }
    }
    Failure::new(
        if host {
            "install_host"
        } else {
            "install_runner"
        },
        if host {
            "主 App 安装失败。请解锁设备，确认 USB 连接后重试。"
        } else {
            "Pallas 手机控制尚未装好。主 App 可能已更新，请保持 USB 连接后重试；本次不会记为续签成功。"
        },
    )
}

pub async fn tick(id: &str, manual: bool) -> Result<()> {
    // launchd must not wait indefinitely behind an invisible Keychain dialog.
    let _keychain_ui =
        security_framework::os::macos::keychain::SecKeychain::disable_user_interaction()
            .safe("keychain", "请打开安装器并解锁登录钥匙串后重试。")?;
    let binding = Binding::new(id)?;
    if !manual {
        // The installer still owns the install lock during this launch handshake.
        binding.record_start()?;
    }
    let _lock = match binding.lock() {
        Ok(lock) => lock,
        Err(e) if e.code == "busy" && !manual => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut receipt = binding.receipt()?;
    let now = chrono::Utc::now().timestamp();
    if manual && receipt.paused {
        return Err(Failure::new(
            "renewal_paused",
            "自动续签已暂停，请用当前安装器重新安装并开启续签。",
        ));
    }
    if !manual && !due(&receipt, now) {
        return Ok(());
    }
    let phone = match Phone::select(Some(&receipt.device), false).await {
        Ok(phone) => phone,
        Err(e) if e.code == "disconnected" && !manual => return Ok(()),
        Err(e) => {
            receipt.status = e.message.into();
            receipt.next_check = now + 60;
            binding.save(&receipt)?;
            if manual {
                return Err(e);
            }
            return Ok(());
        }
    };
    receipt.next_check = now + 1800;
    receipt.status = "正在尝试 USB 自动续签。".into();
    binding.save(&receipt)?;
    let operation = tokio::time::timeout(
        std::time::Duration::from_secs(900),
        renew(&binding, &receipt, phone, manual),
    )
    .await
    .unwrap_or_else(|_| {
        Err(Failure::new(
            "timeout",
            "续签等候超时，请检查网络并保持 iPhone 解锁，稍后会重试。",
        ))
    });
    match operation {
        Ok(Some(expiry)) => {
            let extended = expiry > receipt.expires;
            receipt.expires = expiry;
            receipt.next_check = if extended { 0 } else { now + 6 * 3600 };
            receipt.status = if extended {
                "主 App 和 Pallas 手机控制均已续签。"
            } else {
                "两个 App 已重新签名并安装，Apple 返回的到期时间未变。"
            }
            .into();
        }
        Ok(None) => {
            receipt.next_check = now + 6 * 3600;
            receipt.status = "Apple 尚未签发更晚到期的描述文件，将自动重试。".into();
        }
        Err(e) => {
            receipt.status = e.message.into();
            if ["pair_changed", "login_required", "team_missing"].contains(&e.code) {
                receipt.paused = true;
            }
            if manual {
                binding.save(&receipt)?;
                return Err(e);
            }
        }
    }
    binding.save(&receipt)
}

async fn renew(
    binding: &Binding,
    receipt: &Receipt,
    mut phone: Phone,
    manual: bool,
) -> Result<Option<i64>> {
    let _phone_lock = phone.install_lock()?;
    let actual = phone.apps(&receipt.identity.ids()).await?;
    if actual != receipt.versions {
        return Err(Failure::new(
            "pair_changed",
            "手机上的 App 已移除或更新，已暂停旧包续签。请用当前版本安装器重新开启。",
        ));
    }
    phone.developer_mode(false).await?;
    let package = Package::open(&binding.current)?;
    package.check_ios(&phone.version)?;
    let mut account = Account::open(Some(&receipt.account), false, true).await?;
    let team = account.team(Some(&receipt.team))?;
    developer_image::prepare(&phone).await?;
    let work = tempfile::tempdir().safe("storage", "无法建立签名临时目录。")?;
    let pair = signing::sign(
        &package,
        work.path(),
        &phone,
        &mut account,
        &team,
        &receipt.identity,
        signing::InstallTarget::All,
        None,
    )
    .await?;
    if !manual && pair.expires <= receipt.expires {
        return Ok(None);
    }
    install_apps(&phone, &pair, &receipt.identity).await?;
    Ok(Some(pair.expires))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn installation_limit_reports_the_cause_without_device_or_account_data() {
        use idevice::installation_proxy::InstallationProxyError;
        for reason in [
            "This device has reached the maximum number of installed apps using a free developer profile: PRIVATE_APP_LIST",
            "ApplicationVerificationFailed: 0xe8008021 PRIVATE_APP_LIST",
        ] {
            let error = isideload::SideloadError::IdeviceError(
                InstallationProxyError::OperationFailed(reason.into()).into(),
            );
            let failure = installation_error(rootcause::report!(error).into(), false);
            assert_eq!(failure.code, "install_runner_limit");
            assert_eq!(failure.detail, Some(("Installation", -402620383)));
            assert!(!format!("{failure:?}").contains("PRIVATE_APP_LIST"));
        }
        let failure = installation_error(
            rootcause::report!("PRIVATE_AUTHENTICATION_CONTEXT").into(),
            true,
        );
        assert_eq!(failure.code, "install_host");
        assert!(!format!("{failure:?}").contains("PRIVATE_AUTHENTICATION_CONTEXT"));
        let error = InstallationProxyError::DeviceReported {
            name: "ApplicationVerificationFailed".into(),
            code: "-402620383".into(),
            reason: "PRIVATE_DEVICE_DESCRIPTION".into(),
        };
        let report = rootcause::report!(isideload::SideloadError::IdeviceError(error.into()));
        let failure = installation_error(report.into(), false);
        assert_eq!(failure.code, "install_runner_limit");
        assert_eq!(failure.detail, Some(("Installation", -402620383)));
        assert!(!format!("{failure:?}").contains("PRIVATE_DEVICE_DESCRIPTION"));
    }

    #[test]
    fn renewal_window_and_backoff() {
        let mut r = Receipt {
            backend: "native-v1".into(),
            device: "phone".into(),
            phone: "iPhone".into(),
            team: "ABCDE12345".into(),
            account: "account".into(),
            identity: Identity::new("ABCDE12345", false).unwrap(),
            expires: 700_000,
            versions: BTreeMap::new(),
            next_check: 0,
            paused: false,
            status: String::new(),
        };
        assert!(!due(&r, 400_000));
        assert!(due(&r, 500_000));
        r.next_check = 510_000;
        assert!(!due(&r, 500_000));
        r.next_check = 0;
        r.paused = true;
        assert!(!due(&r, 900_000));
    }
    #[test]
    fn legacy_binding_matches_python_agent() {
        let id = Identity::new("ABCDE12345", true).unwrap();
        let b = Binding::for_phone(&id, "0000-1234", "ABCDE12345").unwrap();
        assert_eq!(b.id, &state::hash(b"DSH-Go.ipa/0000-1234/ABCDE12345")[..24]);
        assert!(Binding::new("../../other").is_err());
    }
    #[test]
    fn foreground_and_agent_share_the_same_lock_file() {
        let temp = tempfile::tempdir().unwrap();
        let b = Binding {
            id: "abcdef".into(),
            base: temp.path().join("binding"),
            current: temp.path().join("current"),
            label: "test-renewal".into(),
            job: temp.path().join("job.plist"),
        };
        let guard = b.lock().unwrap();
        assert_eq!(b.lock().unwrap_err().code, "busy");
        drop(guard);
        assert!(b.lock().is_ok());
    }
    #[test]
    fn receipt_cannot_follow_a_link_outside_its_binding() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("binding");
        fs::create_dir(&base).unwrap();
        let outside = temp.path().join("kit-outside");
        fs::create_dir(&outside).unwrap();
        let current = base.join("current");
        symlink(&outside, &current).unwrap();
        let b = Binding {
            id: "abcdef".into(),
            base,
            current,
            label: "test-renewal".into(),
            job: temp.path().join("job.plist"),
        };
        assert_eq!(b.receipt().err().unwrap().code, "receipt");
    }
    #[test]
    fn cached_worker_must_start_again_after_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("kit-first");
        let second = temp.path().join("kit-second");
        for kit in [&first, &second] {
            fs::create_dir(kit).unwrap();
            fs::write(kit.join("dsh-installer-service"), b"test executable").unwrap();
        }
        let b = Binding {
            id: "abcdef".into(),
            base: temp.path().to_owned(),
            current: temp.path().join("current"),
            label: "test-renewal".into(),
            job: temp.path().join("job.plist"),
        };
        symlink(&first, &b.current).unwrap();
        assert_eq!(
            b.wait_for_start(std::time::Duration::ZERO)
                .unwrap_err()
                .code,
            "agent_start"
        );
        let worker = first.join("dsh-installer-service").canonicalize().unwrap();
        state::write_json(&first.join("worker-started.json"), &worker).unwrap();
        b.wait_for_start(std::time::Duration::ZERO).unwrap();
        fs::remove_file(&b.current).unwrap();
        symlink(&second, &b.current).unwrap();
        assert!(!b.started());
        // Even an accidentally copied old marker cannot certify the replacement.
        fs::copy(
            first.join("worker-started.json"),
            second.join("worker-started.json"),
        )
        .unwrap();
        assert_eq!(
            b.wait_for_start(std::time::Duration::ZERO)
                .unwrap_err()
                .code,
            "agent_start"
        );
    }
}

fn installation_diagnostic(component: &str, report: &rootcause::Report) -> serde_json::Value {
    // This is used only for signed-app transfer/install, never Apple authentication.
    // Do not format Report: attachments may contain unrelated private data.
    for node in report.iter_reports() {
        if let Some(isideload::SideloadError::IdeviceError(error)) =
            node.downcast_current_context::<isideload::SideloadError>()
        {
            return json!({"type":"installation_error", "component":component,
                "code":error.code(), "subcode":error.sub_code(), "reason":error.to_string()});
        }
        if let Some(error) = node.downcast_current_context::<std::io::Error>() {
            return json!({"type":"installation_error", "component":component,
                "code":error.raw_os_error(), "reason":error.to_string()});
        }
    }
    json!({"type":"installation_error", "component":component,
        "reason":"Installation failed without a typed device or IO error"})
}

#[cfg(test)]
mod installation_error_tests {
    use super::*;

    #[test]
    fn preserves_device_failure_without_report_attachments() {
        let error = idevice::installation_proxy::InstallationProxyError::DeviceReported {
            name: "ApplicationVerificationFailed".into(),
            code: "-402620395".into(),
            reason: "A valid provisioning profile was not found".into(),
        };
        let report: rootcause::Report =
            rootcause::Report::new(isideload::SideloadError::IdeviceError(error.into()))
                .attach("sentinel-private-attachment")
                .into();
        let event = installation_diagnostic("runner", &report);
        assert_eq!(event["component"], "runner");
        assert!(event["code"].is_number());
        let text = event.to_string();
        assert!(text.contains("ApplicationVerificationFailed"));
        assert!(text.contains("-402620395"));
        assert!(text.contains("valid provisioning profile"));
        assert!(!text.contains("sentinel-private-attachment"));
    }
}
