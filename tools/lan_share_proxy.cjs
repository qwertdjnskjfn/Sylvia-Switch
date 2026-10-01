#!/usr/bin/env node
/**
 * LAN 分享反代（白名单 + 稳定性护栏模式）—— 供他人（含手机客户端）稳定调用。
 *
 * 七层设计：
 *   1) 路径白名单：只转发 /opencode/v1/*，其余（尤其 124 条 /api/* 管理接口）一律 403。
 *   2) 独立 LAN key：调用方拿 lan-* 专用 key，本进程转发时**注入**真正的网关 key；
 *      key 支持文件 mtime 热更新（改文件即生效，无需重启）。
 *   3) 每 IP 限流：默认 60 请求/分钟，超出 429。
 *   4) 快模型白名单：只放行快模型（默认 dumate-* 与 opencode/*），慢模型直接 400 + 人话指引
 *      —— 慢模型实测 6–11 分钟，任何手机客户端都会先超时，与其干等不如立刻告知。
 *   5) **串行闸门（2026-10-01 新增）**：同一时刻只放行 1 个对话请求，其余 FIFO 排队（等待上限见下）。
 *      根因：上游引擎是**串行**的，并发请求会互相拖成 195s/240s 的排队超时
 *      —— 实测「多条并发长请求把 3.9s 的 dumate-lite 挤到 120s 超时」。串行化后每单都能正常完成。
 *   6) **瞬时故障重试（新增）**：上游返回 408/429/5xx（含网关侧超时）时自动重试一次，
 *      再失败才回错；引擎偶发长尾/瞬时掉线都能扛过第一次。
 *   7) **人话化错误（新增）**：所有失败都返回可操作的中文说明（引擎没开 / 上游太慢 / 排队已满…），
 *      绝不把裸 502 丢给朋友的客户端。
 *   另：客户端常探测的健康路径本地直答 200；单次尝试超时护栏避免无限等待。
 *
 * 用法：node tools/lan_share_proxy.cjs [port]     (57900 = 局域网/CF 隧道；8443 = Funnel)
 * 日志：~/.wb-switch/lan_proxy.log
 *
 * 环境变量（一般不用改）：
 *   LAN_SHARE_RPM=60               每 IP 每分钟上限
 *   LAN_SHARE_MODEL_FILTER=off     关闭快模型白名单
 *   LAN_SHARE_ALLOW_MODELS=dumate-,opencode/   允许的模型前缀（逗号分隔）
 *   LAN_SHARE_DEADLINE_MS=90000    单次上游尝试超时（毫秒）
 *   LAN_SHARE_INFLIGHT=1           并发闸门大小（1 = 完全串行，最稳）
 *   LAN_SHARE_QUEUE_MS=45000       排队等待上限（超出即 503 + 人话）
 *   LAN_SHARE_RETRY=1              瞬时故障重试次数
 */

const http = require('http');
const fs = require('fs');
const os = require('os');
const path = require('path');
const crypto = require('crypto');

const PORT = Number(process.argv[2]) || 57900;
const UPSTREAM_HOST = '127.0.0.1';
const UPSTREAM_PORT = 57890;
// 白名单：只放行这些前缀，其余路径一律 403（永不对转发管理面 /api/*）
const ALLOW_PREFIXES = ['/opencode/v1'];
const RATE_LIMIT_PER_MIN = Number(process.env.LAN_SHARE_RPM || 60);

// ④ 快模型白名单：默认只放行 dumate 四档（实测 2–4s）与 opencode/*（12–50s）。
//    慢模型（如 unorouter/*，实测 6–11 分钟）一律拒绝并给出指引。
const MODEL_FILTER_ON = (process.env.LAN_SHARE_MODEL_FILTER || 'on') !== 'off';
const MODEL_ALLOW_PREFIXES = String(process.env.LAN_SHARE_ALLOW_MODELS || 'dumate-,opencode/')
  .split(',')
  .map((s) => s.trim())
  .filter(Boolean);
