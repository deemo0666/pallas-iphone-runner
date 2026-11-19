use crate::protocol::{Failure, Result, Safe};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

pub fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
pub fn support() -> Result<PathBuf> {
    let home =
        std::env::var_os("HOME").ok_or(Failure::new("home", "无法读取此 Mac 的用户目录。"))?;
    Ok(PathBuf::from(home).join("Library/Application Support/DSH Sideload"))
}
pub fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).safe("storage", "无法创建本机安装缓存。")?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .safe("storage", "无法保护本机安装缓存。")
}
pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or(Failure::new("storage", "本机缓存路径无效。"))?;
    private_dir(parent)?;
    let mut temp =
        tempfile::NamedTempFile::new_in(parent).safe("storage", "无法保存本机安装状态。")?;
    serde_json::to_writer(&mut temp, value).safe("storage", "无法保存本机安装状态。")?;
    temp.flush().safe("storage", "无法保存本机安装状态。")?;
    temp.persist(path)
        .safe("storage", "无法保存本机安装状态。")?;
    Ok(())
}
