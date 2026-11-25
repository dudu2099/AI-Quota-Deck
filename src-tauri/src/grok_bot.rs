//! Grok Bot quota, read with the short-lived access token the signed-in desktop
//! app keeps in its Chromium profile. The refresh token is deliberately never
//! deserialised or decrypted: Grok Bot remains the sole owner of renewal.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};

use crate::claude::provider_cache_dir;
use crate::quota::{http_failure, now, ProviderQuota, QuotaWindow, Stale};

const PROVIDER: &str = "grok_bot";
const USAGE_URL: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetSandUsageStatus";
/// Grok Bot's per-day usage report. It is the only place its tokens are
/// counted: the app itself never sees a token count, so nothing on this
/// machine records one.
const DAILY_SPEND_URL: &str =
    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetDailySpendByCategory";
// Enum numbers from Grok Bot's own bundle: SpendGroupByCategory.MODEL and
// SpendType.ALL. The request mirrors the one Grok Bot sends for its usage view.
const GROUP_BY_MODEL: i32 = 1;
const SPEND_TYPE_ALL: i32 = 3;
const REQUEST_TIMEOUT_SECONDS: u64 = 10;
const SNAPSHOT_MAX_AGE_SECONDS: i64 = 24 * 60 * 60;
const FALLBACK_CLIENT_VERSION: &str = "0.29.0";

static FETCH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Deserialize)]
struct LocalState {
    os_crypt: OsCrypt,
}

#[derive(Deserialize)]
struct OsCrypt {
    encrypted_key: String,
}

#[derive(Deserialize)]
struct Secrets {
    #[serde(rename = "cursor-machine-id")]
    machine_id: String,
    #[serde(rename = "cursor-accounts")]
    accounts: String,
}

#[derive(Deserialize)]
struct Accounts {
    active: String,
    accounts: HashMap<String, Account>,
}

#[derive(Deserialize)]
struct Account {
    #[serde(rename = "cursor-access-token")]
    access_token: String,
    // Intentionally no refresh-token field. Unknown fields are discarded.
}

#[derive(Deserialize)]
struct JwtClaims {
    exp: Option<i64>,
}

struct Credentials {
    access_token: String,
    machine_id: String,
}

#[derive(Default, Debug, PartialEq)]
struct UsageStatus {
    current_period_start: Option<i64>,
    next_reset: Option<i64>,
    usage_percent: Option<f64>,
    included_limit_zero: Option<bool>,
    uses_pooled_enterprise_allowance: Option<bool>,
    has_available_usage: Option<bool>,
    has_non_zero_included_limit: Option<bool>,
    included_plan: Option<String>,
    plan_label: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct CachedUsage {
    plan: Option<String>,
    windows: Vec<QuotaWindow>,
    observed_at: i64,
}

fn profile_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("Grok Bot"))
}

fn snapshot_path() -> Option<PathBuf> {
    provider_cache_dir().map(|dir| dir.join("grok-bot.json"))
}

fn snapshot_exists() -> bool {
    snapshot_path().is_some_and(|path| matches!(path.try_exists(), Ok(true)))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path, subject: &str) -> Result<T, String> {
    let raw =
        std::fs::read_to_string(path).map_err(|error| format!("无法读取 {subject}：{error}"))?;
    serde_json::from_str(&raw).map_err(|error| format!("无法解析 {subject}：{error}"))
}

#[cfg(target_os = "windows")]
fn dpapi_unprotect(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes
            .len()
            .try_into()
            .map_err(|_| "加密密钥过大")?,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let succeeded = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if succeeded == 0 || output.pbData.is_null() {
        return Err(format!(
            "Windows 无法解锁 Grok Bot 配置文件（{}）",
            std::io::Error::last_os_error()
        ));
    }
    let clear = unsafe {
        let value = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(output.pbData.cast());
        value
    };
    Ok(clear)
}

#[cfg(not(target_os = "windows"))]
fn dpapi_unprotect(_bytes: &[u8]) -> Result<Vec<u8>, String> {
    Err("只有 Windows 支持解密 Grok Bot 配置文件".to_string())
}

