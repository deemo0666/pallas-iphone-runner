use crate::{
    account,
    certificate::Credential,
    device::{AppVersion, Phone},
    protocol::{self, Failure, Result, Safe},
    state,
};
use apple_codesign_quick::ProvisioningProfile;
use isideload::dev::developer_session::{
    AppGroupsApi, AppIdsApi, DeveloperSession, DeveloperTeam, DevicesApi,
};
use plist::{Dictionary, Value};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const GROUPS: &str = "com.apple.security.application-groups";
const RUNNER_IPA: &str = "ScreenRunner.ipa";
const RUNNER_HOST: &str = "RunnerHost.json";

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Identity {
    pub host: String,
    pub runner: String,
    pub group: String,
    pub scheme: String,
}
impl Identity {
    pub fn new(team: &str, legacy: bool) -> Result<Self> {
        if team.len() != 10 || !team.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(Failure::new("team_id", "Apple 返回的团队标识无效。"));
        }
        let host = format!(
            "io.{}.sideload.t{}",
            if legacy { "dshgo" } else { "dshlocal" },
            team.to_ascii_lowercase()
        );
        Ok(Self {
            runner: format!("{host}.screen.xctrunner"),
            group: format!("group.{host}.screen"),
            scheme: format!("dsh-screen-runner-{}", team.to_ascii_lowercase()),
            host,
        })
    }
    pub fn ids(&self) -> Vec<String> {
        vec![self.host.clone(), self.runner.clone()]
    }
    pub fn cache_name(&self) -> &'static str {
        if self.host.starts_with("io.dshgo.") {
            "DSH-Go.ipa"
        } else {
            "dsh-local.ipa"
        }
    }
    pub async fn resolve(phone: &Phone, team: &str) -> Result<Self> {
        let current = Self::new(team, false)?;
        let legacy = Self::new(team, true)?;
        let apps = phone.apps(&[current.ids(), legacy.ids()].concat()).await?;
        let has_current = current.ids().iter().any(|id| apps.contains_key(id));
        let has_legacy = legacy.ids().iter().any(|id| apps.contains_key(id));
        match (has_current, has_legacy) {
            (true, true) => {
                let choice = protocol::choose(
                    "手机上有两份 App，请选择要更新的一份",
                    &[
                        "Pallas Agent 安装标识".into(),
                        "旧版 DSH-Go 安装标识".into(),
                    ],
                )?;
                Ok(if choice == 0 { current } else { legacy })
            }
            (false, true) => Ok(legacy),
            _ => Ok(current),
        }
    }
}

/// A host installed from TestFlight is signed by another team than the runner, so the two cannot
/// share an App Group. They hold one key instead: the runner in its Info.plist, the host as a file
/// in its documents. The runner then serves over the loopback interface to whoever proves the key.
pub struct Pairing {
    pub host: String,
    pub key: zeroize::Zeroizing<String>,
}
impl Pairing {
    pub const KEY_FILE: &'static str = "/Documents/.screen-runner-key";
    pub const SCHEME: &'static str = "dsh-screen-runner";

    /// Some when the runner is to serve the host from TestFlight: the only host on the phone, or
    /// the one the person picks when this account has installed a host of its own as well.
    pub async fn find(
        required_host: &HostRequirement,
        phone: &Phone,
        identity: &mut Identity,
    ) -> Result<Option<Self>> {
        let own = !phone
            .apps(std::slice::from_ref(&identity.host))
            .await?
            .is_empty();
        let store = phone
            .store_host(&required_host.bundle_id, &required_host.version.version)
            .await;
        if own {
            // A host from TestFlight that cannot be paired is no reason to refuse the other one.
            if !matches!(store, Ok(true))
                || protocol::choose(
                    "手机上有两份主 App，请选择操控组件配对的一份",
                    &[
                        "此账号安装的 Pallas Agent".into(),
                        "TestFlight 版 Pallas Agent".into(),
                    ],
                )? == 0
            {
                return Ok(None);
            }
        } else if !store? {
            return Ok(None);
        }
        let mut bytes = zeroize::Zeroizing::new([0u8; 32]);
        unsafe { libc::arc4random_buf(bytes.as_mut_ptr().cast(), bytes.len()) };
        // The host from TestFlight cannot be told another scheme than the one it was built with.
        identity.scheme = Self::SCHEME.into();
        Ok(Some(Self {
            host: required_host.bundle_id.clone(),
            key: hex::encode(&bytes[..]).into(),
        }))
    }