// ⑤ 单次上游尝试超时（毫秒）；⑥ 串行闸门与排队上限；⑦ 瞬时故障重试
const ATTEMPT_TIMEOUT_MS = Number(process.env.LAN_SHARE_DEADLINE_MS || 90000);
const MAX_INFLIGHT = Math.max(1, Number(process.env.LAN_SHARE_INFLIGHT || 1));
const QUEUE_WAIT_MS = Number(process.env.LAN_SHARE_QUEUE_MS || 45000);
const RETRY_TIMES = Math.max(0, Number(process.env.LAN_SHARE_RETRY || 1));
const RETRY_DELAY_MS = 1200;
const RETRY_STATUSES = new Set([408, 429, 500, 502, 503, 504]);
// 客户端健康探测路径：本地直答，不打上游、不需 key
const HEALTH_PATHS = ['/global/health', '/health', '/healthz', '/v1/health'];
// 请求/响应体缓冲上限（对话响应很小；超上限则只透传已收部分）
const MAX_REQ_BODY = 2 * 1024 * 1024;
const MAX_RESP_BODY = 8 * 1024 * 1024;

const STORE = path.join(os.homedir(), '.wb-switch');
const LAN_KEY_FILE = path.join(STORE, 'lan_share_key.txt');
const OC_CFG_FILE = path.join(STORE, 'opencode_proxy_config.json');
const LOG_FILE = path.join(STORE, 'lan_proxy.log');
// pid 文件名带端口：允许多实例（如 57900 给局域网、8443 给 Funnel）并存，
// 否则后启动的实例会覆盖前一个的 pid 文件，导致停止脚本杀错进程。
const PID_FILE = path.join(STORE, `lan_proxy_${PORT}.pid`);

function log(line) {
  const stamp = new Date().toISOString().replace('T', ' ').slice(0, 19);
  try {
    fs.appendFileSync(LOG_FILE, `${stamp} ${line}\n`, 'utf8');
  } catch (_) {}
}

function loadLanKey() {
  try {
    const k = fs.readFileSync(LAN_KEY_FILE, 'utf8').trim();
    if (k) return k;
  } catch (_) {}
  const k = 'lan-' + crypto.randomUUID().replace(/-/g, '').slice(0, 24);
  fs.writeFileSync(LAN_KEY_FILE, k + '\n', 'utf8');
  log(`已生成新的 LAN 专用 key → ${LAN_KEY_FILE}`);
  return k;
}

function loadUpstreamKey() {
  // 转发时注入的真实网关 key（只在本机内存与上游使用，不返回给调用方）
  const cfg = JSON.parse(fs.readFileSync(OC_CFG_FILE, 'utf8'));
  if (!cfg.api_key) throw new Error('opencode_proxy_config.json 缺少 api_key');
  return cfg.api_key;
}

// LAN key 支持**热更新**：按文件 mtime 缓存，改完 lan_share_key.txt 立即生效（无需重启反代）。
// 旧实现是启动时读一次常量 → 换 key 必须重启，与「吊销只需换掉 lan_share_key.txt」的设计意图不符。
let _lanKeyCache = { mtimeMs: 0, key: '' };
function currentLanKey() {
  try {
    const st = fs.statSync(LAN_KEY_FILE);
    if (st.mtimeMs !== _lanKeyCache.mtimeMs) {
      const k = fs.readFileSync(LAN_KEY_FILE, 'utf8').trim();
      if (k) {
        _lanKeyCache = { mtimeMs: st.mtimeMs, key: k };
        log('已加载 LAN 专用 key（热更新）');
      }
    }
  } catch (_) {
    /* 文件缺失走下面兜底生成 */
  }
  if (_lanKeyCache.key) return _lanKeyCache.key;
  return loadLanKey(); // 文件不存在 → 生成并落盘
}
currentLanKey(); // 启动时确保 key 文件存在
let UPSTREAM_KEY;
try {
  UPSTREAM_KEY = loadUpstreamKey();
} catch (e) {
  console.error('FATAL: 读取上游密钥失败 -', e.message);
  process.exit(2);
}

// 每 IP 滑动窗口限流
const hits = new Map();
function rateLimited(ip) {
  const now = Date.now();
  const arr = (hits.get(ip) || []).filter((t) => now - t < 60_000);
  arr.push(now);
  hits.set(ip, arr);
  return arr.length > RATE_LIMIT_PER_MIN;
}