fn master_key(state: &LocalState) -> Result<Vec<u8>, String> {
    let wrapped = general_purpose::STANDARD
        .decode(&state.os_crypt.encrypted_key)
        .map_err(|error| format!("Grok Bot 的加密密钥不是有效的 base64：{error}"))?;
    let payload = wrapped
        .strip_prefix(b"DPAPI")
        .ok_or("Grok Bot 的加密密钥格式未知")?;
    let key = dpapi_unprotect(payload)?;
    if key.len() != 32 {
        return Err("Grok Bot 解锁后的配置密钥长度不对".to_string());
    }
    Ok(key)
}

fn decrypt_v10(encoded: &str, key: &[u8]) -> Result<Vec<u8>, String> {
    let blob = general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("Grok Bot 的加密数据不是有效的 base64：{error}"))?;
    if !blob.starts_with(b"v10") || blob.len() < 3 + 12 + 16 {
        return Err("Grok Bot 的加密数据格式未知".to_string());
    }
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|_| "Grok Bot 的配置密钥长度不对".to_string())?;
    cipher
        .decrypt(Nonce::from_slice(&blob[3..15]), &blob[15..])
        .map_err(|_| "Grok Bot 配置文件校验失败".to_string())
}

fn decrypt_string(encoded: &str, key: &[u8], subject: &str) -> Result<String, String> {
    String::from_utf8(decrypt_v10(encoded, key)?)
        .map_err(|_| format!("解密后的 {subject}不是文本"))
}

fn jwt_exp(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice::<JwtClaims>(&bytes).ok()?.exp
}

fn load_credentials_from(profile: &Path) -> Result<Credentials, String> {
    let state: LocalState = read_json(&profile.join("Local State"), "Grok Bot 的 Local State 文件")?;
    let secrets: Secrets = read_json(
        &profile.join("sand-secrets.json"),
        "Grok Bot 的本地登录记录",
    )?;
    let key = master_key(&state)?;
    let accounts: Accounts = serde_json::from_str(&secrets.accounts)
        .map_err(|error| format!("无法解析 Grok Bot 的账号列表：{error}"))?;
    let active = accounts
        .accounts
        .get(&accounts.active)
        .ok_or("找不到 Grok Bot 当前登录的账号")?;
    let access_token = decrypt_string(&active.access_token, &key, "Grok Bot 访问令牌")?;
    let machine_id = decrypt_string(&secrets.machine_id, &key, "Grok Bot 机器 ID")?;
    if access_token.trim().is_empty() || machine_id.trim().is_empty() {
        return Err("Grok Bot 的本地登录记录不完整".to_string());
    }
    Ok(Credentials {
        access_token,
        machine_id,
    })
}

fn checksum_at(machine_id: &str, epoch_millis: i64) -> String {
    // This reproduces Grok Bot's six JavaScript signed-shift bytes exactly.
    let bucket = epoch_millis.div_euclid(1_000_000) as u32;
    let mut bytes = [
        (bucket >> 8) as u8,
        bucket as u8,
        (bucket >> 24) as u8,
        (bucket >> 16) as u8,
        (bucket >> 8) as u8,
        bucket as u8,
    ];
    let mut state = 165_u8;
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = (*byte ^ state).wrapping_add(index as u8);
        state = *byte;
    }
    format!(
        "{}{}",
        general_purpose::URL_SAFE_NO_PAD.encode(bytes),
        machine_id
    )
}

fn read_varint(bytes: &[u8], cursor: &mut usize) -> Result<u64, String> {
    let mut value = 0_u64;
    for shift in (0..70).step_by(7) {
        let byte = *bytes.get(*cursor).ok_or("protobuf varint 被截断")?;
        *cursor += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("protobuf varint 过长".to_string())
}

fn length_delimited<'a>(bytes: &'a [u8], cursor: &mut usize) -> Result<&'a [u8], String> {
    let length: usize = read_varint(bytes, cursor)?
        .try_into()
        .map_err(|_| "protobuf 字段过大")?;
    let end = cursor
        .checked_add(length)
        .ok_or("protobuf 字段长度溢出")?;
    let field = bytes.get(*cursor..end).ok_or("protobuf 字段被截断")?;
    *cursor = end;
    Ok(field)
}

