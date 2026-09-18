import React from "react";
import { Check, Monitor, Moon, Pipette, Sun } from "lucide-react";

import { useTheme, type ThemePreference } from "@/lib/theme";
import {
  ACCENT_PRESETS,
  contrastOnHex,
  useAccent,
  type AccentId,
} from "@/lib/accent";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

interface ThemeSwitcherProps {
  className?: string;
}

/**
 * 侧边栏常驻外观与主题色快捷切换面板。
 * 支持：
 * 1. 浅色 / 跟随系统 / 深色 三段式快速切换
 * 2. 6 种预设强调色（翡翠、天青、靛蓝、紫罗兰、玫红、琥珀）
 * 3. 任意自定义 Hex 颜色选择器
 * 4. 伴随 View Transitions 圆形扩散动效
 */
export function SidebarThemeSwitcher({ className }: ThemeSwitcherProps) {
  const { theme, setTheme } = useTheme();
  const { accent, customHex, setAccent, setCustomHex } = useAccent();

  const handleThemeClick = (
    pref: ThemePreference,
    e: React.MouseEvent<HTMLButtonElement>,
  ) => {
    if (pref === theme) return;
    setTheme(pref, { x: e.clientX, y: e.clientY });
  };

  const handleAccentClick = (
    id: AccentId,
    e: React.MouseEvent<HTMLButtonElement>,
  ) => {
    if (id === accent) return;
    setAccent(id, { x: e.clientX, y: e.clientY });
  };

  const isCustom = accent === "custom";

  return (
    <div
      className={cn(
        "rounded-xl border border-sidebar-border/60 bg-sidebar-accent/30 p-2 text-xs",
        className,
      )}
    >
      {/* 顶部：明暗模式切换小按钮组 */}
      <div className="flex items-center justify-between gap-1 rounded-lg bg-background/50 p-0.5 shadow-xs">
        <Tooltip>
          <TooltipTrigger asChild>
            <button
              type="button"
              aria-label="浅色模式"
              onClick={(e) => handleThemeClick("light", e)}
              className={cn(
                "flex h-6 flex-1 items-center justify-center rounded-md transition-all",
                theme === "light"
                  ? "bg-background font-medium text-foreground shadow-xs"
                  : "text-muted-foreground hover:text-foreground",
              )}
            >
              <Sun className="size-3.5" />
            </button>
          </TooltipTrigger>
          <TooltipContent side="top">浅色模式</TooltipContent>
        </Tooltip>

        <Tooltip>
          <TooltipTrigger asChild>
            <button
              type="button"
              aria-label="跟随系统"
              onClick={(e) => handleThemeClick("system", e)}
              className={cn(
                "flex h-6 flex-1 items-center justify-center rounded-md transition-all",
                theme === "system"
                  ? "bg-background font-medium text-foreground shadow-xs"
                  : "text-muted-foreground hover:text-foreground",
              )}
            >
              <Monitor className="size-3.5" />
            </button>
          </TooltipTrigger>
          <TooltipContent side="top">跟随系统</TooltipContent>
        </Tooltip>

        <Tooltip>
          <TooltipTrigger asChild>
            <button
              type="button"
              aria-label="深色模式"
              onClick={(e) => handleThemeClick("dark", e)}
              className={cn(
                "flex h-6 flex-1 items-center justify-center rounded-md transition-all",
                theme === "dark"
                  ? "bg-background font-medium text-foreground shadow-xs"
                  : "text-muted-foreground hover:text-foreground",
              )}
            >
              <Moon className="size-3.5" />
            </button>
          </TooltipTrigger>
          <TooltipContent side="top">深色模式</TooltipContent>
        </Tooltip>
      </div>

      {/* 底部：主题颜色色块列表 */}
      <div
        className="mt-2 flex items-center justify-between gap-1 px-0.5"
        role="radiogroup"
        aria-label="主题色快捷选择"
      >
        {ACCENT_PRESETS.map((preset) => {
          const active = preset.id === accent;
          return (
            <Tooltip key={preset.id}>
              <TooltipTrigger asChild>
                <button
                  type="button"
                  role="radio"
                  aria-checked={active}
                  aria-label={preset.label}
                  onClick={(e) => handleAccentClick(preset.id, e)}
                  style={{ backgroundColor: preset.swatch }}
                  className={cn(
                    "grid size-5.5 place-items-center rounded-full transition-transform hover:scale-115 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
                    active
                      ? "ring-2 ring-foreground/40 ring-offset-1 ring-offset-background"
                      : "ring-1 ring-foreground/15 hover:ring-foreground/30",
                  )}
                >
                  {active && (
                    <Check
                      className="size-3"
                      strokeWidth={3}
                      style={{ color: preset.check }}
                    />
                  )}
                </button>
              </TooltipTrigger>
              <TooltipContent side="top">{preset.label}</TooltipContent>
            </Tooltip>
          );
        })}

        {/* 自定义颜色取色器 */}
        <Tooltip>
          <TooltipTrigger asChild>
            <label
              role="radio"
              aria-checked={isCustom}
              aria-label="自定义主题色"
              style={{ backgroundColor: customHex }}
              className={cn(
                "relative grid size-5.5 cursor-pointer place-items-center overflow-hidden rounded-full transition-transform hover:scale-115 focus-within:ring-2 focus-within:ring-ring",
                isCustom
                  ? "ring-2 ring-foreground/40 ring-offset-1 ring-offset-background"
                  : "ring-1 ring-foreground/15 hover:ring-foreground/30",
              )}
            >
              <input
                type="color"
                value={customHex}
                onChange={(e) => setCustomHex(e.target.value)}
                className="absolute inset-0 size-full cursor-pointer opacity-0"
                aria-label="选择自定义主题色"
              />
              {isCustom ? (
                <Check
                  className="size-3"
                  strokeWidth={3}
                  style={{ color: contrastOnHex(customHex) }}
                />
              ) : (
                <Pipette className="size-3 text-white drop-shadow-[0_1px_1px_rgba(0,0,0,0.6)]" />
              )}
            </label>
          </TooltipTrigger>
          <TooltipContent side="top">自定义颜色</TooltipContent>
        </Tooltip>
      </div>
    </div>
  );
}