// ------------------------------------------------- ⑤ 并发闸门（串行化，防排队超时）
let inflight = 0;
const waiters = [];
function acquire(timeoutMs) {
  return new Promise((resolve) => {
    if (inflight < MAX_INFLIGHT) {
      inflight += 1;
      return resolve(true);
    }
    const entry = { fire: null };
    const timer = setTimeout(() => {
      const i = waiters.indexOf(entry);
      if (i >= 0) waiters.splice(i, 1);
      resolve(false); // 排队超时：名额不再给它
    }, timeoutMs);
    entry.fire = () => {
      clearTimeout(timer);
      resolve(true);
    };
    waiters.push(entry);
  });
}
function release() {
  const next = waiters.shift();
  if (next) next.fire(); // 名额直接交接，inflight 不变
  else inflight -= 1;
}

function deny(res, code, msg) {
  const body = JSON.stringify({ error: { message: msg, type: 'lan_share_proxy_error' } });
  res.writeHead(code, { 'Content-Type': 'application/json; charset=utf-8', 'Content-Length': Buffer.byteLength(body) });
  res.end(body);
}

function bearer(req) {
  const h = req.headers['authorization'] || '';
  if (h.startsWith('Bearer ')) return h.slice(7).trim();
  return (req.headers['x-api-key'] || '').trim();
}

function modelAllowed(id) {
  if (!MODEL_FILTER_ON) return true;
  return MODEL_ALLOW_PREFIXES.some((p) => id.startsWith(p));
}

function allowHint() {
  return MODEL_ALLOW_PREFIXES.map((p) => `${p}*`).join(' / ');
}

/** 从请求体里取 model 字段（取不到返回空串）。 */
function extractModel(buf) {
  if (!buf.length) return '';
  try {
    const j = JSON.parse(buf.toString('utf8'));
    return (j && typeof j.model === 'string' && j.model) || '';
  } catch (_) {
    return '';
  }
}

// ==================== ⑧ 工具调用桥（Tool-Calling Bridge，2026-10-01）====================
// 动机：上游（DuMate 引擎）的 message API **只接受文本 parts**，没有「客户端自定义函数」入口
//（契约实测：其 tools 字段是 {toolName: boolean} 的**内置工具开关**）。
// 而 Operit / Cline / WorkBuddy 这类客户端走的是 OpenAI 风格 tools → tool_calls 往返（客户端执行工具）。
// 桥做双向翻译，**只在该请求带 tools 时启用**（不带 tools = 零行为变化）：
//   ① 请求侧：把 tools 定义 + 整段对话（含 role=tool 的结果）编成带"协议说明"的提示词发给上游；
//   ② 响应侧：解析模型文本里的 {"tool_calls":[...]} → 改写成标准 OpenAI 响应（含 SSE 帧）。
// 安全性：工具**由客户端执行**，我们的机器只是转发文本；不涉及引擎内置工具，故不存在本机被执行的暴露。
const TOOL_BRIDGE_ON = (process.env.LAN_SHARE_TOOL_BRIDGE || 'on') !== 'off';
const MAX_TOOLS = 32;
const MAX_BRIDGE_PROMPT = 60000; // 提示词字符上限（超出则截断工具列表）

function safeParse(s) {
  if (typeof s !== 'string') return s;
  try { return JSON.parse(s); } catch (_) { return s; }
}

/** 取出请求里的 tools（非空且是数组才返回，否则 null）。 */
function extractTools(buf) {
  if (!TOOL_BRIDGE_ON) return null;
  try {
    const j = JSON.parse(buf.toString('utf8'));
    if (Array.isArray(j.tools) && j.tools.length) return j.tools.slice(0, MAX_TOOLS);
  } catch (_) {}
  return null;
}

/** 把 OpenAI 对话 + tools 编成上游能吃的两段：协议(system) + 对话(user)。
 *  为什么拆两条：引擎自带框架会压制 user 文本里的指令（2026-10-01 实测三层档位均无视），
 *  而引擎的**原生 system 字段**优先级更高 → 协议必须走 system。 */