fn skip_field(bytes: &[u8], cursor: &mut usize, wire: u64) -> Result<(), String> {
    match wire {
        0 => {
            read_varint(bytes, cursor)?;
        }
        1 => *cursor = cursor.checked_add(8).ok_or("protobuf 字段溢出")?,
        2 => {
            length_delimited(bytes, cursor)?;
        }
        5 => *cursor = cursor.checked_add(4).ok_or("protobuf 字段溢出")?,
        _ => return Err(format!("不支持的 protobuf 线路类型 {wire}")),
    }
    if *cursor > bytes.len() {
        return Err("protobuf 字段被截断".to_string());
    }
    Ok(())
}

fn timestamp_seconds(bytes: &[u8]) -> Result<i64, String> {
    let mut cursor = 0;
    let mut seconds = None;
    while cursor < bytes.len() {
        let key = read_varint(bytes, &mut cursor)?;
        let (field, wire) = (key >> 3, key & 7);
        if field == 1 && wire == 0 {
            seconds = Some(read_varint(bytes, &mut cursor)? as i64);
        } else {
            skip_field(bytes, &mut cursor, wire)?;
        }
    }
    seconds.ok_or("protobuf 时间戳缺少秒数".to_string())
}

fn optional_text(bytes: &[u8]) -> Option<String> {
    String::from_utf8(bytes.to_vec())
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn parse_usage_status(bytes: &[u8]) -> Result<UsageStatus, String> {
    let mut result = UsageStatus::default();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let key = read_varint(bytes, &mut cursor)?;
        let (field, wire) = (key >> 3, key & 7);
        match (field, wire) {
            (1, 2) => {
                result.current_period_start =
                    Some(timestamp_seconds(length_delimited(bytes, &mut cursor)?)?)
            }
            (2, 2) => {
                result.next_reset = Some(timestamp_seconds(length_delimited(bytes, &mut cursor)?)?)
            }
            (3, 1) => {
                let raw: [u8; 8] = bytes
                    .get(cursor..cursor + 8)
                    .ok_or("用量百分比被截断")?
                    .try_into()
                    .map_err(|_| "用量百分比无效")?;
                result.usage_percent = Some(f64::from_le_bytes(raw));
                cursor += 8;
            }
            (4, 0) => result.included_limit_zero = Some(read_varint(bytes, &mut cursor)? != 0),
            (6, 0) => {
                result.uses_pooled_enterprise_allowance =
                    Some(read_varint(bytes, &mut cursor)? != 0)
            }
            (7, 0) => result.has_available_usage = Some(read_varint(bytes, &mut cursor)? != 0),
            (8, 0) => {
                result.has_non_zero_included_limit = Some(read_varint(bytes, &mut cursor)? != 0)
            }
            (14, 2) => result.included_plan = optional_text(length_delimited(bytes, &mut cursor)?),
            (15, 2) => result.plan_label = optional_text(length_delimited(bytes, &mut cursor)?),
            _ => skip_field(bytes, &mut cursor, wire)?,
        }
    }
    Ok(result)
}

fn humanize_plan(value: &str) -> String {
    value
        .split(['_', '-'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        })
        .collect::<Vec<String>>()
        .join(" ")
}