    fn mark(&self, runner: &Path) -> Result<()> {
        let mut fields = read_info(runner)?;
        fields.insert("DSHScreenHost".into(), self.host.clone().into());
        fields.insert("DSHScreenKey".into(), self.key.as_str().into());
        plist::to_file_xml(runner.join("Info.plist"), &fields)
            .safe("plist", "无法更新 App 的安装标识。")
    }
}

#[derive(Clone, Deserialize)]
pub struct HostRequirement {
    bundle_id: String,
    #[serde(flatten)]
    pub version: AppVersion,
}

enum HostSource {
    Bundled(String),
    Installed(HostRequirement),
}

pub struct Package {
    pub root: PathBuf,
    host: HostSource,
}
impl Package {
    pub fn open(root: &Path) -> Result<Self> {
        let hosts: Vec<_> = ["dsh-local.ipa", "DSH-Go.ipa"]
            .into_iter()
            .filter(|n| root.join(n).is_file())
            .collect();
        if hosts.len() != 1 || !root.join(RUNNER_IPA).is_file() {
            return Err(Failure::new(
                "package",
                "安装包不完整，请重新解压完整安装器。",
            ));
        }
        Self::verify_files(root, &[hosts[0], RUNNER_IPA])?;
        Ok(Self {
            root: root.to_owned(),
            host: HostSource::Bundled(hosts[0].into()),
        })
    }

    pub fn open_for_install(root: &Path, target: InstallTarget) -> Result<Self> {
        if target == InstallTarget::Runner && root.join(RUNNER_HOST).is_file() {
            Self::open_runner(root)
        } else {
            Self::open(root)
        }
    }

    pub fn open_runner(root: &Path) -> Result<Self> {
        Self::verify_files(root, &[RUNNER_IPA, RUNNER_HOST])?;
        let read_error = || Failure::new("runner_host", "无法读取操控组件的主 App 兼容信息。");
        let data = fs::read(root.join(RUNNER_HOST)).map_err(|_| read_error())?;
        let host: HostRequirement = serde_json::from_slice(&data).map_err(|_| read_error())?;
        if host.bundle_id.is_empty()
            || host.version.version.is_empty()
            || host.version.build.is_empty()
        {
            return Err(read_error());
        }
        Ok(Self {
            root: root.to_owned(),
            host: HostSource::Installed(host),
        })
    }

    fn verify_files(root: &Path, files: &[&str]) -> Result<()> {
        let sums = fs::read_to_string(root.join("SHA256SUMS"))
            .safe("checksum", "缺少安装包校验信息，请重新下载安装器。")?;
        for &file in files {
            let expected = sums
                .lines()
                .find_map(|l| {
                    let (hash, name) = l.split_once(char::is_whitespace)?;
                    (name.trim().trim_start_matches('*') == file).then_some(hash)
                })
                .ok_or(Failure::new("checksum", "安装包校验信息不完整。"))?;
            let data = fs::read(root.join(file)).safe("package", "无法读取安装包。")?;
            if state::hash(&data) != expected {
                return Err(Failure::new("checksum", "安装包校验失败，请重新下载。"));
            }
        }
        Ok(())
    }

    pub fn is_runner_only(&self) -> bool {
        matches!(self.host, HostSource::Installed(_))
    }

    pub fn host_file(&self) -> Result<&str> {
        match &self.host {
            HostSource::Bundled(file) => Ok(file),
            HostSource::Installed(_) => Err(Failure::new("package", "此安装包只提供操控组件。")),
        }
    }

    pub fn host_requirement(&self) -> Result<HostRequirement> {
        match &self.host {
            HostSource::Installed(host) => Ok(host.clone()),
            HostSource::Bundled(file) => {
                let info = self.info(file)?;
                let bundle_id = info
                    .get("CFBundleIdentifier")
                    .and_then(Value::as_string)
                    .ok_or(Failure::new("package", "无法读取 App 的安装信息。"))?;
                Ok(HostRequirement {
                    bundle_id: bundle_id.into(),
                    version: app_version(&info)?,
                })
            }
        }
    }