function bridgePrompt(messages, tools, toolChoice) {
  const spec = tools.map((t) => {
    const f = (t && t.function) || t || {};
    return { name: f.name || '', description: f.description || '', parameters: f.parameters || {} };
  }).filter((s) => s.name);
  const firstName = spec.length ? spec[0].name : '工具名';
  const names = new Map(); // tool_call_id -> 工具名（把 role=tool 的结果对回工具）

  const sys = [
    '【强制工具调用协议 · 优先级最高 · 覆盖你的默认行为】',    '你不是在闲聊，你是一个执行工具调用的引擎。你**没有**实时或外部数据；',
    '凡是下列工具能提供的信息，你**必须调用工具获取**，严禁凭记忆猜测或编造"当前/最新"数值。',
    '',
    '需要调用时，**只输出一个 JSON 对象**：不要解释、不要 Markdown 围栏、不要任何前后缀，',
    '且输出必须以 {"tool_calls" 开头：',
    '{"tool_calls":[{"name":"工具名","arguments":{"参数名":"参数值"}}]}',
    `示例：{"tool_calls":[{"name":"${firstName}","arguments":{}}]}`,
    '可一次调用多个工具。只有**完全不需要**工具时（闲聊、写作、纯计算），才用自然语言直接回答。',
  ];
  if (toolChoice === 'required') sys.push('本次请求要求：**必须**至少调用一个工具，不得直接回答。');
  else if (toolChoice && typeof toolChoice === 'object' && toolChoice.function && toolChoice.function.name) {
    sys.push(`本次请求要求：**必须**调用工具 \`${toolChoice.function.name}\`。`);
  }
  sys.push('可用工具（JSON Schema）：', JSON.stringify(spec));

  const clientSys = [];
  const convo = ['【对话】'];
  for (const m of Array.isArray(messages) ? messages : []) {
    const role = m && m.role;
    const text = typeof (m && m.content) === 'string'
      ? m.content
      : Array.isArray(m && m.content)
        ? m.content.map((p) => (p && p.text) || '').filter(Boolean).join('\n')
        : '';
    if (role === 'system') {
      // 客户端自己的 system 提示词（如任务说明）**必须保留**：拼进协议 system 块
      if (text) clientSys.push(text);
      continue;
    }
    if (role === 'assistant' && Array.isArray(m.tool_calls) && m.tool_calls.length) {
      const calls = m.tool_calls.map((tc) => {
        if (tc && tc.id) names.set(tc.id, (tc.function || {}).name || tc.name || '');
        return { name: (tc && (tc.function || {}).name) || (tc && tc.name) || '', arguments: safeParse(tc && (tc.function || {}).arguments) };
      });
      convo.push('assistant（已请求调用工具）: ' + JSON.stringify({ tool_calls: calls }));
      if (text) convo.push('assistant: ' + text);
    } else if (role === 'tool') {
      const nm = names.get(m.tool_call_id) || m.name || '工具';
      convo.push(`tool（${nm} 的返回结果）: ${text}`);
    } else {
      convo.push(`${role || 'user'}: ${text}`);
    }
  }
  convo.push('', '现在请作答：若需调用工具，**只输出那个 JSON 对象**（以 {"tool_calls" 开头）。');

  if (clientSys.length) {
    sys.push('', '【客户端原始系统设定（务必遵守）】', ...clientSys);
  }

  let user = convo.join('\n');
  if (user.length > MAX_BRIDGE_PROMPT) user = user.slice(0, MAX_BRIDGE_PROMPT) + '\n（对话过长已截断）';
  return { system: sys.join('\n'), user };
}

/** 生成发往上游的请求体（协议走 system、对话走 user；去掉 tools/tool_choice）。 */
function applyToolBridge(reqBody, tools) {
  const j = JSON.parse(reqBody.toString('utf8'));
  const { system, user } = bridgePrompt(j.messages, tools, j.tool_choice);
  const out = Object.assign({}, j);
  delete out.tools;
  delete out.tool_choice;
  delete out.parallel_tool_calls;
  out.messages = [{ role: 'system', content: system }, { role: 'user', content: user }];
  return Buffer.from(JSON.stringify(out), 'utf8');
}

