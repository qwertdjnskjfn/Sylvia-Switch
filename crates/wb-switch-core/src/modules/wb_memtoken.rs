//! WorkBuddy 登录态内存提取（加密 auth 文件的导入回退）。
//!
//! 背景：WorkBuddy 5.5.6 起把 `auth/workbuddy-desktop.info` 的
//! accessToken / refreshToken / nickname 改为 `$wbEncrypted` 信封
//! （AES-GCM，protector 密钥内置在定制 Electron 原生模块
//! `workbuddyStorage` 里，静态不可提取）。明文导入因此失效。
//!
//! 回退方案：WorkBuddy 客户端运行时会在内存中持有已解密的 token
//! （V8 堆，ASCII 与 UTF-16LE 两种编码均有可能）。这里扫描其进程
//! 内存，按 `sub`/`uid` claim 匹配目标账号，区分 access（typ=Bearer）
//! 与 refresh（typ=Offline）后返回。仅用于本机、本人账号导入到自己
//! 的管理工具，与其它平台读取本地凭证的既有行为一致。

use serde_json::Value;

/// JWT 头前缀（RS256 与 HS256 都覆盖）。V8 堆可能以 UTF-16LE 存字符串，
/// 因此每种编码各扫一遍；UTF-16LE 版长度 >20 用于区分两种步进。
fn patterns() -> Vec<Vec<u8>> {
    ["eyJhbGciOiJSUzI1NiIs", "eyJhbGciOiJIUzI1NiIs"]
        .iter()
        .flat_map(|head| {
            let ascii = head.as_bytes().to_vec();
            let utf16: Vec<u8> = head.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
            [ascii, utf16]
        })
        .collect()
}

/// 真实 WorkBuddy token 的最小长度（实测 access 1498 / refresh 1499）。
/// 内存里存在被截断的同形 JWT 片段（241 字符等），不滤除会在按 iat
/// 选优时把垃圾串当成最新 token。
const MIN_TOKEN_LEN: usize = 1000;

/// token 字符集（base64url + 点分隔）。
fn is_token_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'.'
}

/// 从一段内存里抽取所有 JWT 形态的长字符串。
fn extract_jwts(data: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    for pat in patterns() {
        let utf16 = pat.len() > 20;
        let step = if utf16 { 2 } else { 1 };
        // memmem 是 SIMD 子串搜索（O(n)）；朴素 windows 比较在 GB 级内存上
        // 会跑几百亿次字节操作，实测把单次扫描拖到数分钟。
        let mut search_from = 0usize;
        while let Some(rel) = memchr::memmem::find(&data[search_from..], &pat) {
            let i = search_from + rel;
            let mut p = i;
            let mut chars = Vec::new();
            // UTF-16LE 的字符在低字节（'e' = 65 00），与 ASCII 同样读 data[p]，
            // 差别只在步进。
            while p + step <= data.len() {
                let c = data[p];
                if !is_token_char(c) {
                    break;
                }
                chars.push(c);
                p += step;
            }
            // 长度过滤：>100 挡普通噪声，真实 token 由调用方按 MIN_TOKEN_LEN
            // 再筛（截断片段也是合法 JWT 形态，只能靠长度区分）。
            if chars.len() > 100 {
                out.push(String::from_utf8_lossy(&chars).into_owned());
            }
            search_from = i + 1;
        }
    }
    out
}

/// 供外部读取 token payload（昵称兜底等展示用途）。
pub(crate) fn jwt_payload_for_display(token: &str) -> Option<Value> {
    jwt_payload(token)
}

