# -*- coding: utf-8 -*-
"""调试：/matrix 余额端点的域/路径/token位置变体矩阵。"""
import json, uuid, urllib.request, time, hashlib, os

HOME = os.path.expanduser('~')
AUTH = os.path.join(HOME, '.minimax', 'auth', 'prod', 'cn', 'mcode-public', 'auth.json')
recs = json.load(open(AUTH, encoding='utf-8'))['records']
tok = None
for k, v in recs.items():
    if v.get('accessToken'):
        tok = v['accessToken']
        break
assert tok, 'no token'
SALT = "I*7Cf%WZ#S&%1RlZJ&C2"

# 桌面端指纹（真实值）
cfgp = os.path.expandvars(r'%APPDATA%\MiniMax\minimax-agent-cn-config.json')
shared = json.load(open(cfgp, encoding='utf-8')).get('sharedUser', {})
UUID_ = shared.get('userID', '')
DEVICE = shared.get('deviceID', '')
USER = shared.get('userID', '')

def md5(s):
    import hashlib
    return hashlib.md5(s.encode()).hexdigest()

def enc(s):
    out = ''
    for b in s.encode():
        c = chr(b)
        if c.isalnum() or c in "*-._":
            out += c
        else:
            out += '%%%02X' % b
    return out

def query(forsig):
    ms = str(int(time.time() * 1000))
    pairs = [
        ('device_platform', 'web'), ('biz_id', '3'), ('app_id', '3001'),
        ('version_code', '22201'), ('unix', ms), ('timezone_offset', '28800'),
        ('sys_language', 'zh'), ('lang', 'zh'), ('uuid', UUID_),
        ('device_id', DEVICE), ('os_name', 'Windows'), ('browser_name', 'Chrome'),
        ('device_memory', '16'), ('cpu_core_num', '8'), ('browser_language', 'zh-CN'),
        ('browser_platform', 'Win32'), ('user_id', USER),
    ]
    if forsig:
        pairs.append(('op_ticket', 'undefined'))
    pairs += [('screen_width', '1707'), ('screen_height', '1067'), ('token', tok), ('client', 'web'), ('region', 'cn')]
    q = '&'.join(f'{k}={enc(v)}' for k, v in pairs)
    return q, ms

HOSTS = ['https://agent.minimaxi.com', 'https://agent.minimax.cn']
PATHS = ['/matrix/api/v1/commerce/get_membership_info']

for host in HOSTS:
    for path in PATHS:
        body = '{}'
        forsig, ms = query(True)
        wire, _ = query(False)
        sec = str(int(time.time()))
        xs = md5(sec + SALT + body)
        rel = path + '?' + forsig
        yy = md5(enc(rel) + '_' + body + md5(ms) + 'ooui')
        url = host + path + '?' + wire
        req = urllib.request.Request(url, data=body.encode(), method='POST')
        req.add_header('Content-Type', 'application/json')
        req.add_header('Accept', 'application/json')
        req.add_header('token', tok)
        req.add_header('x-timestamp', sec)
        req.add_header('x-signature', xs)
        req.add_header('yy', yy)
        req.add_header('Origin', host)
        req.add_header('Referer', host + '/')
        req.add_header('User-Agent', 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0 Safari/537.36')
        try:
            r = opener.open(req, timeout=30) if (opener := urllib.request.build_opener(urllib.request.ProxyHandler({}))) else None
            body_txt = r.read().decode('utf-8', 'replace')
            print(f'[{host.split("//")[1]}{path}] 200:', body_txt[:200])
        except urllib.error.HTTPError as e:
            bt = e.read().decode('utf-8', 'replace')
            print(f'[{host.split("//")[1]}{path}] HTTP {e.code}:', bt[:200])
        except Exception as e:
            print(f'[{host}{path}] FAIL:', repr(e)[:100])
        time.sleep(1)