/** 从 OCR 文本里揪出 tool_calls（支持裸 JSON / 代码块 / 正文里嵌 JSON）。 */
function extractToolCalls(text) {
  const t = String(text || '');
  if (!t) return null;
  const candidates = [t.trim()];
  const fence = /```(?:json)?\s*([\s\S]*?)```/gi;
  let m;
  while ((m = fence.exec(t))) candidates.push(m[1].trim());
  const idx = t.indexOf('"tool_calls"');
  if (idx >= 0) {
    const start = t.lastIndexOf('{', idx);
    if (start >= 0) {
      let depth = 0;
      for (let i = start; i < t.length; i += 1) {
        if (t[i] === '{') depth += 1;
        else if (t[i] === '}') {
          depth -= 1;
          if (depth === 0) { candidates.push(t.slice(start, i + 1)); break; }
        }
      }
    }
  }
  for (const c of candidates) {
    try {
      const j = JSON.parse(c);
      const arr = j && (j.tool_calls || j.toolCalls);
      if (Array.isArray(arr) && arr.length) {
        const calls = arr.map((x, i) => {
          const f = (x && x.function) || x || {};
          let args = f.arguments;
          if (args == null) args = {};
          if (typeof args !== 'string') args = JSON.stringify(args);
          return {
            id: `call_bridge_${Date.now().toString(36)}_${i}`,
            type: 'function',
            function: { name: f.name || x.name || '', arguments: args },
          };
        }).filter((x) => x.function.name);
        if (calls.length) return calls;
      }
    } catch (_) {}
  }
  return null;
}

/** 把上游 JSON 响应改写成 OpenAI 的 tool_calls 响应。 */
function rewriteToToolCalls(bodyBuf, calls) {
  const j = JSON.parse(bodyBuf.toString('utf8'));
  const c0 = (j.choices && j.choices[0]) || {};
  j.choices = [
    Object.assign({}, c0, {
      finish_reason: 'tool_calls',
      message: { role: 'assistant', content: null, tool_calls: calls },
    }),
  ];
  return Buffer.from(JSON.stringify(j), 'utf8');
}

/** 生成 tool_calls 的 SSE 帧（客户端流式场景）。 */
function sseToolCalls(model, calls) {
  const id = `chatcmpl-bridge-${Date.now().toString(36)}`;
  const created = Math.floor(Date.now() / 1000);
  const frame = (delta, finish) =>
    'data: ' + JSON.stringify({
      id, object: 'chat.completion.chunk', created, model,
      choices: [{ index: 0, delta, finish_reason: finish || null }],
    }) + '\n\n';
  let out = frame({
    role: 'assistant',
    tool_calls: calls.map((c, i) => ({ index: i, id: c.id, type: 'function', function: { name: c.function.name, arguments: '' } })),
  }, null);
  out += frame({ tool_calls: calls.map((c, i) => ({ index: i, function: { arguments: c.function.arguments } })) }, null);
  out += frame({}, 'tool_calls');
  out += 'data: [DONE]\n\n';
  return Buffer.from(out, 'utf8');
}

/** 从聚合后的 SSE 文本里取出正文（上游 stream 是聚合后合成的帧）。 */
function textFromSse(raw) {
  let text = '';
  for (const line of String(raw || '').split('\n')) {
    if (!line.startsWith('data: ')) continue;
    const payload = line.slice(6).trim();
    if (!payload || payload === '[DONE]') continue;
    try {
      const d = JSON.parse(payload);
      const c = ((d.choices || [])[0] || {}).delta || {};
      if (c.content) text += c.content;
    } catch (_) {}
  }
  return text;
}