fn quota_from(status: UsageStatus) -> ProviderQuota {
    if status.uses_pooled_enterprise_allowance == Some(true) {
        return ProviderQuota::unavailable(
            PROVIDER,
            "Grok Bot 用的是企业共享额度，不提供个人用量百分比。",
        );
    }
    if status.included_limit_zero == Some(true)
        || (status.has_non_zero_included_limit == Some(false) && status.usage_percent.is_none())
    {
        return ProviderQuota::unavailable(
            PROVIDER,
            "已登录 Grok Bot，但这个账号没有包含的用量限额。",
        );
    }
    let Some(percent) = status
        .usage_percent
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
    else {
        return ProviderQuota::unavailable(
            PROVIDER,
            "Grok Bot 没有提供个人的每周用量百分比。",
        );
    };
    let window_seconds = status
        .current_period_start
        .zip(status.next_reset)
        .and_then(|(start, reset)| (reset > start).then_some(reset - start));
    let plan = status
        .plan_label
        .or_else(|| status.included_plan.as_deref().map(humanize_plan));
    ProviderQuota::ok(
        PROVIDER,
        plan,
        vec![QuotaWindow {
            label: "Weekly".to_string(),
            percent,
            resets_at: status.next_reset,
            window_seconds,
            severity: None,
        }],
    )
}

fn write_snapshot(plan: &Option<String>, windows: &[QuotaWindow]) -> Result<(), String> {
    let path = snapshot_path().ok_or("could not locate the local application data directory")?;
    let parent = path
        .parent()
        .ok_or("Grok Bot cache path has no parent directory")?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let bytes = serde_json::to_vec(&CachedUsage {
        plan: plan.clone(),
        windows: windows.to_vec(),
        observed_at: now(),
    })
    .map_err(|error| format!("cannot serialize Grok Bot cache: {error}"))?;
    let staging = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&staging, bytes)
        .map_err(|error| format!("cannot write {}: {error}", staging.display()))?;
    std::fs::rename(&staging, &path).map_err(|error| {
        let _ = std::fs::remove_file(&staging);
        format!("cannot replace {}: {error}", path.display())
    })
}

fn cached_quota_from_raw(raw: &str, current_time: i64, reason: &str) -> Option<ProviderQuota> {
    let mut snapshot: CachedUsage = serde_json::from_str(raw).ok()?;
    let age = current_time.checked_sub(snapshot.observed_at)?;
    if snapshot.observed_at <= 0 || !(0..=SNAPSHOT_MAX_AGE_SECONDS).contains(&age) {
        return None;
    }
    snapshot
        .windows
        .retain(|window| window.resets_at.is_none_or(|reset| reset > current_time));
    if snapshot.windows.is_empty() {
        return None;
    }
    Some(ProviderQuota::ok_stale(
        PROVIDER,
        snapshot.plan,
        snapshot.windows,
        Stale {
            source: "上次的 Grok Bot 查询".to_string(),
            observed_at: Some(snapshot.observed_at),
            reason: reason.to_string(),
        },
    ))
}

fn cached_quota(reason: &str) -> Option<ProviderQuota> {
    let raw = std::fs::read_to_string(snapshot_path()?).ok()?;
    cached_quota_from_raw(&raw, now(), reason)
}

fn fallback(reason: &str) -> ProviderQuota {
    cached_quota(reason).unwrap_or_else(|| ProviderQuota::error(PROVIDER, reason))
}

fn action_required(reason: &str) -> ProviderQuota {
    cached_quota(reason).unwrap_or_else(|| {
        ProviderQuota::action_required(
            PROVIDER,
            "打开 Grok Bot 并重新登录，让它更新访问令牌。",
        )
    })
}

