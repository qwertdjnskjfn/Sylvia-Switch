//! 统计数据源适配器。
//!
//! 约定：`source/*` 只负责「把外部数据取回来并归一化为 `stats` 层类型」，
//! **不做聚合**（聚合在 [`super::engine`]），也不做口径决策（口径在 [`super::units`] /
//! [`super::window`]）。

pub mod ledger;
