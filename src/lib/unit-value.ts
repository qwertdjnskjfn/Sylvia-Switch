// 带单位后缀的数值字符串解析（'5M' → 5000000）。
//
// 面向「把外部展示值还原成可运算数值」的场景（如 '154.20M'、'1,542.1万'）。
// 十进制单位走整数移位：先拼出全部有效数字，再按 10 的幂平移小数点，
// 避免 154.2 * 1e6 = 154200000.00000001 一类的浮点误差；其余单位
// （Ki/Mi/Gi/Ti 等非 10 的幂）按普通乘法处理。无法解析时返回 null
// 而不是抛错，便于在渲染层安全降级。
//
// 扩展：registerUnitSuffix('kb', 1000) 即可新增后缀；值为 10 的整数次幂
// 时自动走精确移位路径，否则按乘数处理。

/** 10 的整数次幂单位（精确路径）：后缀小写 → 指数。 */
const EXPONENT_SUFFIXES: Record<string, number> = {
  k: 3, // 千
  m: 6, // 百万
  b: 9, // 十亿
  t: 12, // 万亿
  万: 4,
  亿: 8,
};

/** 非 10 的幂单位（乘数路径）：后缀小写 → 倍数。 */
const MULTIPLIER_SUFFIXES: Record<string, number> = {
  ki: 1024,
  mi: 1024 ** 2,
  gi: 1024 ** 3,
  ti: 1024 ** 4,
};

/**
 * 注册/覆盖一个单位后缀。`value` 为 10 的整数次幂（如 10000）时走精确
 * 移位路径，否则按乘数处理（如 1024）。后缀不区分大小写。
 */
export function registerUnitSuffix(suffix: string, value: number): void {
  const key = suffix.trim().toLowerCase();
  if (!key) throw new Error("单位后缀不能为空");
  if (!Number.isFinite(value) || value <= 0) {
    throw new Error(`非法单位倍数: ${value}`);
  }
  const isPowerOfTen =
    Number.isInteger(value) && value > 1 && Math.log10(value) % 1 === 0;
  if (isPowerOfTen) {
    delete MULTIPLIER_SUFFIXES[key];
    EXPONENT_SUFFIXES[key] = Math.round(Math.log10(value));
  } else {
    delete EXPONENT_SUFFIXES[key];
    MULTIPLIER_SUFFIXES[key] = value;
  }
}

/**
 * 把带单位后缀的数字字符串/数值还原为可运算的 number。
 *
 * 支持：纯数字（'1234' / 1234）、小数（'.5M'）、正负号、千分位逗号
 * （'1,542.1M'）、数字与单位间空白（'10 M'）、十进制单位（k/m/b/t，
 * 大小写不敏感）、中文单位（万/亿）、二进制单位（Ki/Mi/Gi/Ti）。
 * 解析失败（空串、无数字、未知后缀等）返回 null。
 */
export function parseUnitValue(input: string | number | null | undefined): number | null {
  if (typeof input === "number") {
    return Number.isFinite(input) ? input : null;
  }
  if (input == null) return null;

  const text = String(input).trim().replace(/[,，_\s]/g, "");
  if (!text) return null;

  const match = /^([+-]?)(\d+)?(?:\.(\d+))?(.*)$/.exec(text);
  if (!match) return null;
  const sign = match[1] === "-" ? -1 : 1;
  const intPart = match[2] ?? "";
  const fracPart = match[3] ?? "";
  if (!intPart && !fracPart) return null;

  const digits = `${intPart}${fracPart}`;
  const scale = fracPart.length;
  const unit = match[4].toLowerCase();

  let value: number | null;
  if (!unit) {
    value = Number(digits) / 10 ** scale;
  } else {
    const exponent = EXPONENT_SUFFIXES[unit];
    if (exponent !== undefined) {
      value = shiftDecimal(digits, exponent - scale);
    } else {
      const multiplier = MULTIPLIER_SUFFIXES[unit];
      if (multiplier === undefined) return null; // 未知后缀
      value = (Number(digits) / 10 ** scale) * multiplier;
    }
  }
  return sign * value;
}

/** 按 10 的幂平移整数数字串的小数点（shift >= 0 时结果为精确整数）。 */
function shiftDecimal(digits: string, shift: number): number {
  if (shift >= 0) {
    return Number(digits) * 10 ** shift;
  }
  return Number(digits) / 10 ** -shift;
}