pub async fn fetch() -> ProviderQuota {
    let _guard = FETCH_LOCK.lock().await;
    let Some(profile) = profile_dir() else {
        return ProviderQuota::not_configured(PROVIDER, "安装 Grok Bot 并登录一次。");
    };
    if !profile.join("sand-secrets.json").is_file() || !profile.join("Local State").is_file() {
        return if snapshot_exists() {
            action_required("Grok Bot 的本地登录记录不见了。")
        } else {
            ProviderQuota::not_configured(PROVIDER, "安装 Grok Bot 并登录一次。")
        };
    }

    let credentials =
        match tauri::async_runtime::spawn_blocking(move || load_credentials_from(&profile)).await {
            Ok(Ok(credentials)) => credentials,
            Ok(Err(error)) => return action_required(&error),
            Err(error) => {
                return fallback(&format!(
                    "无法读取 Grok Bot 的登录记录：{error}"
                ))
            }
        };
    if jwt_exp(&credentials.access_token).is_some_and(|expiry| expiry <= now()) {
        return action_required("Grok Bot 的访问令牌已过期。");
    }

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECONDS))
        .build()
    {
        Ok(client) => client,
        Err(error) => return fallback(&format!("无法创建 Grok Bot 客户端：{error}")),
    };
    let response = match signed(client.post(USAGE_URL), &credentials)
        .header("content-type", "application/proto")
        .body(Vec::new())
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return fallback(&format!("Grok Bot 用量请求失败：{error}")),
    };
    if response.status() == reqwest::StatusCode::UNAUTHORIZED
        || response.status() == reqwest::StatusCode::FORBIDDEN
    {
        return action_required("Grok Bot 拒绝了保存的访问令牌。");
    }
    if !response.status().is_success() {
        return fallback(&http_failure(response.status(), "Grok Bot 用量服务"));
    }
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            return fallback(&format!(
                "无法读取 Grok Bot 用量服务的返回：{error}"
            ))
        }
    };
    let status = match parse_usage_status(&bytes) {
        Ok(status) => status,
        Err(error) => {
            return fallback(&format!(
                "无法解析 Grok Bot 用量服务的返回：{error}"
            ))
        }
    };
    let quota = quota_from(status);
    if let ProviderQuota::Ok { plan, windows, .. } = &quota {
        let _ = write_snapshot(plan, windows);
    }
    quota
}

/// The headers Grok Bot itself sends with every dashboard request.
fn signed(request: reqwest::RequestBuilder, credentials: &Credentials) -> reqwest::RequestBuilder {
    let checksum = checksum_at(
        &credentials.machine_id,
        chrono::Utc::now().timestamp_millis(),
    );
    request
        .header(
            "authorization",
            format!("Bearer {}", credentials.access_token),
        )
        .header("connect-protocol-version", "1")
        .header("x-cursor-checksum", checksum)
        .header("x-cursor-client-type", "sand")
        .header("x-cursor-client-version", FALLBACK_CLIENT_VERSION)
        .header("x-sand-box-namespace", "prod")
        .header("x-ghost-mode", "true")
        .header("x-request-id", uuid_like_request_id())
}

/// Every token Grok Bot spent in `[start_ms, end_ms)`, from its server's
/// daily usage report. The report buckets by UTC day but filters on the exact
/// instants asked for, so a day that begins at Beijing midnight is counted as
/// such. One total per day is all it gives: no cache split, no call count.
pub async fn tokens_between(start_ms: i64, end_ms: i64) -> Result<u64, String> {
    let profile = profile_dir().ok_or("找不到 Grok Bot 的配置目录")?;
    if !profile.join("sand-secrets.json").is_file() || !profile.join("Local State").is_file() {
        return Err("没找到 Grok Bot 的登录记录，打开 Grok Bot 登录一次".to_string());
    }
    let credentials = tauri::async_runtime::spawn_blocking(move || load_credentials_from(&profile))
        .await
        .map_err(|error| format!("无法读取 Grok Bot 的登录记录：{error}"))??;
    if jwt_exp(&credentials.access_token).is_some_and(|expiry| expiry <= now()) {
        return Err("Grok Bot 的访问令牌过期了，打开 Grok Bot 让它刷新".to_string());
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECONDS))
        .build()
        .map_err(|error| format!("无法创建 Grok Bot 客户端：{error}"))?;
    let body = serde_json::json!({
        "periodStartMs": start_ms.to_string(),
        "periodEndMs": end_ms.to_string(),
        "groupBy": GROUP_BY_MODEL,
        "spendType": SPEND_TYPE_ALL,
        "clientType": "grok-bot",
        "products": ["sand"],
    });
    let response = signed(client.post(DAILY_SPEND_URL), &credentials)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|error| format!("Grok Bot 用量请求失败：{error}"))?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED
        || response.status() == reqwest::StatusCode::FORBIDDEN
    {
        return Err("Grok Bot 拒绝了保存的访问令牌，打开 Grok Bot 重新登录".to_string());
    }
    if !response.status().is_success() {
        return Err(http_failure(response.status(), "Grok Bot 用量服务"));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("无法读取 Grok Bot 用量服务的返回：{error}"))?;
    daily_tokens(&bytes)
}