/** 上游单次调用（缓冲整包，便于重试与人话化改写）。 */
function sendOnce({ method, url, headers, body }) {
  return new Promise((resolve, reject) => {
    const req = http.request(
      { host: UPSTREAM_HOST, port: UPSTREAM_PORT, method, path: url, headers },
      (up) => {
        const bufs = [];
        let n = 0;
        up.on('data', (c) => {
          n += c.length;
          if (n <= MAX_RESP_BODY) bufs.push(c);
        });
        up.on('end', () =>
          resolve({
            status: up.statusCode || 502,
            headers: up.headers,
            body: Buffer.concat(bufs),
            tooBig: n > MAX_RESP_BODY,
          })
        );
        up.on('error', reject);
      }
    );
    req.setTimeout(ATTEMPT_TIMEOUT_MS, () => req.destroy(new Error('attempt-timeout')));
    req.on('error', reject);
    req.end(body);
  });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** ⑦ 把上游失败翻译成**可操作**的中文（绝不把裸 502 丢给客户端）。 */
function friendlyError(status, text) {
  const t = String(text || '');
  if (/未发现运行中的|引擎/.test(t)) {
    return '我电脑上的 DuMate 引擎当前没在运行（客户端可能被关了）。\n'
      + '请在微信上喊我一声让我打开它；或临时改用 opencode/* 的模型试一次。';
  }
  if (status === 504 || /attempt-timeout|deadline/.test(t)) {
    return `上游这次太慢了（我已等 ${Math.round(ATTEMPT_TIMEOUT_MS / 1000)} 秒并自动重试过）。\n`
      + '建议：改用 dumate-lite（实测 2–4 秒）；若在生成很长的内容，请把一次要的量拆小些。';
  }
  if (status === 429) return '请求太频繁了（上游限流），等几秒再试。';
  if (status === 503) return '服务忙（前面还有请求在处理），等几秒再试。';
  return `上游返回 ${status}。${t.slice(0, 160)}\n建议稍后重试一次；若持续失败请把这句话发给我。`;
}

const server = http.createServer((req, res) => {
  const started = Date.now();
  const ip = (req.socket.remoteAddress || '').replace('::ffff:', '');
  const url = req.url || '/';
  const bare = url.split('?')[0];

  // ---- 健康探测：本地直答（不打上游、不校验 key）----
  if (req.method === 'GET' && HEALTH_PATHS.includes(bare)) {
    const body = JSON.stringify({ ok: true, healthy: true, status: 'ok', service: 'lan-share-proxy' });
    res.writeHead(200, { 'Content-Type': 'application/json; charset=utf-8', 'Content-Length': Buffer.byteLength(body) });
    res.end(body);
    log(`200 ${ip} GET ${url} health(local)`);
    return;
  }

  if (!ALLOW_PREFIXES.some((p) => url === p || url.startsWith(p + '/') || url.startsWith(p + '?'))) {
    log(`403 ${ip} ${req.method} ${url}`);
    return deny(res, 403, `路径不在白名单内（本反代只开放 ${ALLOW_PREFIXES.join(', ')}）`);
  }
  if (bearer(req) !== currentLanKey()) {
    log(`401 ${ip} ${req.method} ${url}`);
    return deny(res, 401, '无效的 API Key（请使用本机分配的 lan-* 专用 key；整串复制，别手打）');
  }
  if (rateLimited(ip)) {
    log(`429 ${ip} ${req.method} ${url}`);
    return deny(res, 429, `请求过于频繁（每 IP 上限 ${RATE_LIMIT_PER_MIN} 次/分钟），等一分钟再试。`);
  }

  // ---- 读请求体（有上限），用于模型白名单判定 ----
  const chunks = [];
  let size = 0;
  let overflow = false;
  req.on('data', (c) => {
    size += c.length;
    if (size <= MAX_REQ_BODY) chunks.push(c);
    else overflow = true;
  });
  req.on('error', () => {});
  req.on('end', async () => {
    if (overflow) {
      log(`413 ${ip} ${req.method} ${url}`);
      return deny(res, 413, '请求体过大（超过 2MB）');
    }
    const reqBody = Buffer.concat(chunks);
    const model = extractModel(reqBody);
    const isChat = req.method === 'POST' && /\/chat\/completions$/.test(bare);
    const isModels = req.method === 'GET' && /\/models$/.test(bare);

    // ---- ④ 快模型白名单：慢模型直接拒绝（否则对方会等到超时）----
    if (model && !modelAllowed(model)) {
      log(`400 ${ip} ${req.method} ${url} model=${model} not-allowed`);
      return deny(
        res,
        400,
        `该模型未对本分享开放：${model}\n` +
          `对外仅开放：${allowHint()}\n` +
          `（慢模型实测要 6–11 分钟，手机客户端必然超时；请改用 dumate-lite，实测 3 秒级）`
      );
    }

    // ⑧ 工具调用桥：仅当请求带 tools 时，把请求体换成「协议提示词」版本
    const tools = isChat ? extractTools(reqBody) : null;
    let sendBody = reqBody;
    if (tools) {
      try {
        sendBody = applyToolBridge(reqBody, tools);
        log(`tool-bridge: 已启用（tools=${tools.length}，发上游提示词 ${sendBody.length} 字节）model=${model}`);
      } catch (e) {
        log(`tool-bridge: 构造失败，按原样转发 —— ${e.message}`);
        sendBody = reqBody;
      }
    }

    const headers = Object.assign({}, req.headers);
    headers['authorization'] = 'Bearer ' + UPSTREAM_KEY; // 注入真 key
    delete headers['x-api-key']; // 避免调用方自带 x-api-key 混淆
    headers['host'] = `${UPSTREAM_HOST}:${UPSTREAM_PORT}`;
    headers['content-length'] = String(sendBody.length);

    // ---- 对话请求：⑤ 串行闸门 + ⑥ 瞬时重试 + ⑦ 人话化错误 ----
    if (isChat) {
      const slot = await acquire(QUEUE_WAIT_MS);
      if (!slot) {
        log(`503 ${ip} ${req.method} ${url} queue-full model=${model}`);
        return deny(res, 503, '服务忙：前面的请求还没处理完，排队也超时了。请等几秒再发一次（别连点）。');
      }
      try {
        let r = null;
        let lastErr = '';
        for (let attempt = 0; attempt <= RETRY_TIMES; attempt += 1) {
          if (attempt > 0) {
            await sleep(RETRY_DELAY_MS);
            log(`RETRY ${url} attempt=${attempt + 1} model=${model}`);
          }
          try {
            r = await sendOnce({ method: req.method, url, headers, body: sendBody });
          } catch (e) {
            lastErr = e.message;
            r = null;
            continue; // 网络层失败 → 直接进入下一轮（若还有）
          }
          if (!RETRY_STATUSES.has(r.status)) break; // 正常或业务错误 → 不重试
          lastErr = `HTTP ${r.status} ${r.body.toString('utf8').slice(0, 120)}`;
        }
        const elapsed = Date.now() - started;
        if (!r) {
          log(`504 ${ip} ${req.method} ${url} ${elapsed}ms model=${model} err=${lastErr}`);
          return deny(res, 504, friendlyError(504, lastErr));
        }
        if (RETRY_STATUSES.has(r.status)) {
          log(`${r.status} ${ip} ${req.method} ${url} ${elapsed}ms model=${model} err=${lastErr}`);
          return deny(res, 502, friendlyError(r.status, r.body.toString('utf8')));
        }
        // ⑧ 启用工具桥时，尝试把模型回复改写成 OpenAI 的 tool_calls
        if (tools && r.status === 200) {
          const ctype = String(r.headers['content-type'] || '');
          if (/event-stream/.test(ctype)) {
            const calls = extractToolCalls(textFromSse(r.body.toString('utf8')));
            if (calls) {
              const buf = sseToolCalls(model, calls);
              res.writeHead(200, {
                'Content-Type': 'text/event-stream; charset=utf-8',
                'Content-Length': buf.length,
                'Cache-Control': 'no-cache',
              });
              res.end(buf);
              log(`200 ${ip} POST ${url} ${elapsed}ms tool-bridge(stream) tools=${tools.length} calls=${calls.length} model=${model}`);
              return;
            }
          } else {
            let text = '';
            try {
              const parsed = JSON.parse(r.body.toString('utf8'));
              const msg = ((parsed.choices || [])[0] || {}).message || {};
              text = typeof msg.content === 'string' ? msg.content : '';
            } catch (_) {}
            const calls = extractToolCalls(text);
            if (calls) {
              const buf = rewriteToToolCalls(r.body, calls);
              const h = Object.assign({}, r.headers);
              delete h['content-length'];
              res.writeHead(200, h);
              res.end(buf);
              log(`200 ${ip} POST ${url} ${elapsed}ms tool-bridge tools=${tools.length} calls=${calls.length} model=${model}`);
              return;
            }
          }
          log(`tool-bridge: 本轮模型直接回答（未调用工具）tools=${tools.length} model=${model}`);
        }
        const out = Object.assign({}, r.headers);
        delete out['content-length']; // 可能被改写，长度交回 node 重算
        res.writeHead(r.status, out);
        res.end(r.body);
        log(`${r.status} ${ip} ${req.method} ${url} ${elapsed}ms model=${model}`);
      } finally {
        release();
      }
      return;
    }

    // ---- 模型目录：过滤后再回（对外只展示可用模型）----
    if (isModels) {
      try {
        const r = await sendOnce({ method: req.method, url, headers, body: reqBody });
        let out = r.body;
        let kept = null;
        if (!r.tooBig) {
          try {
            const j = JSON.parse(out.toString('utf8'));
            if (Array.isArray(j.data)) {
              const before = j.data.length;
              j.data = j.data.filter((m) => modelAllowed(String((m && m.id) || '')));
              kept = j.data.length;
              out = Buffer.from(JSON.stringify(j), 'utf8');
              log(`models-filter ${before} → ${kept}（对外只放行 ${allowHint()}）`);
            }
          } catch (_) {
            /* 解析失败则原样透传 */
          }
        }
        const h = Object.assign({}, r.headers);
        delete h['content-length'];
        res.writeHead(r.status, h);
        res.end(out);
        log(`${r.status} ${ip} ${req.method} ${url} ${Date.now() - started}ms models=${kept === null ? 'all' : kept}`);
      } catch (e) {
        log(`504 ${ip} ${req.method} ${url} models-error=${e.message}`);
        deny(res, 504, '读取模型目录超时，请稍后重试。');
      }
      return;
    }

    // ---- 其它路径：直连转发 ----
    try {
      const r = await sendOnce({ method: req.method, url, headers, body: reqBody });
      const out = Object.assign({}, r.headers);
      delete out['content-length'];
      res.writeHead(r.status, out);
      res.end(r.body);
      log(`${r.status} ${ip} ${req.method} ${url} ${Date.now() - started}ms`);
    } catch (e) {
      log(`504 ${ip} ${req.method} ${url} other-error=${e.message}`);
      deny(res, 504, '上游超时，请稍后重试。');
    }
  });
});

server.on('error', (e) => {
  if (e.code === 'EADDRINUSE') {
    console.error(`端口 ${PORT} 已被占用（反代可能已在运行）。`);
    process.exit(3);
  }
  console.error('监听失败:', e.message);
  process.exit(4);
});

server.listen(PORT, '0.0.0.0', () => {
  fs.writeFileSync(PID_FILE, String(process.pid), 'utf8');
  const ips = [];
  for (const list of Object.values(os.networkInterfaces())) {
    for (const i of list || []) {
      if (i.family === 'IPv4' && !i.internal) ips.push(i.address);
    }
  }
  const lines = [
    `LAN 分享反代已启动：端口 ${PORT}（pid ${process.pid}）`,
    `白名单：${ALLOW_PREFIXES.join(', ')}；限流：${RATE_LIMIT_PER_MIN} 次/分钟/IP`,
    `模型过滤：${MODEL_FILTER_ON ? `开（只放行 ${allowHint()}）` : '关'}；健康路径本地直答：${HEALTH_PATHS.join(' ')}`,
    `稳定性：串行闸门 ${MAX_INFLIGHT} 并发（排队上限 ${Math.round(QUEUE_WAIT_MS / 1000)}s）；单次上游超时 `
      + `${Math.round(ATTEMPT_TIMEOUT_MS / 1000)}s；瞬时重试 ${RETRY_TIMES} 次`,
    `LAN key 文件：${LAN_KEY_FILE}`,
    `其它设备可用的地址：`,
    ...ips.map((a) => `    http://${a}:${PORT}/opencode/v1`),
  ];
  lines.forEach((l) => console.log(l));
  log('START ' + lines.slice(0, 4).join(' | '));
});