    pub fn extract(
        &self,
        work: &Path,
        identity: &Identity,
        target: InstallTarget,
    ) -> Result<Vec<(usize, PathBuf)>> {
        let mut apps = Vec::new();
        for index in (0..2).filter(|index| target.includes(*index)) {
            let file = if index == 0 {
                self.host_file()?
            } else {
                RUNNER_IPA
            };
            let dest = work.join(index.to_string());
            state::private_dir(&dest)?;
            let status = Command::new("/usr/bin/ditto")
                .args(["-x", "-k"])
                .arg(self.root.join(file))
                .arg(&dest)
                .output()
                .safe("unpack", "无法解压安装包。")?;
            if !status.status.success() {
                return Err(Failure::new("unpack", "安装包解压失败，请重新下载。"));
            }
            let found: Vec<_> = fs::read_dir(dest.join("Payload"))
                .safe("unpack", "安装包没有 App。")?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "app"))
                .collect();
            if found.len() != 1 {
                return Err(Failure::new("unpack", "安装包的 App 结构无效。"));
            }
            let app = found[0].clone();
            // Refuse unexpected extensions rather than silently stripping functionality or consuming extra App IDs.
            for entry in walkdir::WalkDir::new(&app) {
                let entry = entry.safe("unpack", "无法检查安装包。")?;
                if entry.path().extension().is_some_and(|e| e == "appex") {
                    return Err(Failure::new(
                        "extension",
                        "此安装包包含未配置的扩展，请更新安装器。",
                    ));
                }
                if entry.file_type().is_symlink() {
                    let target =
                        fs::canonicalize(entry.path()).safe("unpack", "安装包包含无效链接。")?;
                    if !target
                        .starts_with(fs::canonicalize(&app).safe("unpack", "安装包路径无效。")?)
                    {
                        return Err(Failure::new("unpack", "安装包包含越界链接。"));
                    }
                }
            }
            rewrite(&app, identity, index == 0)?;
            // isideload uses the folder name as its AFC staging path. A fresh namespace
            // prevents files removed in a newer version surviving from an earlier upload.
            let upload = app
                .parent()
                .unwrap()
                .join(format!("DSHInstaller-{}-{index}.app", uuid::Uuid::new_v4()));
            fs::rename(&app, &upload).safe("unpack", "无法准备 App 的上传目录。")?;
            apps.push((index, upload));
        }
        Ok(apps)
    }

    pub fn check_ios(&self, product_version: &str) -> Result<()> {
        let actual = ios_version(product_version)
            .ok_or(Failure::new("ios_version", "无法读取 iPhone 的系统版本。"))?;
        for file in self.host_file().ok().into_iter().chain([RUNNER_IPA]) {
            let fields = self.info(file)?;
            let minimum = fields
                .get("MinimumOSVersion")
                .and_then(Value::as_string)
                .and_then(ios_version)
                .ok_or(Failure::new(
                    "ios_version",
                    "安装包缺少有效的最低系统版本。",
                ))?;
            if actual < minimum {
                return Err(Failure::new(
                    "ios_too_old",
                    "iPhone 系统版本低于此安装包要求，请更新系统或使用兼容的安装包。",
                ));
            }
        }
        Ok(())
    }

    /// The Info.plist of a packaged app, as built and before any identity is written into it.
    pub fn info(&self, file: &str) -> Result<Dictionary> {
        {
            let path = self.root.join(file);
            let listing = Command::new("/usr/bin/unzip")
                .args(["-Z", "-1"])
                .arg(&path)
                .output()
                .safe("package", "无法读取 App 的安装信息。")?;
            let listing_text = String::from_utf8_lossy(&listing.stdout);
            let infos: Vec<_> = listing_text
                .lines()
                .filter(|name| {
                    name.starts_with("Payload/")
                        && name.ends_with(".app/Info.plist")
                        && name.matches('/').count() == 2
                })
                .collect();
            if !listing.status.success() || infos.len() != 1 {
                return Err(Failure::new("package", "安装包的 App 结构无效。"));
            }
            let info = Command::new("/usr/bin/unzip")
                .arg("-p")
                .arg(&path)
                .arg(infos[0])
                .output()
                .safe("package", "无法读取 App 的安装信息。")?;
            if !info.status.success() {
                return Err(Failure::new("package", "无法读取 App 的安装信息。"));
            }
            plist::from_bytes(&info.stdout).safe("package", "无法读取 App 的安装信息。")
        }
    }
}

fn ios_version(value: &str) -> Option<[u64; 3]> {
    let mut version = [0; 3];
    for (index, component) in value.split('.').enumerate() {
        *version.get_mut(index)? = component.parse().ok()?;
    }
    Some(version)
}

