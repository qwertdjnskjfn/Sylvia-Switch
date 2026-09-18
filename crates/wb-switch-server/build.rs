//! 构建期注入「这一版二进制到底是谁」的标识，供 `GET /api/version` 自证。
//!
//! 为什么需要：部署脚本 `deploy.py` 过去探测的 `/api/version` 其实并不存在，
//! 被 SPA 兜底当成前端路由返回了 index.html（HTTP 200），于是「健康检查」
//! 只要端口有人监听就恒为真 —— 换 exe 后跑的是新二进制还是旧二进制，脚本
//! 无法分辨（2026-09-16 定位）。补上真实端点 + 构建时间戳后，脚本可以断言
//! 「服务端自报的版本 == 刚构建出来的版本」。
//!
//! 注意：这里**故意不写** `cargo:rerun-if-changed`。默认行为是「包内任一文件
//! 变化就重跑本脚本」，这正是我们要的（源码变了 → 构建时间刷新）。若写成
//! `rerun-if-changed=build.rs`，改源码时本脚本不会重跑，时间戳会变陈旧。

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // 本机是 UTC+8；不引入 chrono，直接手工换算（避免多一个依赖）。
    let local = epoch + 8 * 3600;
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    let secs_of_day = local.rem_euclid(86_400);
    let (hh, mm, ss) = (secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60);

    println!("cargo:rustc-env=WB_BUILD_TIME={y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02} +0800");
    println!("cargo:rustc-env=WB_BUILD_EPOCH={epoch}");

    // git 可能不存在、可能不在 PATH、也可能不是仓库 —— 一律降级为 unknown，
    // 绝不因为取不到 sha 就让构建失败。
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=WB_GIT_SHA={sha}");
}

/// Howard Hinnant 的 civil_from_days：把「1970-01-01 起的天数」换算成公历年月日。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}
