//! 统一统计模块。
//!
//! 把「模型用量 / Token / 积分」三个统计模块的口径与聚合收敛到一处。
//!
//! 目标分层（逐阶段落地）：
//! ```text
//! view   →  engine  →  event        单向依赖
//! source →  event
//! ```
//! - `units`  ：单位与边界口径的唯一定义（P0，已落地）
//! - `window` ：时间窗口 / 本地日 / 分桶的唯一定义（P1，已落地）
//! - `engine` ：聚合内核与派生指标，纯函数（P1，已落地）
//! - `event`  ：统一事件模型 `StatEvent` / `EventKey` + 来源适配器（P1，已落地）
//! - `reconcile`：官方流水认领的唯一实现（P2）
//! - `source` ：四个数据源适配器（P1/P2）
//! - `view`   ：旧接口的响应投影，保证兼容（P3）
//!
//! 规则：`engine` 不得依赖任何 `source`；本模块内的纯逻辑不得直接做 IO。

pub mod engine;
pub mod event;
pub mod reconcile;
pub mod source;
pub mod units;
pub mod window;
