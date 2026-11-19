use crate::{
    protocol::{self, Failure, Result, Safe},
    state,
};
use fs2::FileExt;
use isideload::{
    anisette::{AnisetteClientInfo, remote_v3::RemoteV3AnisetteProvider},
    auth::{
        apple_account::{
            AppToken, AppleAccount, TwoFactorCallbackParams, TwoFactorCallbackResponse,
        },
        builder::AppleAccountBuilder,
    },
    dev::developer_session::{DeveloperSession, DeveloperTeam, TeamsApi},
    util::{keyring_storage::KeyringStorage, storage::SideloadingStorage},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zeroize::Zeroizing;

#[derive(Serialize, Deserialize, Clone)]
pub struct AccountInfo {
    pub id: String,
    pub email: String,
}

#[derive(Serialize, Deserialize)]
struct SavedSession {
    token: String,
    adsid: String,
    duration: u64,
    expiry: u64,
}

pub struct Account {
    pub info: AccountInfo,
    pub session: DeveloperSession,
    pub teams: Vec<DeveloperTeam>,
    _lock: std::fs::File,
}

fn lock(id: &str) -> Result<std::fs::File> {
    let dir = state::support()?.join("Accounts");
    state::private_dir(&dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{id}.lock")))
        .safe("account_lock", "无法锁定此 Apple 账号的签名操作。")?;
    file.try_lock_exclusive().safe(
        "account_busy",
        "此 Apple 账号正在为另一台设备签名，请稍后重试。",
    )?;
    Ok(file)
}

pub fn storage(id: &str) -> KeyringStorage {
    KeyringStorage::new(format!("io.dshlocal.installer.account.{id}"))
}

fn builder(info: &AccountInfo) -> Result<AppleAccountBuilder> {
    let provider = RemoteV3AnisetteProvider::new(
        "https://ani.iloader.app",
        Box::new(storage(&info.id)),
        "0".into(),
    )
    .safe("anisette", "无法准备 Apple 登录服务，请检查网络后重试。")?
    .set_client_info_override(Some(AnisetteClientInfo {
        client_info: "<Mac15,7> <macOS;27.0;26A5378j> <com.apple.AuthKit/1 (com.apple.akd/1.0)>"
            .into(),
        user_agent: "akd/1.0 CFNetwork/811.5.4".into(),
    }));
    Ok(AppleAccount::builder(&info.email)
        .anisette_provider(provider)
        .err_429_retries(Some(2)))
}

async fn two_factor(
    params: TwoFactorCallbackParams,
) -> std::result::Result<TwoFactorCallbackResponse, rootcause::Report> {
    let mut choices = vec!["向受信任设备发送验证码".into()];
    for number in &params.numbers {
        choices.push(format!("SMS · {}", number.last_two_digits));
    }
    let message = if params.last_error.is_some() {
        "验证码未通过，请重新输入或重新发送。"
    } else {
        "输入 Apple 验证码"
    };
    let value = match protocol::prompt("verification", message, &choices) {
        Ok(value) => value,
        Err(_) => return Ok(TwoFactorCallbackResponse::Abort),
    };
    if let Some(code) = value.get("code").and_then(Value::as_str) {
        return Ok(TwoFactorCallbackResponse::SubmitCode(
            code.trim().to_owned(),
        ));
    }
    match value.get("send").and_then(Value::as_u64) {
        Some(0) => Ok(TwoFactorCallbackResponse::SendToDevices),
        Some(i) if i as usize <= params.numbers.len() => Ok(TwoFactorCallbackResponse::SendSms(
            params.numbers[i as usize - 1].id,
        )),
        _ => Ok(TwoFactorCallbackResponse::Abort),
    }
}

pub fn accounts() -> Result<Vec<AccountInfo>> {
    let dir = state::support()?.join("Accounts");
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut list = vec![];
    for entry in std::fs::read_dir(dir)
        .safe("storage", "无法读取本机账号列表。")?
        .flatten()
    {
        if let Ok(data) = std::fs::read(entry.path())
            && let Ok(info) = serde_json::from_slice::<AccountInfo>(&data)
            && info.id == state::hash(info.email.to_lowercase().as_bytes())
        {
            list.push(info);
        }
    }
    list.sort_by(|a, b| a.email.cmp(&b.email));
    Ok(list)
}

impl Account {
    pub async fn open(wanted: Option<&str>, interactive: bool, remember: bool) -> Result<Self> {
        let infos = accounts()?;
        let info = if let Some(id) = wanted {
            infos.into_iter().find(|a| a.id == id)
        } else if infos.is_empty() {
            None
        } else {
            let mut choices: Vec<_> = infos.iter().map(|a| a.email.clone()).collect();
            choices.push("登录其他 Apple 账号".into());
            let i = protocol::choose("选择用于安装的 Apple 账号", &choices)?;
            infos.get(i).cloned()
        };
        if let Some(info) = &info {
            let account_lock = lock(&info.id)?;
            let saved = storage(&info.id).retrieve("developer_session").safe(
                "keychain",
                "无法读取钥匙串，请解锁此 Mac 的登录钥匙串后重试。",
            )?;
            if let Some(saved) = saved {
                let saved = Zeroizing::new(saved);
                if let Ok(saved) = serde_json::from_str::<SavedSession>(&saved) {
                    let account = builder(info)?
                        .build()
                        .await
                        .safe("network", "无法连接 Apple 登录服务，请检查网络。")?;
                    let mut session = DeveloperSession::new(
                        AppToken {
                            token: saved.token,
                            duration: saved.duration,
                            expiry: saved.expiry,
                        },
                        saved.adsid,
                        account.grandslam_client.clone(),
                        account.anisette_generator.clone(),
                    );
                    if let Ok(teams) = session.list_teams().await {
                        return Ok(Self {
                            info: info.clone(),
                            session,
                            teams,
                            _lock: account_lock,
                        });
                    }
                }
            }
        }
        if !interactive {
            return Err(Failure::new(
                "login_required",
                "Apple 登录已失效，请打开安装器重新登录。",
            ));
        }
        let mut credentials = protocol::prompt("login", "登录免费 Apple 账号", &[])?;
        let email = credentials
            .get_mut("email")
            .map(Value::take)
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default()
            .trim()
            .to_owned();
        let password = Zeroizing::new(
            credentials
                .get_mut("password")
                .map(Value::take)
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
        );
        if email.is_empty() || password.is_empty() {
            return Err(Failure::new("login_input", "请输入 Apple 账号和密码。"));
        }
        let info = AccountInfo {
            id: state::hash(email.to_lowercase().as_bytes()),
            email,
        };
        let account_lock = lock(&info.id)?;
        if wanted.is_some_and(|id| id != info.id) {
            return Err(Failure::new(
                "account_mismatch",
                "请使用原安装账号续签，避免生成另一份 App。",
            ));
        }
        protocol::activity("正在向 Apple 验证账号…");
        let mut account = builder(&info)?.login(&password, two_factor).await
            .safe("login_failed", "Apple 登录未完成，请检查密码、验证码和网络后重试。账号若被锁定，请先在 Apple 官网解锁。")?;
        drop(password);
        let token = account.get_app_token("xcode.auth").await.safe(
            "developer_login",
            "Apple 未返回开发者授权，请先在 Apple 开发者网站接受免费开发协议。",
        )?;
        let adsid = account
            .spd
            .as_ref()
            .and_then(|s| s.get("adsid"))
            .and_then(plist::Value::as_string)
            .ok_or(Failure::new(
                "developer_login",
                "Apple 未返回有效的账号授权。",
            ))?
            .to_owned();
        let saved = SavedSession {
            token: token.token.clone(),
            adsid: adsid.clone(),
            duration: token.duration,
            expiry: token.expiry,
        };
        let mut session = DeveloperSession::new(
            token,
            adsid,
            account.grandslam_client.clone(),
            account.anisette_generator.clone(),
        );
        let teams = session.list_teams().await.safe(
            "teams",
            "无法读取开发者团队，请先在 Apple 开发者网站接受免费开发协议后重试。",
        )?;
        if teams.is_empty() {
            return Err(Failure::new(
                "teams",
                "账号没有可用团队，请先在 Apple 开发者网站接受免费开发协议。",
            ));
        }
        if remember {
            let serialized = Zeroizing::new(
                serde_json::to_string(&saved).safe("keychain", "无法保存开发者授权。")?,
            );
            storage(&info.id)
                .store("developer_session", &serialized)
                .safe("keychain", "钥匙串未能保存授权，无法开启自动续签。")?;
            state::write_json(
                &state::support()?
                    .join("Accounts")
                    .join(format!("{}.json", info.id)),
                &info,
            )?;
        }
        Ok(Self {
            info,
            session,
            teams,
            _lock: account_lock,
        })
    }

    pub fn team(&self, wanted: Option<&str>) -> Result<DeveloperTeam> {
        if let Some(id) = wanted {
            return self
                .teams
                .iter()
                .find(|t| t.team_id == id)
                .cloned()
                .ok_or(Failure::new(
                    "team_missing",
                    "原签名团队不再可用，请打开安装器处理。",
                ));
        }
        let choices: Vec<_> = self
            .teams
            .iter()
            .enumerate()
            .map(|(i, t)| {
                format!(
                    "{} · {}",
                    t.name.as_deref().unwrap_or("Apple Account"),
                    i + 1
                )
            })
            .collect();
        let index = protocol::choose("选择签名团队", &choices)?;
        let team = self
            .teams
            .get(index)
            .cloned()
            .ok_or(Failure::new("teams", "没有可用的签名团队。"))?;
        protocol::emit(
            json!({"type":"account", "name":team.name.as_deref().unwrap_or("Apple Account")}),
        );
        Ok(team)
    }
}

pub fn logout() -> Result<()> {
    for info in accounts()? {
        let _lock = lock(&info.id)?;
        storage(&info.id)
            .delete("developer_session")
            .safe("keychain", "未能移除钥匙串授权，请重试。")?;
        // Keep the signing key so a later login can reuse its certificate instead of exhausting Apple's limit.
        let _ = std::fs::remove_file(
            state::support()?
                .join("Accounts")
                .join(format!("{}.json", info.id)),
        );
    }
    protocol::activity("已退出 Apple 账号，自动续签会等待重新登录。");
    Ok(())
}
