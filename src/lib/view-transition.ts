/**
 * View Transitions 圆形扩散切换。
 *
 * 用于主题色 / 明暗主题切换：以点击位置为圆心，用 clip-path 圆形把「新配色」
 * 从指针处向外铺开，形成扩散动画。
 *
 * 降级策略：
 * - 浏览器不支持 `document.startViewTransition`（如旧内核）→ 直接切换
 * - 用户开启「减少动态效果」→ 直接切换
 * - 未提供圆心坐标 → 直接切换
 */

interface CircleOrigin {
  x: number;
  y: number;
}

type ViewTransitionCapableDocument = Document & {
  startViewTransition?: (callback: () => void) => { ready: Promise<void> };
};

export function withCircleReveal(apply: () => void, origin?: CircleOrigin) {
  const reduceMotion =
    typeof window !== "undefined" &&
    window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  const doc = document as ViewTransitionCapableDocument;

  if (reduceMotion || !origin || typeof doc.startViewTransition !== "function") {
    apply();
    return;
  }

  const transition = doc.startViewTransition(apply);

  transition.ready
    .then(() => {
      const { x, y } = origin;
      // 圆心到最远角的距离，保证圆形能覆盖整屏
      const radius = Math.hypot(
        Math.max(x, window.innerWidth - x),
        Math.max(y, window.innerHeight - y),
      );

      document.documentElement.animate(
        {
          clipPath: [
            `circle(0px at ${x}px ${y}px)`,
            `circle(${radius}px at ${x}px ${y}px)`,
          ],
        },
        {
          duration: 520,
          easing: "cubic-bezier(0.4, 0, 0.2, 1)",
          pseudoElement: "::view-transition-new(root)",
        },
      );
    })
    .catch(() => {
      // 过渡被中断（如快速连点）时忽略，不影响已应用的配色
    });
}
