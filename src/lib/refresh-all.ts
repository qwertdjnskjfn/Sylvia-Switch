/**
 * 全局刷新事件广播。
 *
 * 背景：各网关页的数据都是组件本地 state（没有共享 store），“刷新全部”
 * 无法通过调用统一 store 实现。仿照项目已有的
 * `ACCOUNT_EXTRAS_CHANGE_EVENT`（src/lib/account-extras.ts）模式：
 * 侧边栏按钮广播事件，各页面订阅后调用自己的 load 函数。
 *
 * 用法：
 *   发出：emitRefreshAll()
 *   订阅：useRefreshAllListener(() => void loadAll())
 */

export const REFRESH_ALL_EVENT = "wb-switch:refresh-all";

/** 广播一次全局刷新（幂等；无监听方时不报错）。 */
export function emitRefreshAll(): void {
  if (typeof window === "undefined") return;
  window.dispatchEvent(new CustomEvent(REFRESH_ALL_EVENT));
}

/**
 * 订阅全局刷新事件，返回取消订阅函数（供 useEffect cleanup 调用）。
 *
 * 注意：监听方应自行做并发保护（按钮侧也有 busy 态），避免与页面自身的
 * 轮询/手动刷新叠加打成惊群。
 */
export function useRefreshAllListener(handler: () => void): () => void {
  if (typeof window === "undefined") return () => undefined;
  const listener = () => handler();
  window.addEventListener(REFRESH_ALL_EVENT, listener);
  return () => window.removeEventListener(REFRESH_ALL_EVENT, listener);
}
