use crate::{
    device::Phone,
    protocol::{self, Failure, Result, Safe},
    state,
};
use idevice::{IdeviceService, lockdown::LockdownClient, mobile_image_mounter::ImageMounter};
use std::{fs, io::Write, time::Duration};

// Immutable upstream snapshot of Apple's signed image, also used by pymobiledevice3.
// iOS verifies its personalized TSS ticket; hashes additionally pin our download.
const BASE: &str = "https://raw.githubusercontent.com/doronz88/DeveloperDiskImage/e2aaeac89ca8fa56c2fa1f144d182994e07a525b/PersonalizedImages/Xcode_iOS_DDI_Personalized";
const FILES: [(&str, &str); 3] = [
    (
        "Image.dmg",
        "05fd807da5e19f030fa4941f24800c965c6c77982ab572dd5d1ef778fb69f9ca",
    ),
    (
        "Image.dmg.trustcache",
        "36af60889ff5a737874a26daeb8e1a0139ebfebec6ec2e4d8f6a3c1bf1dce35c",
    ),
    (
        "BuildManifest.plist",
        "8edd4a2f4f4ef1fbd7bfe49785d8badc673d1395d1d94d85b132ca8ab5ecaf54",
    ),
];

pub async fn mounted(phone: &Phone) -> Result<bool> {
    let mut mounter = ImageMounter::connect(&phone.provider).await.safe(
        "developer_image",
        "无法检查手机控制运行环境，请解锁 iPhone。",
    )?;
    // copy_devices also detects a DDI mounted as a cryptex on newer iOS versions.
    let devices = mounter
        .copy_devices()
        .await
        .safe("developer_image", "无法读取 iPhone 的开发者运行环境。")?;
    Ok(devices.iter().any(|v| {
        v.as_dictionary().is_some_and(|d| {
            d.get("MountPath")
                .and_then(plist::Value::as_string)
                .is_some_and(|s| s.contains("Developer"))
                || d.get("PersonalizedImageType")
                    .and_then(plist::Value::as_string)
                    == Some("DeveloperDiskImage")
        })
    }))
}

pub async fn prepare(phone: &Phone) -> Result<()> {
    if mounted(phone).await? {
        return Ok(());
    }
    protocol::activity("正在准备手机控制所需的开发者运行环境…");
    let directory = state::support()?.join("DeveloperImages/27A5228h");
    state::private_dir(&directory)?;
    let client = reqwest::Client::builder()
        .https_only(true)
        .timeout(Duration::from_secs(300))
        .build()
        .safe("download", "无法连接开发者运行环境下载服务。")?;
    let mut payloads = Vec::new();
    for (name, hash) in FILES {
        let path = directory.join(name);
        let cached = fs::read(&path)
            .ok()
            .filter(|bytes| state::hash(bytes) == hash);
        let bytes = if let Some(bytes) = cached {
            bytes
        } else {
            let response = client
                .get(format!("{BASE}/{name}"))
                .send()
                .await
                .safe("download", "下载开发者运行环境失败，请检查网络后重试。")?
                .error_for_status()
                .safe("download", "开发者运行环境下载服务暂不可用，请稍后重试。")?;
            let bytes = response
                .bytes()
                .await
                .safe("download", "开发者运行环境下载未完成。")?
                .to_vec();
            if state::hash(&bytes) != hash {
                return Err(Failure::new(
                    "download_hash",
                    "开发者运行环境校验失败，已停止安装。",
                ));
            }
            let mut file = tempfile::NamedTempFile::new_in(&directory)
                .safe("storage", "无法缓存开发者运行环境。")?;
            file.write_all(&bytes)
                .safe("storage", "无法缓存开发者运行环境。")?;
            file.persist(&path)
                .safe("storage", "无法保存开发者运行环境。")?;
            bytes
        };
        payloads.push(bytes);
    }
    let mut lockdown = LockdownClient::connect(&phone.provider)
        .await
        .safe("connection", "无法连接 iPhone。")?;
    let chip = lockdown
        .get_value(Some("UniqueChipID"), None)
        .await
        .safe("developer_image", "无法读取运行环境所需的设备信息。")?
        .as_unsigned_integer()
        .ok_or(Failure::new(
            "developer_image",
            "iPhone 未返回有效的硬件标识。",
        ))?;
    let product = lockdown
        .get_value(Some("ProductType"), None)
        .await
        .safe("developer_image", "无法读取 iPhone 型号。")?;
    let manifest: plist::Dictionary =
        plist::from_bytes(&payloads[2]).safe("developer_image", "开发者运行环境清单无效。")?;
    if !manifest
        .get("SupportedProductTypes")
        .and_then(plist::Value::as_array)
        .is_some_and(|types| types.contains(&product))
    {
        return Err(Failure::new(
            "developer_image_model",
            "此安装器的操控运行环境尚不支持这款 iPhone，请更新安装器。",
        ));
    }
    let mut mounter = ImageMounter::connect(&phone.provider)
        .await
        .safe("developer_image", "无法准备手机控制运行环境。")?;
    let manifest = payloads.pop().unwrap();
    let trustcache = payloads.pop().unwrap();
    let image = payloads.pop().unwrap();
    mounter.mount_personalized_with_callback(&phone.provider, image, trustcache, &manifest, None, chip,
        async |((done,total),())| {
            protocol::emit(serde_json::json!({"type":"transfer","component":"developerImage","percent":done.saturating_mul(100).checked_div(total).unwrap_or(0)}));
        }, ()).await.safe("developer_image_mount", "iPhone 未能启用手机控制运行环境，请保持解锁和联网后重试。")?;
    if !mounted(phone).await? {
        return Err(Failure::new(
            "developer_image_mount",
            "手机控制运行环境尚未就绪，已停止安装。",
        ));
    }
    Ok(())
}
