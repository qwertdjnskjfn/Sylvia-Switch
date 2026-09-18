/**
 * 主题配色（强调色）系统。
 *
 * 设计思路：
 * - 每个预设包含 light / dark 两套 oklch 值，分别对应 `:root` 与 `.dark` 下的观感
 * - 应用方式：把强调色写到 <html> 的行内 CSS 变量上，覆盖 index.css 里的默认值
 *   （行内样式优先级高于 `:root` / `.dark` 规则，且无需重写样式表）
 * - 图表配色跟随：把 `--data-series-*` 整体做色相旋转（保持各系列相对关系，
 *   不会因换主题而变得难以区分）
 * - 支持自定义主题色：取色器 hex → OKLCH，自动裁到可读的亮度/彩度区间
 * - 切换时配合 view-transition.ts 做圆形扩散动画
 * - 选择结果持久化到 localStorage
 */

import { useEffect, useState } from "react";
import { withCircleReveal } from "./view-transition";

export const ACCENT_CHANGE_EVENT = "wb-switch:accent-change";

export type PresetAccentId = "emerald" | "sky" | "indigo" | "violet" | "rose" | "amber";
export type AccentId = PresetAccentId | "custom";

interface AccentTone {
  /** 强调色本体：--primary / --ring / --brand / --sidebar-primary / --sidebar-ring */
  accent: string;
  /** 强调色之上的前景色：--primary-foreground / --brand-foreground / --sidebar-primary-foreground */
  onAccent: string;
}

export interface AccentPreset {
  id: PresetAccentId;
  label: string;
  /** 色板按钮显示用的颜色 */
  swatch: string;
  /** 色板上对勾的颜色（保证对比度） */
  check: string;
  /** 色相（用于派生图表配色） */
  hue: number;
  light: AccentTone;
  dark: AccentTone;
}

export const ACCENT_PRESETS: AccentPreset[] = [
  {
    id: "emerald",
    label: "翡翠",
    swatch: "oklch(0.725 0.133 163.03)",
    check: "oklch(0.99 0.002 247.839)",
    hue: 163.03,
    light: { accent: "oklch(0.725 0.133 163.03)", onAccent: "oklch(0.99 0.002 247.839)" },
    dark: { accent: "oklch(0.696 0.17 162.48)", onAccent: "oklch(0.208 0.042 265.755)" },
  },
  {
    id: "sky",
    label: "天青",
    swatch: "oklch(0.68 0.14 235)",
    check: "oklch(0.99 0.002 247.839)",
    hue: 235,
    light: { accent: "oklch(0.68 0.14 235)", onAccent: "oklch(0.99 0.002 247.839)" },
    dark: { accent: "oklch(0.72 0.14 235)", onAccent: "oklch(0.208 0.042 265.755)" },
  },
  {
    id: "indigo",
    label: "靛蓝",
    swatch: "oklch(0.62 0.16 264)",
    check: "oklch(0.99 0.002 247.839)",
    hue: 264,
    light: { accent: "oklch(0.62 0.16 264)", onAccent: "oklch(0.99 0.002 247.839)" },
    dark: { accent: "oklch(0.70 0.15 264)", onAccent: "oklch(0.208 0.042 265.755)" },
  },
  {
    id: "violet",
    label: "紫罗兰",
    swatch: "oklch(0.62 0.18 300)",
    check: "oklch(0.99 0.002 247.839)",
    hue: 300,
    light: { accent: "oklch(0.62 0.18 300)", onAccent: "oklch(0.99 0.002 247.839)" },
    dark: { accent: "oklch(0.70 0.17 300)", onAccent: "oklch(0.208 0.042 265.755)" },
  },
  {
    id: "rose",
    label: "玫红",
    swatch: "oklch(0.64 0.19 15)",
    check: "oklch(0.99 0.002 247.839)",
    hue: 15,
    light: { accent: "oklch(0.64 0.19 15)", onAccent: "oklch(0.99 0.002 247.839)" },
    dark: { accent: "oklch(0.70 0.17 15)", onAccent: "oklch(0.208 0.042 265.755)" },
  },
  {
    id: "amber",
    label: "琥珀",
    swatch: "oklch(0.80 0.15 78)",
    check: "oklch(0.30 0.05 70)",
    hue: 78,
    light: { accent: "oklch(0.74 0.15 78)", onAccent: "oklch(0.30 0.05 70)" },
    dark: { accent: "oklch(0.80 0.14 78)", onAccent: "oklch(0.208 0.042 265.755)" },
  },
];

