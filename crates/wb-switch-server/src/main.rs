//! workbuddy-switch CLI：npm 安装形态的入口。
//!
//! ```bash
//! workbuddy-switch              # 启动本地服务 + 打开浏览器 webui
//! workbuddy-switch serve        # 只起服务不开浏览器（--port / --no-open）
//! workbuddy-switch status       # 终端输出当前账号
//! workbuddy-switch version      # 版本号
//! ```

mod api;

use serde_json::json;

use wb_switch_core::modules::{
    account, account_protection, auth_file, checkin, config, official_usage, process, travel,
    update, workbuddy_proxy,
};

fn default_port() -> u16 {
    57890
}

/// 后台任务：自动签到启动即核验、每 30 分钟补签。
fn spawn_background_loops() {
    tokio::spawn(async move {
        if let Err(error) = config::compact_checkin_logs() {
            eprintln!("[签到] 历史日志整理失败: {error}");
        }
        let _ = checkin::run_checkin_cycle(checkin::CheckinCycleMode::StartupVerify).await;

        let mut last_date = checkin::current_local_date();
        loop {
            // 计算距离下一个零点（00:00:05）的时长
            let midnight_dur = checkin::duration_until_next_midnight();
            // 常规巡检间隔：当天有未签到账号时 5 分钟积极重试，全部已签到时 30 分钟兜底巡检
            let all_done = checkin::all_accounts_checked_in_today();
            let check_interval = if all_done {
                checkin::CHECKIN_RECOVERY_INTERVAL
            } else {
                std::time::Duration::from_secs(5 * 60)
            };

            // 睡眠等待：取两者较小值（若零点更近则在零点准时醒来）
            let sleep_dur = midnight_dur.min(check_interval);

            // 每 15 秒检查一次是否发生系统时间跳变或跨天（完美应对笔记本休眠唤醒）
            let sleep_deadline = tokio::time::Instant::now() + sleep_dur;
            while tokio::time::Instant::now() < sleep_deadline {
                let step = std::time::Duration::from_secs(15)
                    .min(sleep_deadline.saturating_duration_since(tokio::time::Instant::now()));
                tokio::time::sleep(step).await;
                let today = checkin::current_local_date();
                if today != last_date {
                    break;
                }
            }

            let today = checkin::current_local_date();
            if today != last_date {
                last_date = today;
                println!("[签到] 跨天唤醒，立即执行新一天全账号自动签到");
            }

            let _ = checkin::run_checkin_cycle(checkin::CheckinCycleMode::PeriodicRecovery).await;
        }
    });

    // 派猫猫旅行：启动即派发，之后周期性补派（并重试 no-buddy / 瞬时错误）。
    tokio::spawn(async move {
        let _ = travel::run_travel_cycle().await;
        loop {
            tokio::time::sleep(travel::TRAVEL_RETRY_INTERVAL).await;
            let _ = travel::run_travel_cycle().await;
        }
    });

    // 旅行领取：启动立刻查一轮（避免重启后空等 15 分钟漏领），之后按周期检查。
    tokio::spawn(async move {
        let _ = travel::run_travel_claim_cycle().await;
        loop {
            tokio::time::sleep(travel::TRAVEL_CLAIM_INTERVAL).await;
            let _ = travel::run_travel_claim_cycle().await;
        }
    });

    // 官方用量投影：启动即同步（覆盖跨天后的第一屏），之后每 10 分钟滚动刷新。
    // 没有这条周期任务时，官方投影只在用户点击「刷新统计」时才会重采，
    // 导致「今日消耗」长期停留在上一次采集的数字上（跨天更会整天错位）。
    tokio::spawn(async move {
        loop {
            official_usage::background_sync_once().await;
            tokio::time::sleep(official_usage::OFFICIAL_USAGE_SYNC_INTERVAL).await;
        }
    });

    // 账号保护与 AutoClaw 反代主动保活：启动巡检、周期心跳与前瞻式静默续期。
    account_protection::spawn_protection_loop();
    // WorkBuddy 反代上游（独立 OpenCode server，4097）模块级看门狗：
    // 与 Startup 文件夹的 vbs 看门狗协同，30s 探活 + 限流重启。
    workbuddy_proxy::spawn_server_watchdog();
}