/// 解码 JWT payload（base64url → JSON）。
fn jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 批次扫描一个进程的内存，回调命中到的所有 JWT。
/// `stop` 置位后尽快结束扫描（调用方已拿到所需 token）。
#[cfg(target_os = "windows")]
fn scan_process(pid: u32, stop: &std::sync::atomic::AtomicBool, mut on_token: impl FnMut(&str, &Value)) {
    use std::ffi::c_void;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::Memory::{
        VirtualQueryEx, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_NOACCESS, PAGE_GUARD,
        PAGE_PROTECTION_FLAGS,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };

    let Ok(handle) = (unsafe {
        OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_INFORMATION, false, pid)
    }) else {
        return;
    };
    let mut addr: usize = 0;
    let max_addr: usize = 0x7FFF_FFFF_0000;
    let mut buf = vec![0u8; 1 << 20];
    while addr < max_addr {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        let written = unsafe {
            VirtualQueryEx(
                handle,
                Some(addr as *const c_void),
                &mut mbi,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if written == 0 {
            break;
        }
        let base = mbi.BaseAddress as usize;
        let size = mbi.RegionSize;
        if size == 0 {
            break;
        }
        if size > (1 << 30) {
            addr = base + size;
            continue;
        }
        let readable = mbi.State == MEM_COMMIT
            && !mbi.Protect.contains(PAGE_GUARD)
            && !mbi.Protect.contains(PAGE_NOACCESS)
            && mbi.Protect != PAGE_PROTECTION_FLAGS(0);
        if readable {
            let mut off = 0usize;
            while off < size {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let to_read = buf.len().min(size - off);
                let mut got = 0usize;
                let ok = unsafe {
                    ReadProcessMemory(
                        handle,
                        (base + off) as *const c_void,
                        buf.as_mut_ptr() as *mut c_void,
                        to_read,
                        Some(&mut got),
                    )
                }
                .is_ok();
                if !ok || got == 0 {
                    break;
                }
                for token in extract_jwts(&buf[..got]) {
                    if let Some(payload) = jwt_payload(&token) {
                        on_token(&token, &payload);
                    }
                }
                // 重叠 4KB，避免 JWT 跨读取边界被截断。
                // 重叠 4KB 推进，避免 JWT 跨读取边界被截断；部分读取时按实际读取
// 字节全量前进（每次只 +1 会在 RPM 返回部分读取时退化为每步重读 1MB
// 的病态循环，实测把整轮扫描拖到数十分钟）。
                off += if got > 4096 { got - 4096 } else { got.max(1) };
            }
        }
        addr = base + size;
        if addr <= base {
            break;
        }
    }
    unsafe {
        let _ = CloseHandle(handle);
    }
}

/// token 分类：access 是 typ=Bearer 的会话令牌，refresh 是 typ=Offline。
fn jwt_sub(payload: &Value) -> Option<&str> {
    payload
        .get("sub")
        .or_else(|| payload.get("uid"))
        .and_then(Value::as_str)
}

fn jwt_typ(payload: &Value) -> &str {
    payload.get("typ").and_then(Value::as_str).unwrap_or("")
}

/// 从运行中的 WorkBuddy 进程提取指定 uid 账号的 (access, refresh, access_exp, refresh_exp)。
///
/// `expected_uid` 为空时不过滤（调用方应始终传 auth 文件里的明文 uid）。
/// 任一 token 找不到时返回 None。
#[cfg(target_os = "windows")]
pub fn extract_account_tokens(expected_uid: &str) -> Option<(String, String, i64, i64)> {
    let pids = crate::modules::process::windows_tasklist_image_rows("WorkBuddy.exe")
        .into_iter()
        .map(|row| row.pid)
        .collect::<Vec<_>>();
    if pids.is_empty() {
        return None;
    }

    // token 集中在少数进程的少量区段，拿到 access+refresh 立即停，
    // 避免全量扫完所有进程的 GB 级内存（实测全量要数分钟）。
    let stop = std::sync::atomic::AtomicBool::new(false);
    let mut best_access: Option<(i64, String)> = None; // (iat, token)
    let mut best_refresh: Option<(i64, String)> = None;
    for pid in pids {
        scan_process(pid, &stop, |token, payload| {
            // 过滤截断片段（与真实 token 同 JWT 形态，仅长度不同）。
            if token.len() < MIN_TOKEN_LEN {
                return;
            }
            let Some(sub) = jwt_sub(payload) else {
                return;
            };
            if !expected_uid.is_empty() && sub != expected_uid {
                return;
            }
            let iat = payload.get("iat").and_then(Value::as_i64).unwrap_or(0);
            let typ = jwt_typ(payload);
            let slot = if typ.eq_ignore_ascii_case("Offline") {
                &mut best_refresh
            } else if typ.eq_ignore_ascii_case("Bearer") || typ.is_empty() {
                &mut best_access
            } else {
                return;
            };
            let better = slot.as_ref().map(|(old, _)| iat > *old).unwrap_or(true);
            if better {
                *slot = Some((iat, token.to_string()));
            }
            if best_access.is_some() && best_refresh.is_some() {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
    }

    let access = best_access?;
    let refresh = best_refresh?;
    let access_exp = jwt_payload(&access.1)
        .and_then(|p| p.get("exp").and_then(Value::as_i64))
        .unwrap_or(0);
    let refresh_exp = jwt_payload(&refresh.1)
        .and_then(|p| p.get("exp").and_then(Value::as_i64))
        .unwrap_or(0);
    Some((access.1, refresh.1, access_exp, refresh_exp))
}

#[cfg(not(target_os = "windows"))]
pub fn extract_account_tokens(_expected_uid: &str) -> Option<(String, String, i64, i64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_jwt(payload_json: &str) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
        let sig = URL_SAFE_NO_PAD.encode(b"signature-bytes-here");
        format!("{header}.{payload}.{sig}")
    }

    #[test]
    fn extracts_ascii_jwt_from_buffer() {
        let token = make_jwt(r#"{"sub":"uid-1","typ":"Bearer","iat":100,"exp":200}"#);
        let mut buf = b"\"token\":\"".to_vec();
        buf.extend_from_slice(token.as_bytes());
        buf.extend_from_slice(b"\",\"exp\":"); // 实际内存里 token 以引号/逗号结尾
        let found = extract_jwts(&buf);
        assert_eq!(found, vec![token.clone()]);
        let payload = jwt_payload(&token).expect("payload decodes");
        assert_eq!(jwt_sub(&payload), Some("uid-1"));
        assert_eq!(jwt_typ(&payload), "Bearer");
    }

    #[test]
    fn extracts_utf16le_jwt_from_buffer() {
        let token = make_jwt(r#"{"sub":"uid-2","typ":"Offline","iat":100,"exp":9999}"#);
        let utf16: Vec<u8> = token
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let terminator: Vec<u8> = "\",".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let mut buf = vec![0u8, 0];
        buf.extend_from_slice(&utf16);
        buf.extend_from_slice(&terminator);
        let found = extract_jwts(&buf);
        assert_eq!(found, vec![token.clone()]);
        let payload = jwt_payload(&token).expect("payload decodes");
        assert_eq!(jwt_typ(&payload), "Offline");
    }

    #[test]
    fn ignores_short_or_foreign_strings() {
        let buf = b"eyJhbGciOiJSUzI1NiIs.short";
        assert!(extract_jwts(buf).is_empty());
    }
}


#[cfg(test)]
mod perf_tests {
    use super::*;

    #[test]
    #[ignore = "live integration: requires WorkBuddy running"]
    fn perf_extract_account_tokens() {
        let uid = "5c57d4da-d5d3-4f5b-9d4c-97ac45e4c31e";
        let t0 = std::time::Instant::now();
        let pids = crate::modules::process::windows_tasklist_image_rows("WorkBuddy.exe")
            .into_iter()
            .map(|row| row.pid)
            .collect::<Vec<_>>();
        println!("pids={:?}", pids);
        for pid in &pids {
            let tp = std::time::Instant::now();
            let mut count = 0usize;
            let stop = std::sync::atomic::AtomicBool::new(false);
            scan_process(*pid, &stop, |token, payload| {
                count += 1;
                println!(
                    "  PID={pid} token len={} sub={:?} typ={}",
                    token.len(),
                    jwt_sub(payload),
                    jwt_typ(payload)
                );
                if count >= 6 {
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            });
            println!("PID={pid} scanned in {:?} ({} tokens)", tp.elapsed(), count);
        }
        let result = extract_account_tokens(uid);
        println!(
            "extract_account_tokens ok={} took {:?}",
            result.is_some(),
            t0.elapsed()
        );
        panic!("see output");
    }
}