/**
 * 图表数据色板基准（与 index.css 的 `--data-series-*` 一致）。
 * 换主题色时按色相差值整体旋转，保持各系列之间的相对区分度。
 */
const SERIES_BASE: { name: string; lL: number; lD: number; c: number; h: number }[] = [
  { name: "emerald", lL: 0.62, lD: 0.73, c: 0.13, h: 163 },
  { name: "teal", lL: 0.66, lD: 0.76, c: 0.1, h: 190 },
  { name: "indigo", lL: 0.55, lD: 0.77, c: 0.11, h: 265 },
  { name: "violet", lL: 0.63, lD: 0.78, c: 0.11, h: 300 },
  { name: "amber", lL: 0.72, lD: 0.81, c: 0.12, h: 78 },
  { name: "rose", lL: 0.64, lD: 0.77, c: 0.12, h: 20 },
  { name: "sky", lL: 0.63, lD: 0.76, c: 0.1, h: 235 },
  { name: "lime", lL: 0.67, lD: 0.78, c: 0.11, h: 125 },
];

/** 基准色相（翡翠），图表旋转以此为原点 */
const BASE_HUE = 163.03;

const STORAGE_KEY = "wb-switch.accent";
const CUSTOM_STORAGE_KEY = "wb-switch.accent.custom";
const DEFAULT_ACCENT: AccentId = "emerald";

/** 自定义主题色默认值（紫罗兰） */
export const DEFAULT_CUSTOM_HEX = "#7c5cff";

const HEX_RE = /^#[0-9a-fA-F]{6}$/;

export function isHexColor(value: string): boolean {
  return HEX_RE.test(value);
}

// ---------------------------------------------------------------------------
// 颜色换算：hex → OKLCH
// ---------------------------------------------------------------------------

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function hexToRgb(hex: string): [number, number, number] {
  const match = HEX_RE.exec(hex.trim());
  if (!match) return [124, 92, 255];
  const n = Number.parseInt(match[1], 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

function srgbToLinear(channel: number): number {
  const s = channel / 255;
  return s <= 0.04045 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
}

/** 把 sRGB hex 转成 OKLCH（l: 0~1, c: 0~0.4, h: 0~360） */
export function hexToOklch(hex: string): { l: number; c: number; h: number } {
  const [r8, g8, b8] = hexToRgb(hex);
  const r = srgbToLinear(r8);
  const g = srgbToLinear(g8);
  const b = srgbToLinear(b8);

  const l_ = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
  const m_ = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
  const s_ = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);

  const L = 0.2104542553 * l_ + 0.793617785 * m_ - 0.0040720468 * s_;
  const A = 1.9779984951 * l_ - 2.428592205 * m_ + 0.4505937099 * s_;
  const B = 0.0259040371 * l_ + 0.7827717662 * m_ - 0.808675766 * s_;

  const c = Math.hypot(A, B);
  let h = (Math.atan2(B, A) * 180) / Math.PI;
  if (h < 0) h += 360;
  return { l: L, c, h };
}

/** 根据背景色亮度，返回对比度足够的前景色 */
export function contrastOnHex(hex: string): string {
  const { l } = hexToOklch(hex);
  return l > 0.68 ? "oklch(0.22 0.02 265)" : "oklch(0.99 0.002 247.839)";
}

// ---------------------------------------------------------------------------
// 读取 / 持久化
// ---------------------------------------------------------------------------

export function isAccentId(value: unknown): value is AccentId {
  return value === "custom" || ACCENT_PRESETS.some((preset) => preset.id === value);
}

export function getAccent(): AccentId {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (isAccentId(stored)) return stored;
  } catch {
    // 存储不可用时回落到默认配色
  }
  return DEFAULT_ACCENT;
}

export function getCustomHex(): string {
  try {
    const stored = localStorage.getItem(CUSTOM_STORAGE_KEY);
    if (stored && isHexColor(stored)) return stored.toLowerCase();
  } catch {
    // 忽略
  }
  return DEFAULT_CUSTOM_HEX;
}

export function getAccentPreset(id: AccentId): AccentPreset {
  return ACCENT_PRESETS.find((preset) => preset.id === id) ?? ACCENT_PRESETS[0];
}

// ---------------------------------------------------------------------------
// 应用到 DOM
// ---------------------------------------------------------------------------

