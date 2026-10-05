/**
 * 账号辅助功能：自定义别名/备注名持久化。
 *
 * 隐私脱敏机制已按 docs/unified-accounts-contract.md §3 移除：
 * 全界面直显完整全名（备注名优先），前端不再自行加星号。
 */

const NOTES_STORAGE_KEY = "wb-switch.account-notes";
export const ACCOUNT_EXTRAS_CHANGE_EVENT = "wb-switch:account-extras-change";

function dispatchChange() {
  if (typeof window !== "undefined") {
    window.dispatchEvent(new CustomEvent(ACCOUNT_EXTRAS_CHANGE_EVENT));
  }
}

/** 读取所有账号的自定义备注名映射表 */
export function getAccountNotes(): Record<string, string> {
  try {
    const raw = localStorage.getItem(NOTES_STORAGE_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw);
    return typeof parsed === "object" && parsed !== null ? parsed : {};
  } catch {
    return {};
  }
}

/** 保存或更新指定账号的备注名（传空则删除） */
export function setAccountNote(id: string, note: string): void {
  try {
    const current = getAccountNotes();
    const trimmed = note.trim();
    if (trimmed) {
      current[id] = trimmed;
    } else {
      delete current[id];
    }
    localStorage.setItem(NOTES_STORAGE_KEY, JSON.stringify(current));
    dispatchChange();
  } catch {
    // 静默忽略本地存储异常
  }
}
