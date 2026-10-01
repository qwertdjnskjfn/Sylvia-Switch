#!/usr/bin/env node
/**
 * LAN 分享反代（白名单模式）—— 只为「局域网/公网临时分享给他人调用」设计。
 *
 * 安全与体验设计（五层）：
 *   1) 路径白名单：只转发 /opencode/v1/*，其余（尤其 124 条 /api/* 管理接口）一律 403，
 *      上游根本收不到请求 —— 管理面仍只在本机回环。
 *   2) 独立 LAN key：调用方拿的是 lan-* 专用 key，不是网关真 key。
 *      key 校验通过后由本进程**注入**真正的网关 key 转发 →
 *      网关 key 永不离开本机，吊销只需换掉 lan_share_key.txt（支持热更新，无需重启）。
 *   3) 每 IP 限流：默认 60 请求/分钟，超出 429（防被拿去做批量）。
 *   4) **快模型白名单**（2026-10-01 新增）：对外只放行「响应快」的模型，
 *      慢模型直接 400 + 人话指引 —— 因为实测慢模型要 6–11 分钟，任何手机客户端都会先超时，
 *      与其让对方等到超时，不如立刻告诉他换哪个。
 *   5) **超时护栏**（2026-10-01 新增）：上游超过 `LAN_SHARE_DEADLINE_MS` 仍未开始返回时，
 *      本地直接 504 + 人话，避免客户端无限等待；已在流式输出中的连接不打断。
 *   另：客户端常探测的 /global/health 等健康路径**本地直答 200**（不打上游），
 *      避免客户端据此误判 provider 不可用。
 *
 * 只监听 0.0.0.0 的**一个端口**，随开随关：关掉进程 = 分享立即终止。
 *
 * 用法：node tools/lan_share_proxy.cjs [port]     (默认 57900；Funnel 用 8443)
 * 日志：~/.wb-switch/lan_proxy.log
 *
 * 可用环境变量（一般不用改）：
 *   LAN_SHARE_RPM=60                     每 IP 每分钟请求上限
 *   LAN_SHARE_MODEL_FILTER=off           关闭快模型白名单（默认 on）
 *   LAN_SHARE_ALLOW_MODELS=dumate-,opencode/   允许的模型名前缀（逗号分隔）
 *   LAN_SHARE_DEADLINE_MS=150000         上游首字节超时（毫秒）
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
// ⑤ 超时护栏：上游在此时限内没开始返回 → 504（默认 150s）
const REQUEST_DEADLINE_MS = Number(process.env.LAN_SHARE_DEADLINE_MS || 150000);
// 客户端健康探测路径：本地直答，不打上游、不需 key
const HEALTH_PATHS = ['/global/health', '/health', '/healthz', '/v1/health'];
// 请求/响应体缓冲上限（模型目录与对话请求都很小；超出则不做过滤，透传）
const MAX_REQ_BODY = 2 * 1024 * 1024;
const MAX_RESP_BODY = 512 * 1024;

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
    return deny(res, 401, '无效的 API Key（请使用本机分配的 lan-* 专用 key）');
  }
  if (rateLimited(ip)) {
    log(`429 ${ip} ${req.method} ${url}`);
    return deny(res, 429, `请求过于频繁（每 IP 上限 ${RATE_LIMIT_PER_MIN} 次/分钟）`);
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
  req.on('end', () => {
    if (overflow) {
      log(`413 ${ip} ${req.method} ${url}`);
      return deny(res, 413, '请求体过大（超过 2MB）');
    }
    const reqBody = Buffer.concat(chunks);
    const model = extractModel(reqBody);

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

    const headers = Object.assign({}, req.headers);
    headers['authorization'] = 'Bearer ' + UPSTREAM_KEY; // 注入真 key
    delete headers['x-api-key']; // 避免调用方自带 x-api-key 混淆
    headers['host'] = `${UPSTREAM_HOST}:${UPSTREAM_PORT}`;
    headers['content-length'] = String(reqBody.length);

    const isModels = req.method === 'GET' && /\/models$/.test(bare);
    let finished = false;
    let guard = null;

    const proxied = http.request(
      { host: UPSTREAM_HOST, port: UPSTREAM_PORT, method: req.method, path: url, headers },
      (up) => {
        // ---- ⑤ 超时护栏：已开始返回则撤掉 ----
        if (guard) clearTimeout(guard);

        // 模型目录：过滤后再回（对外只展示可用模型）
        if (isModels) {
          const bufs = [];
          let n = 0;
          let big = false;
          up.on('data', (c) => {
            n += c.length;
            if (n <= MAX_RESP_BODY) bufs.push(c);
            else big = true;
          });
          up.on('end', () => {
            if (finished) return;
            finished = true;
            let out = Buffer.concat(bufs);
            let kept = null;
            if (!big) {
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
            const h = Object.assign({}, up.headers);
            delete h['content-length']; // 内容已改，长度交回 node 重算
            res.writeHead(up.statusCode || 200, h);
            res.end(out);
            log(`${up.statusCode} ${ip} ${req.method} ${url} ${Date.now() - started}ms models=${kept === null ? 'all' : kept}`);
          });
          up.on('error', () => {
            if (!finished && !res.headersSent) deny(res, 502, '读取上游模型目录失败');
          });
          return;
        }

        res.writeHead(up.statusCode || 502, up.headers);
        up.pipe(res); // 逐块转发：SSE / chunked 原样透传
        up.on('end', () => {
          log(
            `${up.statusCode} ${ip} ${req.method} ${url} ${Date.now() - started}ms` +
              (model ? ` model=${model}` : '')
          );
        });
      }
    );

    // 超时护栏：上游迟迟不开始返回（慢模型 / 排队）→ 本地立刻 504，不让对方干等
    guard = setTimeout(() => {
      if (finished || res.headersSent) return; // 已在流式输出 → 不打断
      finished = true;
      log(`504 ${ip} ${req.method} ${url} deadline=${REQUEST_DEADLINE_MS}ms model=${model || '-'}`);
      try {
        proxied.destroy(new Error('deadline exceeded'));
      } catch (_) {}
      deny(
        res,
        504,
        `上游超时：${Math.round(REQUEST_DEADLINE_MS / 1000)} 秒内没有开始返回。\n` +
          `多因①请求了慢模型（对外已限制，正常不会出现）②同时并发多条请求导致排队。\n` +
          `建议：改用 dumate-lite（实测 3 秒级），并避免连点/并发。`
      );
    }, REQUEST_DEADLINE_MS);

    proxied.on('error', (e) => {
      if (guard) clearTimeout(guard);
      if (finished) return;
      finished = true;
      log(`502 ${ip} ${req.method} ${url} upstream-error=${e.message}${model ? ` model=${model}` : ''}`);
      if (!res.headersSent) deny(res, 502, '上游服务不可达：' + e.message);
      else res.end();
    });
    proxied.end(reqBody);
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
    `模型过滤：${MODEL_FILTER_ON ? `开（只放行 ${allowHint()}）` : '关'}`,
    `超时护栏：${Math.round(REQUEST_DEADLINE_MS / 1000)}s；健康路径本地直答：${HEALTH_PATHS.join(' ')}`,
    `LAN key 文件：${LAN_KEY_FILE}`,
    `其它设备可用的地址：`,
    ...ips.map((a) => `    http://${a}:${PORT}/opencode/v1`),
  ];
  lines.forEach((l) => console.log(l));
  log('START ' + lines.slice(0, 4).join(' | '));
});