function customTone(dark: boolean): AccentTone & { hue: number } {
  const { l, c, h } = hexToOklch(getCustomHex());
  // 裁到可读区间：浅色模式压暗一点，深色模式提亮一点
  const lightness = dark
    ? clamp(Math.max(l, 0.68), 0.62, 0.84)
    : clamp(l, 0.5, 0.72);
  const chroma = clamp(c, 0.05, 0.19);
  return {
    accent: `oklch(${lightness.toFixed(3)} ${chroma.toFixed(3)} ${h.toFixed(1)})`,
    onAccent: lightness > 0.7 ? "oklch(0.22 0.02 265)" : "oklch(0.99 0.002 247.839)",
    hue: h,
  };
}

function resolveTone(id: AccentId, dark: boolean): AccentTone & { hue: number } {
  if (id === "custom") return customTone(dark);
  const preset = getAccentPreset(id);
  const tone = dark ? preset.dark : preset.light;
  return { accent: tone.accent, onAccent: tone.onAccent, hue: preset.hue };
}

/** 把配色写入 <html> 行内 CSS 变量（按当前明暗模式取对应一套值）。 */
export function applyAccent(id: AccentId) {
  const dark = document.documentElement.classList.contains("dark");
  const tone = resolveTone(id, dark);
  const root = document.documentElement;

  root.style.setProperty("--primary", tone.accent);
  root.style.setProperty("--ring", tone.accent);
  root.style.setProperty("--brand", tone.accent);
  root.style.setProperty("--sidebar-primary", tone.accent);
  root.style.setProperty("--sidebar-ring", tone.accent);

  root.style.setProperty("--primary-foreground", tone.onAccent);
  root.style.setProperty("--brand-foreground", tone.onAccent);
  root.style.setProperty("--sidebar-primary-foreground", tone.onAccent);

  // 图表配色跟随：整体旋转色相，保持各系列相对关系
  const delta = tone.hue - BASE_HUE;
  for (const series of SERIES_BASE) {
    const lightness = dark ? series.lD : series.lL;
    const hue = (((series.h + delta) % 360) + 360) % 360;
    root.style.setProperty(
      `--data-series-${series.name}`,
      `oklch(${lightness} ${series.c} ${hue.toFixed(1)})`,
    );
  }

  root.dataset.accent = id;
}

export function persistAccent(id: AccentId) {
  try {
    localStorage.setItem(STORAGE_KEY, id);
  } catch {
    // 忽略存储失败，本次会话仍然生效
  }
}

export function setAccent(id: AccentId) {
  persistAccent(id);
  applyAccent(id);
  if (typeof window !== "undefined") {
    window.dispatchEvent(
      new CustomEvent(ACCENT_CHANGE_EVENT, {
        detail: { accent: id, customHex: getCustomHex() },
      }),
    );
  }
}

/** 切换主题色，并带圆形扩散动效（origin 为色块点击位置）。 */
export function switchAccent(id: AccentId, origin?: { x: number; y: number }) {
  withCircleReveal(() => setAccent(id), origin);
}

/** 应用自定义主题色（取色器实时调用，不做扩散动效以免抖动）。 */
export function applyCustomAccent(hex: string) {
  if (!isHexColor(hex)) return;
  try {
    localStorage.setItem(CUSTOM_STORAGE_KEY, hex.toLowerCase());
    localStorage.setItem(STORAGE_KEY, "custom");
  } catch {
    // 忽略存储失败
  }
  applyAccent("custom");
  if (typeof window !== "undefined") {
    window.dispatchEvent(
      new CustomEvent(ACCENT_CHANGE_EVENT, {
        detail: { accent: "custom", customHex: hex.toLowerCase() },
      }),
    );
  }
}

/** 响应式主题色 Hook，支持跨组件双向联动 */
export function useAccent() {
  const [accent, setAccentState] = useState<AccentId>(getAccent);
  const [customHex, setCustomHexState] = useState<string>(getCustomHex);

  useEffect(() => {
    const onAccentChange = () => {
      setAccentState(getAccent());
      setCustomHexState(getCustomHex());
    };
    window.addEventListener(ACCENT_CHANGE_EVENT, onAccentChange);
    return () => {
      window.removeEventListener(ACCENT_CHANGE_EVENT, onAccentChange);
    };
  }, []);

  return {
    accent,
    customHex,
    setAccent: (id: AccentId, origin?: { x: number; y: number }) => {
      if (origin) {
        switchAccent(id, origin);
      } else {
        setAccent(id);
      }
    },
    setCustomHex: (hex: string) => {
      setCustomHexState(hex);
      applyCustomAccent(hex);
    },
  };
}

