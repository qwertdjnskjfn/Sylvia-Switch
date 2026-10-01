#!/usr/bin/env node
/**
 * LAN 分享反代（白名单模式）—— 只为「局域网另一台设备临时调用」设计。
 *
 * 安全设计（三层，缺一不可）：
 *   1) 路径白名单：只转发 /opencode/v1/*，其余（尤其 124 条 /api/* 管理接口）一律 403，
 *      上游根本收不到请求 —— 管理面仍只在本机回环。
 *   2) 独立 LAN key：调用方拿的是 lan-* 专用 key，不是网关真 key。
 *      key 校验通过后由本进程**注入**真正的网关 key 转发 →
 *      网关 key 永不离开本机，吊销只需换掉 lan_share_key.txt。
 *   3) 每 IP 限流：默认 60 请求/分钟，超出 429（防被拿去做批量）。
 *
 * 只监听 0.0.0.0 的**一个端口**，随开随关：关掉进程 = 分享立即终止。
 *
 * 用法：node tools/lan_share_proxy.js [port]     (默认 57900)
 * 日志：~/.wb-switch/lan_proxy.log
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

const server = http.createServer((req, res) => {
  const started = Date.now();
  const ip = (req.socket.remoteAddress || '').replace('::ffff:', '');
  const url = req.url || '/';

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

  const headers = Object.assign({}, req.headers);
  headers['authorization'] = 'Bearer ' + UPSTREAM_KEY;   // 注入真 key
  delete headers['x-api-key'];                            // 避免调用方自带 x-api-key 混淆
  headers['host'] = `${UPSTREAM_HOST}:${UPSTREAM_PORT}`;

  const proxied = http.request(
    { host: UPSTREAM_HOST, port: UPSTREAM_PORT, method: req.method, path: url, headers },
    (up) => {
      res.writeHead(up.statusCode || 502, up.headers);
      up.pipe(res);                                       // 逐块转发：SSE / chunked 原样透传
      up.on('end', () => log(`${up.statusCode} ${ip} ${req.method} ${url} ${Date.now() - started}ms`));
    }
  );
  proxied.on('error', (e) => {
    log(`502 ${ip} ${req.method} ${url} upstream-error=${e.message}`);
    if (!res.headersSent) deny(res, 502, '上游服务不可达：' + e.message);
    else res.end();
  });
  req.pipe(proxied);
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
    `LAN key 文件：${LAN_KEY_FILE}`,
    `其它设备可用的地址：`,
    ...ips.map((a) => `    http://${a}:${PORT}/opencode/v1`),
  ];
  lines.forEach((l) => console.log(l));
  log('START ' + lines.slice(0, 2).join(' | '));
});
