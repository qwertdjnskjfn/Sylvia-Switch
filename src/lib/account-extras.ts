/**
 * 账号辅助功能：自定义别名/备注名持久化与隐私脱敏支持。
 */

const NOTES_STORAGE_KEY = "wb-switch.account-notes";
const PRIVACY_STORAGE_KEY = "wb-switch.privacy-mask";
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

/** 读取隐私脱敏开关状态 */
export function getPrivacyMask(): boolean {
  try {
    return localStorage.getItem(PRIVACY_STORAGE_KEY) === "1";
  } catch {
    return false;
  }
}

/** 切换或设置隐私脱敏模式 */
export function setPrivacyMask(enabled: boolean): void {
  try {
    localStorage.setItem(PRIVACY_STORAGE_KEY, enabled ? "1" : "0");
    dispatchChange();
  } catch {
    // 忽略异常
  }
}

/** 邮箱脱敏：保留首尾字符，中间以星号替代（如 zh***@qq.com） */
export function maskEmail(email: string): string {
  if (!email) return "";
  const parts = email.split("@");
  if (parts.length !== 2) {
    if (email.length <= 4) return email;
    return `${email.slice(0, 2)}***${email.slice(-2)}`;
  }
  const [local, domain] = parts;
  if (local.length <= 2) {
    return `${local[0] || "*"}***@${domain}`;
  }
  return `${local.slice(0, 2)}***${local.slice(-1)}@${domain}`;
}

/** 普通文本/UID 脱敏：保留前 3 和后 2 位 */
export function maskIdentity(str: string): string {
  if (!str) return "";
  if (str.length <= 6) return `${str.slice(0, 1)}****`;
  return `${str.slice(0, 3)}****${str.slice(-2)}`;
}
