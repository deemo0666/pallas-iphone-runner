use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use zeroize::Zeroizing;

pub type Result<T> = std::result::Result<T, Failure>;

#[derive(Debug)]
pub struct Failure {
    pub code: &'static str,
    pub message: &'static str,
    pub detail: Option<(&'static str, i64)>,
}

impl Failure {
    pub const fn new(code: &'static str, message: &'static str) -> Self {
        Self {
            code,
            message,
            detail: None,
        }
    }
    pub fn detail(mut self, domain: &'static str, number: i64) -> Self {
        self.detail = Some((domain, number));
        self
    }
}

// Never format upstream authentication reports: their attachments can contain secrets.
pub trait Safe<T> {
    fn safe(self, code: &'static str, message: &'static str) -> Result<T>;
}
impl<T, E> Safe<T> for std::result::Result<T, E> {
    fn safe(self, code: &'static str, message: &'static str) -> Result<T> {
        self.map_err(|_| Failure::new(code, message))
    }
}

pub fn emit(event: Value) {
    let mut out = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut out, &event);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}
pub fn step(step: &str) {
    emit(json!({"type":"progress", "step":step}));
}
pub fn activity(message: &str) {
    emit(json!({"type":"activity", "message":message}));
}
pub fn failure(error: &Failure) {
    emit(
        json!({"type":"failure", "code":error.code, "message":error.message,
        "detail":error.detail.map(|(domain, number)| json!({"domain":domain,"number":number}))}),
    );
}

pub fn prompt(action: &str, message: &str, choices: &[String]) -> Result<Value> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    emit(json!({"type":"prompt", "id":id, "action":action, "message":message, "choices":choices}));
    let mut line = Zeroizing::new(String::new());
    // The GUI owns this private pipe. No credentials are accepted on the command line.
    let count = std::io::stdin()
        .lock()
        .take(65_537)
        .read_line(&mut line)
        .safe("input", "无法读取安装窗口的回答，请重新打开安装器。")?;
    if count == 0 || count > 65_536 {
        return Err(Failure::new("cancelled", "已停止安装。"));
    }
    decode_response(&line, id)
}

fn decode_response(line: &str, id: u64) -> Result<Value> {
    let mut value: Value =
        serde_json::from_str(line).safe("input", "安装窗口的回答无效，请重试。")?;
    if value.get("id").and_then(Value::as_u64) != Some(id) {
        return Err(Failure::new("input", "安装窗口的回答已失效，请重试。"));
    }
    Ok(value
        .get_mut("value")
        .map(Value::take)
        .unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_or_invalid_credentials_never_appear_in_errors() {
        let secret = "sentinel-password-do-not-echo";
        for line in [
            secret.to_owned(),
            format!(r#"{{"id":4,"value":{{"password":"{secret}"}}}}"#),
        ] {
            let error = decode_response(&line, 5).unwrap_err();
            assert_eq!(error.code, "input");
            assert!(!format!("{error:?}").contains(secret));
        }
        assert_eq!(
            decode_response(r#"{"id":5,"value":{"code":"123456"}}"#, 5).unwrap()["code"],
            "123456"
        );
    }
}

pub fn choose(message: &str, choices: &[String]) -> Result<usize> {
    if choices.len() == 1 {
        return Ok(0);
    }
    let answer = prompt("choice", message, choices)?;
    choices
        .iter()
        .position(|s| answer.as_str() == Some(s))
        .ok_or(Failure::new("input", "请选择列表中的一项。"))
}
