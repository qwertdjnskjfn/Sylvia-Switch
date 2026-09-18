import { useEffect, useState } from "react";
import { applyAccent, getAccent } from "./accent";
import { withCircleReveal } from "./view-transition";

export type ThemePreference = "system" | "light" | "dark";

export const THEME_CHANGE_EVENT = "wb-switch:theme-change";
const THEME_STORAGE_KEY = "wb-switch.theme";
const LEGACY_DARK_STORAGE_KEY = "wb-switch.dark";

function isThemePreference(value: string | null): value is ThemePreference {
  return value === "system" || value === "light" || value === "dark";
}

export function getThemePreference(): ThemePreference {
  try {
    const stored = localStorage.getItem(THEME_STORAGE_KEY);
    if (isThemePreference(stored)) return stored;

    const legacyDark = localStorage.getItem(LEGACY_DARK_STORAGE_KEY);
    if (legacyDark === "1") return "dark";
    if (legacyDark === "0") return "light";
  } catch {
    // 存储不可用时仍可跟随系统主题。
  }

  return "system";
}

export function applyTheme(preference: ThemePreference) {
  const dark =
    preference === "dark" ||
    (preference === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.classList.toggle("dark", dark);
  // 明暗切换后，强调色需要按新模式的观感重新取值
  applyAccent(getAccent());
}

export function setThemePreference(preference: ThemePreference) {
  try {
    localStorage.setItem(THEME_STORAGE_KEY, preference);
    localStorage.removeItem(LEGACY_DARK_STORAGE_KEY);
  } catch {
    // 存储不可用时仍立即应用当前选择。
  }
  applyTheme(preference);
  if (typeof window !== "undefined") {
    window.dispatchEvent(new CustomEvent(THEME_CHANGE_EVENT, { detail: preference }));
  }
}

export function watchSystemTheme() {
  const media = window.matchMedia("(prefers-color-scheme: dark)");
  const onChange = () => {
    if (getThemePreference() === "system") applyTheme("system");
  };

  media.addEventListener("change", onChange);
  return () => media.removeEventListener("change", onChange);
}

/** 切换明暗主题，并带圆形扩散动效（origin 为触发点坐标）。 */
export function switchTheme(
  preference: ThemePreference,
  origin?: { x: number; y: number },
) {
  withCircleReveal(() => setThemePreference(preference), origin);
}

/** 统一的主题响应式 Hook，支持跨组件双向联动 */
export function useTheme() {
  const [theme, setThemeState] = useState<ThemePreference>(getThemePreference);

  useEffect(() => {
    const onThemeChange = () => {
      setThemeState(getThemePreference());
    };
    window.addEventListener(THEME_CHANGE_EVENT, onThemeChange);
    const unwatch = watchSystemTheme();
    return () => {
      window.removeEventListener(THEME_CHANGE_EVENT, onThemeChange);
      unwatch();
    };
  }, []);

  return {
    theme,
    setTheme: (pref: ThemePreference, origin?: { x: number; y: number }) => {
      if (origin) {
        switchTheme(pref, origin);
      } else {
        setThemePreference(pref);
      }
    },
  };
}

