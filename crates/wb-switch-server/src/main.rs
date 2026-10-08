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
/// 把签到一轮的关键字段打进 stdout（2026-10-08）：
/// 修复"调度层 `let _ =` 吞掉结果"——此前循环崩掉或全失败时，日志里看不到任何痕迹。
fn log_checkin_round(mode: &str, summary: &serde_json::Value) {
    let status = summary.get("status").and_then(|v| v.as_str()).unwrap_or("?");
    let accounts = summary.get("accounts").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    let backoff = summary.get("backoffSkipped").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    let relogin = summary.get("needRelogin").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    println!(
        "[签到] {mode} 轮次结束: status={status} 账号={accounts} 退避跳过={backoff} 需重新登录={relogin}"
    );
    for item in summary.get("needRelogin").and_then(|v| v.as_array()).into_iter().flatten() {
        if let Some(s) = item.as_str() {
            println!("[签到]   ⚠ 需重新登录: {s}");
        }
    }
}

fn spawn_background_loops() {
    tokio::spawn(async move {
        if let Err(error) = config::compact_checkin_logs() {
            eprintln!("[签到] 历史日志整理失败: {error}");
        }
        // 2026-10-08：不再 `let _ =` 丢弃本轮结果 —— 签到失效时必须留痕（stdout 日志）。
        {
            let summary = checkin::run_checkin_cycle(checkin::CheckinCycleMode::StartupVerify).await;
            log_checkin_round("StartupVerify", &summary);
        }

        let mut last_date = checkin::current_local_date();
        loop {
            // 计算距离下一个零点（00:00:05）的时长
            let midnight_dur = checkin::duration_until_next_midnight();
            // 常规巡检间隔：当天有未签到账号时 5 分钟积极重试，全部已签到时 30 分钟兜底巡检
            // 2026-10-08：若剩余未完成账号全部处于退避/待重新登录，也切到 30min 兜底档，
            // 避免"几个号登录态过期"导致每 5 分钟重刷全部账号。
            let all_done = checkin::all_accounts_checked_in_today()
                || checkin::all_pending_accounts_backing_off();
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

            let summary = checkin::run_checkin_cycle(checkin::CheckinCycleMode::PeriodicRecovery).await;
            log_checkin_round("PeriodicRecovery", &summary);
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
    // Build 通道托管看门狗：grok2api（8002）掉线自动拉起（60s 巡检）。
    wb_switch_core::modules::grok2api_manager::spawn_watchdog();
    // Grok SSO 同步循环（30 分钟；专用 Chrome 实例未启动时该轮静默失败不刷屏）。
    wb_switch_core::modules::grok_sso::spawn_sync_loop();
    // v1.5.0：冷账号保活续签循环（每 6 小时；仅对长期未活跃的 web 账号）。
    wb_switch_core::modules::grok_sso::spawn_keepalive_loop();
    // v1.5.0：账号池健康体检循环（默认每 2 小时轮转 1 个账号，策略可调）。
    wb_switch_core::modules::grok_pool::spawn_health_check_loop();
    wb_switch_core::modules::autoclaw_watch::spawn_watch_loop();
    // 国际号轮换池（契约 docs/autoclaw-intl-pool-contract.md §3.4/§3.7）：
    // TokenKeeper 续期循环（15min 扫描 + 401 即时触发，续期回调复用
    // refresh_stored_account 按 region 路由国际网关）+ watch 订阅
    // （60s 读探测结果，把 intl 号 banned/invalid/quota 单向映射进状态机）。
    {
        use wb_switch_core::modules::autoclaw_intl_pool;
        let manager = autoclaw_intl_pool::pool_manager();
        manager.install_default_refresh_callback();
        // 启动对账（契约 §3.1 幂等入池的服务端兜底）：凭据里有 intl 条目但池
        // 状态缺失（池文件被清/首次部署）时自动 join 重建，避免 list 全落
        // "missing" 自愈视图。
        let intl_creds: Vec<serde_json::Value> =
            wb_switch_core::modules::autoclaw_accounts::load_accounts()
                .into_iter()
                .filter(|a| a.get("region").and_then(serde_json::Value::as_str) == Some("international"))
                .collect();
        let rejoined = manager.reconcile_from_credentials(&intl_creds);
        if rejoined > 0 {
            eprintln!("[intl-pool] 启动对账：从凭据重建 {rejoined} 个国际号池条目");
        }
        manager.spawn_token_keeper();
        autoclaw_intl_pool::PoolManager::spawn_watch_subscriber();
    }
    wb_switch_core::modules::zcode::spawn_auto_switch_loop();
    wb_switch_core::modules::minimax::spawn_signin_loop();
    // WorkBuddy 多账号网关：余额巡检循环（启动后 60s 首轮，之后每 balance_refresh_minutes 分钟；0 = 关闭）。
    wb_switch_core::modules::workbuddy_gateway::spawn_balance_patrol_loop();
    // 2026-10-08：统一积分巡检（MiniMax/Loomy/DuMate/妙手/TRAE… 此前**无后台刷新**，
    // 只有打开页面才按需拉 ⇒ 积分长期过期）。复用统一入口 get_platform_credits。
    wb_switch_core::modules::platform_checkin::spawn_credit_patrol_loop();
    // 模型目录自动同步循环（契约 model-sync-contract.md §3.5：启动 5 分钟后首轮，
    // 之后每 intervalMinutes 分钟；0 = 关闭但循环空转，配置热更新每轮现读）。
    wb_switch_core::modules::model_sync::spawn_model_sync_loop();
    // ChatGPT 通道托管循环（契约 chatgpt-channel-contract.md §3/§5：60s 轮询，
    // enabled+auto_start 才拉起 WebUI 服务进程；🔴 只托管服务，绝不自动启动批量注册）。
    wb_switch_core::modules::chatgpt_channel::spawn_watchdog();
    // ZCode 活动自动领取循环（30 分钟巡检；autoClaim 开关在 ~/.wb-switch/zcode_claim_state.json）。
    wb_switch_core::modules::zcode::spawn_claim_loop();

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