fn read_info(path: &Path) -> Result<Dictionary> {
    plist::from_file(path.join("Info.plist")).safe("plist", "无法读取 App 的安装信息。")
}
pub fn version(path: &Path) -> Result<AppVersion> {
    app_version(&read_info(path)?)
}

fn app_version(fields: &Dictionary) -> Result<AppVersion> {
    Ok(AppVersion {
        version: fields
            .get("CFBundleShortVersionString")
            .and_then(Value::as_string)
            .ok_or(Failure::new("version", "App 缺少版本号。"))?
            .into(),
        build: fields
            .get("CFBundleVersion")
            .and_then(Value::as_string)
            .ok_or(Failure::new("version", "App 缺少构建号。"))?
            .into(),
    })
}

pub fn rewrite(app: &Path, identity: &Identity, host: bool) -> Result<()> {
    let bundle_id = if host {
        &identity.host
    } else {
        &identity.runner
    };
    let mut paths = vec![app.to_owned()];
    for entry in walkdir::WalkDir::new(app).into_iter() {
        let entry = entry.safe("package", "无法检查 App 组件。")?;
        if entry.file_type().is_dir() && entry.path().extension().is_some_and(|e| e == "xctest") {
            paths.push(entry.path().to_owned());
        }
    }
    for path in paths {
        let mut fields = read_info(&path)?;
        let original = fields
            .get("CFBundleIdentifier")
            .and_then(Value::as_string)
            .unwrap_or("")
            .to_owned();
        fields.insert("CFBundleIdentifier".into(), bundle_id.clone().into());
        if path == app && host {
            fields.insert(
                "DSHScreenRunnerScheme".into(),
                identity.scheme.clone().into(),
            );
            if let Some(Value::Array(tasks)) = fields.get_mut("BGTaskSchedulerPermittedIdentifiers")
            {
                for item in tasks {
                    if item.as_string() == Some(&format!("{original}.phone.control")) {
                        *item = format!("{bundle_id}.phone.control").into();
                    }
                }
            }
            if !fields.contains_key("LSApplicationQueriesSchemes") {
                fields.insert("LSApplicationQueriesSchemes".into(), Value::Array(vec![]));
            }
            if let Some(Value::Array(queries)) = fields.get_mut("LSApplicationQueriesSchemes") {
                queries.retain(|q| {
                    !q.as_string()
                        .is_some_and(|s| s.starts_with("dsh-screen-runner"))
                });
                queries.push(identity.scheme.clone().into());
            }
        } else if path == app {
            fields.insert("CFBundleDisplayName".into(), "Pallas 手机控制".into());
            fields.insert("CFBundleName".into(), "Pallas 手机控制".into());
            let mut url = Dictionary::new();
            url.insert(
                "CFBundleURLSchemes".into(),
                Value::Array(vec![identity.scheme.clone().into()]),
            );
            fields.insert(
                "CFBundleURLTypes".into(),
                Value::Array(vec![Value::Dictionary(url)]),
            );
        }
        plist::to_file_xml(path.join("Info.plist"), &fields)
            .safe("plist", "无法更新 App 的安装标识。")?;
    }
    Ok(())
}

pub struct SignedApps {
    pub apps: Vec<(String, PathBuf)>,
    pub expires: i64,
    pub versions: BTreeMap<String, AppVersion>,
}