fn print_status() {
    let auth = auth_file::read_auth_file();
    let current = auth.as_ref().and_then(|a| {
        let acct = a.get("account").cloned().unwrap_or_else(|| json!({}));
        Some(json!({
            "uid": acct.get("uid"),
            "nickname": acct.get("nickname"),
            "email": acct.get("email"),
        }))
    });
    let running = process::is_workbuddy_running();
    println!("workbuddy-switch v{}", update::APP_VERSION);
    println!("WorkBuddy 运行中: {}", if running { "是" } else { "否" });
    match current {
        Some(c) => {
            let name = c
                .get("nickname")
                .and_then(|v| v.as_str())
                .or_else(|| c.get("email").and_then(|v| v.as_str()))
                .unwrap_or("未知");
            println!("当前账号: {name}");
        }
        None => println!("当前账号: 未登录"),
    }
    println!("账号数: {}", account::load_accounts().len());
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("serve");
    match cmd {
        "status" => print_status(),
        "version" | "--version" | "-V" => {
            println!("workbuddy-switch {}", env!("CARGO_PKG_VERSION"));
        }
        "serve" | _ => serve(&args).await,
    }
}

async fn serve(args: &[String]) {
    let mut port = default_port();
    if let Some(i) = args.iter().position(|a| a == "--port") {
        if let Some(p) = args.get(i + 1).and_then(|p| p.parse::<u16>().ok()) {
            port = p;
        }
    }

    api::set_bound_port(port);
    let app = api::router();
    let addr = format!("127.0.0.1:{port}");
    // 端口可能被上一个实例短暂占用（部署换 exe、重复双击启动的窗口期）：
    // 重试 ~30 秒再放弃，避免"新进程起太早 → 直接退出 → 只能等看门狗下一轮"。
    // 8 秒曾经不够：旧进程刚退出时端口未必已可重新 bind，过早 exit(1) 会把停机窗口
    // 放大到一整个看门狗周期（实测 2026-09-14 出现过 94 秒窗口）。
    const BIND_ATTEMPTS: u32 = 75;
    const BIND_INTERVAL_MS: u64 = 400;
    let mut bound = None;
    for attempt in 0..BIND_ATTEMPTS {
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => {
                bound = Some(l);
                break;
            }
            Err(e) => {
                if attempt + 1 == BIND_ATTEMPTS {
                    eprintln!(
                        "启动失败: 端口 {port} 等待 {} 秒后仍被占用或不可用（{e}）。可用 --port 指定其他端口。",
                        BIND_ATTEMPTS as u64 * BIND_INTERVAL_MS / 1000
                    );
                    std::process::exit(1);
                }
                tokio::time::sleep(std::time::Duration::from_millis(BIND_INTERVAL_MS)).await;
            }
        }
    }
    let listener = bound.expect("listener bound");

    println!("workbuddy-switch v{}", update::APP_VERSION);
    println!("webui: http://{addr}");
    println!("按 Ctrl+C 停止服务。");

    let no_open = args.iter().any(|a| a == "--no-open");
    if !no_open {
        open_browser(&addr);
    }

    spawn_background_loops();
    // Grok SSO 同步循环（30 分钟；专用 Chrome 实例未启动时该轮静默失败不刷屏）。
    wb_switch_core::modules::grok_sso::spawn_sync_loop();
    // v1.5.0：冷账号保活续签循环（每 6 小时；仅对长期未活跃的 web 账号）。
    wb_switch_core::modules::grok_sso::spawn_keepalive_loop();
    // v1.5.0：账号池健康体检循环（默认每 2 小时轮转 1 个账号，策略可调）。
    wb_switch_core::modules::grok_pool::spawn_health_check_loop();
    // WorkBuddy 多账号网关：余额巡检循环（启动后 60s 首轮，之后每 balance_refresh_minutes 分钟；0 = 关闭）。
    wb_switch_core::modules::workbuddy_gateway::spawn_balance_patrol_loop();

    axum::serve(listener, app).await.unwrap();
}

fn open_browser(addr: &str) {
    let url = format!("http://{addr}");
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let mut c = std::process::Command::new("cmd");
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW：开浏览器不闪 cmd 窗
        }
        let _ = c.args(["/C", "start", &url]).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
    }
}