/// Sum `dailySpend[].totalTokens` of a GetDailySpendByCategory reply. Proto3
/// JSON writes 64-bit integers as strings and leaves zeros out.
fn daily_tokens(body: &[u8]) -> Result<u64, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Report {
        #[serde(default)]
        daily_spend: Vec<Day>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Day {
        total_tokens: Option<serde_json::Value>,
    }

    let report: Report = serde_json::from_slice(body)
        .map_err(|error| format!("无法解析 Grok Bot 用量服务的返回：{error}"))?;
    report.daily_spend.iter().try_fold(0_u64, |sum, day| {
        let tokens = match &day.total_tokens {
            None => Some(0),
            Some(serde_json::Value::String(text)) => text.parse::<u64>().ok(),
            Some(serde_json::Value::Number(number)) => number.as_u64(),
            Some(_) => None,
        }
        .ok_or("Grok Bot 用量服务返回了看不懂的 token 数")?;
        Ok(sum.saturating_add(tokens))
    })
}

fn uuid_like_request_id() -> String {
    // A request correlation id, not a credential. This avoids adding a UUID
    // dependency solely for a header the server treats as opaque.
    let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default() as u128;
    let pid = u128::from(std::process::id());
    let value = now ^ (pid << 64) ^ (&now as *const u128 as usize as u128);
    let hex = format!("{value:032x}");
    format!(
        "{}-{}-4{}-a{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut value: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            bytes.push(byte);
            if value == 0 {
                return bytes;
            }
        }
    }

    fn field_varint(field: u64, value: u64) -> Vec<u8> {
        let mut bytes = varint(field << 3);
        bytes.extend(varint(value));
        bytes
    }

    fn field_bytes(field: u64, value: &[u8]) -> Vec<u8> {
        let mut bytes = varint((field << 3) | 2);
        bytes.extend(varint(value.len() as u64));
        bytes.extend(value);
        bytes
    }

    fn timestamp(value: i64) -> Vec<u8> {
        field_varint(1, value as u64)
    }

    #[test]
    fn decrypts_chromium_v10_aes_gcm_values() {
        let key = [7_u8; 32];
        let nonce = [9_u8; 12];
        let clear = b"short-lived-access-token";
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), clear.as_slice())
            .unwrap();
        let mut blob = b"v10".to_vec();
        blob.extend(nonce);
        blob.extend(ciphertext);
        assert_eq!(
            decrypt_v10(&general_purpose::STANDARD.encode(blob), &key).unwrap(),
            clear
        );
    }

    #[test]
    fn parses_the_weekly_usage_response_without_account_data() {
        let mut bytes = field_bytes(1, &timestamp(1_787_500_000));
        bytes.extend(field_bytes(2, &timestamp(1_788_104_800)));
        bytes.extend(varint((3 << 3) | 1));
        bytes.extend(5.933123_f64.to_le_bytes());
        bytes.extend(field_varint(7, 1));
        bytes.extend(field_varint(8, 1));
        bytes.extend(field_bytes(14, b"supergrok"));
        bytes.extend(field_bytes(15, b"SuperGrok"));
        // Unknown future length-delimited fields are ignored.
        bytes.extend(field_bytes(20, b"future"));

        let parsed = parse_usage_status(&bytes).unwrap();
        assert_eq!(parsed.current_period_start, Some(1_787_500_000));
        assert_eq!(parsed.next_reset, Some(1_788_104_800));
        assert_eq!(parsed.usage_percent, Some(5.933123));
        assert_eq!(parsed.plan_label.as_deref(), Some("SuperGrok"));
        assert_eq!(parsed.has_non_zero_included_limit, Some(true));

        let ProviderQuota::Ok {
            provider,
            plan,
            windows,
            ..
        } = quota_from(parsed)
        else {
            panic!("a metered Grok Bot account should produce a quota");
        };
        assert_eq!(provider, PROVIDER);
        assert_eq!(plan.as_deref(), Some("SuperGrok"));
        assert_eq!(windows[0].label, "Weekly");
        assert_eq!(windows[0].window_seconds, Some(604_800));
    }

    #[test]
    fn cache_never_outlives_a_day_or_a_reset() {
        let raw = serde_json::json!({
            "plan": "SuperGrok",
            "observed_at": 1_000,
            "windows": [{
                "label": "Weekly",
                "percent": 12.5,
                "resets_at": 200_000,
                "window_seconds": 604_800,
                "severity": null
            }]
        })
        .to_string();
        assert!(cached_quota_from_raw(&raw, 1_000 + SNAPSHOT_MAX_AGE_SECONDS, "offline").is_some());
        assert!(
            cached_quota_from_raw(&raw, 1_000 + SNAPSHOT_MAX_AGE_SECONDS + 1, "offline").is_none()
        );
        assert!(cached_quota_from_raw(&raw, 200_000, "offline").is_none());
    }

    #[test]
    fn percentage_is_usable_when_the_advisory_nonzero_flag_is_absent() {
        let status = UsageStatus {
            usage_percent: Some(0.0),
            next_reset: Some(2_000),
            current_period_start: Some(1_000),
            ..UsageStatus::default()
        };
        assert!(matches!(quota_from(status), ProviderQuota::Ok { .. }));
    }

    #[test]
    fn checksum_is_stable_inside_one_server_bucket() {
        assert_eq!(
            checksum_at("machine", 1_800_000_000_000),
            checksum_at("machine", 1_800_000_999_999)
        );
        assert_ne!(
            checksum_at("machine", 1_800_000_000_000),
            checksum_at("machine", 1_800_001_000_000)
        );
        assert!(checksum_at("machine", 1_800_000_000_000).ends_with("machine"));
    }

    #[test]
    #[ignore = "manual integration check against the locally installed Grok Bot"]
    fn installed_grok_bot_returns_a_live_weekly_quota() {
        let ProviderQuota::Ok {
            provider,
            windows,
            stale: None,
            ..
        } = tauri::async_runtime::block_on(fetch())
        else {
            panic!("the installed Grok Bot did not return a live quota");
        };
        assert_eq!(provider, PROVIDER);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].label, "Weekly");
    }

    #[test]
    fn sums_the_daily_report_whether_counts_are_strings_or_numbers() {
        let body = br#"{
            "categories": ["grok-bot-default", "grok-bot-automation"],
            "dailySpend": [
                {"category": "grok-bot-default", "day": "1795564800000", "totalTokens": "43374442"},
                {"category": "grok-bot-automation", "day": "1795564800000", "totalTokens": 1000},
                {"category": "grok-bot-cua", "day": "1795564800000", "spendCents": 0}
            ]
        }"#;
        assert_eq!(daily_tokens(body), Ok(43_375_442));
        assert_eq!(daily_tokens(b"{}"), Ok(0), "a day with nothing spent");
        assert_eq!(daily_tokens(br#"{"startDate":"1"}"#), Ok(0));
        assert!(daily_tokens(br#"{"dailySpend":[{"totalTokens":"-3"}]}"#).is_err());
        assert!(daily_tokens(br#"{"dailySpend":[{"totalTokens":true}]}"#).is_err());
        assert!(daily_tokens(b"not json").is_err());
    }

    #[test]
    #[ignore = "manual integration check against the locally installed Grok Bot"]
    fn installed_grok_bot_reports_tokens_since_beijing_midnight() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let offset_ms = 8 * 60 * 60 * 1000;
        let start_ms = (now_ms + offset_ms).div_euclid(86_400_000) * 86_400_000 - offset_ms;
        let tokens = tauri::async_runtime::block_on(tokens_between(start_ms, now_ms))
            .expect("the installed Grok Bot answered");
        println!("Grok Bot tokens since Beijing midnight: {tokens}");
    }
}