pub fn validate_entitlements(
    ent: &Dictionary,
    team: &str,
    bundle: &str,
    group: &str,
) -> Result<()> {
    let application = format!("{team}.{bundle}");
    if ent.get("application-identifier").and_then(Value::as_string) != Some(&application)
        || ent
            .get("com.apple.developer.team-identifier")
            .and_then(Value::as_string)
            != Some(team)
        || !ent
            .get(GROUPS)
            .and_then(Value::as_array)
            .is_some_and(|g| g.iter().any(|v| v.as_string() == Some(group)))
    {
        return Err(Failure::new(
            "profile_mismatch",
            "Apple 返回的共享权限或安装标识不匹配，尚未安装任何 App。",
        ));
    }
    if ent.get("get-task-allow").and_then(Value::as_boolean) != Some(true) {
        return Err(Failure::new(
            "profile_debug",
            "签名未包含手机控制所需的开发权限。",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallTarget {
    All,
    Host,
    Runner,
}

impl InstallTarget {
    fn includes(self, index: usize) -> bool {
        matches!(
            (self, index),
            (Self::All, 0 | 1) | (Self::Host, 0) | (Self::Runner, 1)
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn sign(
    package: &Package,
    work: &Path,
    phone: &Phone,
    account: &mut account::Account,
    team: &DeveloperTeam,
    identity: &Identity,
    target: InstallTarget,
    pairing: Option<&Pairing>,
) -> Result<SignedApps> {
    let apps = package.extract(work, identity, target)?;
    let session: &mut DeveloperSession = &mut account.session;
    protocol::activity(match target {
        InstallTarget::Runner => "正在准备 Pallas 手机控制签名…",
        InstallTarget::Host => "正在准备主 App 签名…",
        InstallTarget::All => "正在向 Apple 注册设备和两个 App…",
    });
    session
        .ensure_device_registered(team, &phone.name, &phone.udid, None)
        .await
        .safe(
            "device_registration",
            "Apple 未能登记此 iPhone，请检查网络和账号的设备名额。",
        )?;
    let certificate = Credential::retrieve(&account.info, session, team).await?;
    certificate.report_selected()?;
    let group = session
        .ensure_app_group(team, "Pallas Agent Screen", &identity.group, None)
        .await
        .safe(
            "app_group",
            "Apple 未能开通两个 App 的共享组，手机控制尚不可用。请检查账号和网络后重试。",
        )?;
    if group.identifier != identity.group {
        return Err(Failure::new(
            "app_group",
            "Apple 返回了不同的共享组，已停止安装。",
        ));
    }
    let registered = session
        .list_app_ids(team, None)
        .await
        .safe("app_ids", "无法读取 Apple 的 App 名额。")?;
    let mut profiles = Vec::new();
    for (id, name) in [
        (&identity.host, "Pallas Agent"),
        (&identity.runner, "Pallas Phone Control"),
    ]
    .into_iter()
    .enumerate()
    .filter(|(index, _)| target.includes(*index))
    .map(|(_, app)| app)
    {
        let mut app_id =
            if let Some(existing) = registered.app_ids.iter().find(|a| &a.identifier == id) {
                existing.clone()
            } else {
                session.add_app_id(team, name, id, None).await.safe(
                    "app_limit",
                    "Apple 未能注册 App。免费账号的 App ID 名额可能已满，请稍后重试。",
                )?
            };
        if app_id
            .features
            .get("APG3427HIY")
            .and_then(Value::as_boolean)
            != Some(true)
        {
            let mut features = Dictionary::new();
            features.insert("APG3427HIY".into(), true.into());
            app_id = session
                .update_app_id(team, &app_id, features, None)
                .await
                .safe("app_group", "Apple 未能启用 App 的共享权限。")?;
        }
        session
            .assign_app_group(team, &group, &app_id, None)
            .await
            .safe("app_group", "Apple 未能把两个 App 加入同一个共享组。")?;
        let profile = session
            .download_team_provisioning_profile(team, &app_id, None)
            .await
            .safe("profile", "Apple 未能签发描述文件，请稍后重试。")?;
        let parsed = ProvisioningProfile::parse(profile.encoded_profile.as_ref())
            .safe("profile", "Apple 返回的描述文件无效。")?;
        validate_entitlements(parsed.entitlements(), &team.team_id, id, &identity.group)?;
        certificate.validate_profile(&parsed)?;
        if !parsed
            .plist()
            .get("ProvisionedDevices")
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().any(|v| v.as_string() == Some(&phone.udid)))
        {
            return Err(Failure::new(
                "profile_device",
                "签名没有包含这台 iPhone，尚未安装任何 App。",
            ));
        }
        profiles.push((profile, parsed));
    }
    let mut signed = Vec::new();
    let mut expires = certificate.expires();
    let mut versions = BTreeMap::new();
    for ((index, app), (profile, parsed)) in apps.into_iter().zip(&profiles) {
        if index == 1
            && let Some(pairing) = pairing
        {
            pairing.mark(&app)?;
        }
        protocol::activity(if index == 0 {
            "正在签名主 App"
        } else {
            "正在签名 Pallas 手机控制"
        });
        let date = parsed
            .plist()
            .get("ExpirationDate")
            .and_then(Value::as_date)
            .ok_or(Failure::new("expiry", "描述文件缺少有效期。"))?;
        let time: std::time::SystemTime = date.into();
        let expiry = time
            .duration_since(std::time::UNIX_EPOCH)
            .safe("expiry", "描述文件有效期无效。")?
            .as_secs() as i64;
        expires = expires.min(expiry);
        certificate.sign(
            &app,
            &team.team_id,
            parsed,
            profile.encoded_profile.as_ref(),
        )?;
        verify_signature(&app)?;
        let id = identity.ids()[index].clone();
        versions.insert(id.clone(), version(&app)?);
        signed.push((id, app));
    }
    if expires <= chrono::Utc::now().timestamp() {
        return Err(Failure::new(
            "expiry",
            "Apple 返回了已过期的签名，请稍后重试。",
        ));
    }
    Ok(SignedApps {
        apps: signed,
        expires,
        versions,
    })
}

pub fn verify_signature(app: &Path) -> Result<()> {
    let result = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app)
        .output()
        .safe("signature", "无法检查 App 签名。")?;
    if !result.status.success() {
        return Err(Failure::new(
            "signature",
            "App 签名完整性检查失败，尚未安装任何 App。",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ios_versions_compare_numerically_with_missing_patch_components() {
        assert_eq!(ios_version("17"), ios_version("17.0.0"));
        assert!(ios_version("17.10").unwrap() > ios_version("17.9.1").unwrap());
        assert!(ios_version("unknown").is_none());
    }
    #[test]
    fn shared_identity_preserves_old_install() {
        let id = Identity::new("ABCDE12345", true).unwrap();
        assert_eq!(id.host, "io.dshgo.sideload.tabcde12345");
        assert_eq!(id.group, "group.io.dshgo.sideload.tabcde12345.screen");
        assert_eq!(id.runner, format!("{}.screen.xctrunner", id.host));
        assert!(Identity::new("../../bad", false).is_err());
    }
    #[test]
    fn wrong_shared_group_is_a_hard_error() {
        let mut ent = Dictionary::new();
        ent.insert(
            "application-identifier".into(),
            "ABCDE12345.io.example.app".into(),
        );
        ent.insert(
            "com.apple.developer.team-identifier".into(),
            "ABCDE12345".into(),
        );
        ent.insert("get-task-allow".into(), true.into());
        ent.insert(GROUPS.into(), Value::Array(vec!["group.other".into()]));
        assert!(
            validate_entitlements(&ent, "ABCDE12345", "io.example.app", "group.expected").is_err()
        );
        ent.insert(GROUPS.into(), Value::Array(vec!["group.expected".into()]));
        assert!(
            validate_entitlements(&ent, "ABCDE12345", "io.example.app", "group.expected").is_ok()
        );
    }
    #[test]
    fn rewriting_keeps_version_and_updates_cross_app_links() {
        let temp = tempfile::tempdir().unwrap();
        let app = temp.path().join("Host.app");
        fs::create_dir(&app).unwrap();
        let mut info = Dictionary::new();
        info.insert("CFBundleIdentifier".into(), "io.previous.app".into());
        info.insert("CFBundleShortVersionString".into(), "3.7".into());
        info.insert("CFBundleVersion".into(), "81".into());
        info.insert(
            "BGTaskSchedulerPermittedIdentifiers".into(),
            Value::Array(vec![
                "io.previous.app.phone.control".into(),
                "unrelated.task".into(),
            ]),
        );
        info.insert(
            "LSApplicationQueriesSchemes".into(),
            Value::Array(vec!["dsh-screen-runner".into(), "another-app".into()]),
        );
        plist::to_file_xml(app.join("Info.plist"), &info).unwrap();
        let identity = Identity::new("ABCDE12345", true).unwrap();
        rewrite(&app, &identity, true).unwrap();
        let updated = read_info(&app).unwrap();
        assert_eq!(
            version(&app).unwrap(),
            AppVersion {
                version: "3.7".into(),
                build: "81".into()
            }
        );
        assert_eq!(
            updated["DSHScreenRunnerScheme"].as_string(),
            Some(identity.scheme.as_str())
        );
        assert_eq!(
            updated["BGTaskSchedulerPermittedIdentifiers"]
                .as_array()
                .unwrap(),
            &vec![
                Value::String(format!("{}.phone.control", identity.host)),
                "unrelated.task".into()
            ]
        );
        assert_eq!(
            updated["LSApplicationQueriesSchemes"].as_array().unwrap(),
            &vec![Value::String("another-app".into()), identity.scheme.into()]
        );
    }
}

#[cfg(test)]
mod install_target_tests {
    use super::*;

    #[test]
    fn runner_only_kit_verifies_the_ipa_and_installed_host_requirement() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let runner = b"runner fixture";
        let host = br#"{"bundle_id":"io.example.app","version":"0.5.1","build":"18"}"#;
        fs::write(root.join(RUNNER_IPA), runner).unwrap();
        fs::write(root.join(RUNNER_HOST), host).unwrap();
        fs::write(
            root.join("SHA256SUMS"),
            format!(
                "{}  {RUNNER_IPA}\n{}  {RUNNER_HOST}\n",
                state::hash(runner),
                state::hash(host)
            ),
        )
        .unwrap();

        let package = Package::open_for_install(root, InstallTarget::Runner).unwrap();
        assert!(package.is_runner_only());
        let required = package.host_requirement().unwrap();
        assert_eq!(required.bundle_id, "io.example.app");
        assert_eq!(
            required.version,
            AppVersion {
                version: "0.5.1".into(),
                build: "18".into()
            }
        );
        assert!(Package::open_for_install(root, InstallTarget::All).is_err());
        assert!(Package::open_for_install(root, InstallTarget::Host).is_err());

        fs::write(root.join(RUNNER_HOST), b"{}").unwrap();
        assert_eq!(Package::open_runner(root).err().unwrap().code, "checksum");
    }

    #[test]
    fn single_install_selects_only_requested_app_and_profile() {
        for (target, expected) in [
            (InstallTarget::All, vec![0, 1]),
            (InstallTarget::Host, vec![0]),
            (InstallTarget::Runner, vec![1]),
        ] {
            assert_eq!(
                (0..3)
                    .filter(|index| target.includes(*index))
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }
    #[test]
    fn runner_for_a_store_host_carries_its_bundle_id_and_key() {
        let temp = tempfile::tempdir().unwrap();
        let app = temp.path().join("Runner.app");
        fs::create_dir(&app).unwrap();
        let mut info = Dictionary::new();
        info.insert("CFBundleIdentifier".into(), "old.runner".into());
        plist::to_file_xml(app.join("Info.plist"), &info).unwrap();
        let mut identity = Identity::new("TEST123456", false).unwrap();
        identity.scheme = Pairing::SCHEME.into();
        rewrite(&app, &identity, false).unwrap();
        let pairing = Pairing {
            host: "io.example.store".into(),
            key: "5a".repeat(32).into(),
        };
        pairing.mark(&app).unwrap();
        let marked = read_info(&app).unwrap();
        assert_eq!(
            marked["DSHScreenHost"].as_string(),
            Some("io.example.store")
        );
        assert_eq!(
            marked["DSHScreenKey"].as_string(),
            Some("5a".repeat(32).as_str())
        );
        assert_eq!(
            marked["CFBundleIdentifier"].as_string(),
            Some(identity.runner.as_str())
        );
        assert_eq!(
            marked["CFBundleURLTypes"].as_array().unwrap()[0]
                .as_dictionary()
                .unwrap()["CFBundleURLSchemes"]
                .as_array()
                .unwrap()[0]
                .as_string(),
            Some("dsh-screen-runner")
        );
    }
    #[test]
    fn runner_display_name_changes_without_changing_pairing_identity() {
        let temp = tempfile::tempdir().unwrap();
        let app = temp.path().join("Runner.app");
        fs::create_dir(&app).unwrap();
        let mut info = Dictionary::new();
        info.insert("CFBundleIdentifier".into(), "old.runner".into());
        info.insert("CFBundleDisplayName".into(), "DSH 手机操控".into());
        plist::to_file_xml(app.join("Info.plist"), &info).unwrap();
        let identity = Identity::new("TEST123456", false).unwrap();
        rewrite(&app, &identity, false).unwrap();
        let rewritten = read_info(&app).unwrap();
        assert_eq!(
            rewritten["CFBundleDisplayName"].as_string(),
            Some("Pallas 手机控制")
        );
        assert_eq!(
            rewritten["CFBundleIdentifier"].as_string(),
            Some(identity.runner.as_str())
        );
        assert_eq!(
            rewritten["CFBundleURLTypes"].as_array().unwrap()[0]
                .as_dictionary()
                .unwrap()["CFBundleURLSchemes"]
                .as_array()
                .unwrap()[0]
                .as_string(),
            Some(identity.scheme.as_str())
        );
    }
}
