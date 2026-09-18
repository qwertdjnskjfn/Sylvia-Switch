/**
 * Sylvia Switch 自定义 Provider（供 Cuckoo Code 导入）
 *
 * 使用方法：
 *   1. 下载本文件：http://127.0.0.1:57890/cuckoo/provider.js
 *   2. 在 Cuckoo Code 中「导入自定义 Provider」选择本文件
 *   3. 新建窗口时会自动打开 http://127.0.0.1:57890/cuckoo/chat
 *   4. Agent 循环由此驱动：网页 AI（经本机网关桥接）生成工具调用，
 *      Cuckoo Code 解析并执行，结果回传 —— 零 Token 成本
 *
 * 模式说明：useIntercept = true，但 hook 为空操作 ——
 *   聊天页自身在流式回复完成时派发 'cuckoo-ai-response' 事件
 *   （detail: { text: 完整回复, finished: true }），
 *   Cuckoo Code 隔离世界的拦截监听器直接接收，无需 DOM 抓取。
 */
/** @type {import('./provider.d.ts').Provider} */
module.exports = {
  id: 'sylvia-switch',
  name: 'Sylvia Switch',
  homeUrl: 'http://127.0.0.1:57890/cuckoo/chat',
  sessionUrlBase: 'http://127.0.0.1:57890/cuckoo/chat',

  // 输入框选择器（聊天页 DOM 完全自有，选择器稳定）
  inputSelectors: [
    '#sylvia-input',
    'textarea',
  ],
  inputKeywords: ['sylvia', '输入', '消息'],

  // 发送按钮选择器
  sendButtonSelectors: [
    '#sylvia-send',
  ],

  // 用户信息选择器（本平台无登录态，返回空即可）
  userInfoSelector: '#sylvia-status',

  // 首页判断正则
  homeUrlPattern: /^http:\/\/127\.0\.0\.1:57890\/cuckoo\/chat\/?(\?.*)?$/,

  // 单会话模型：聊天页自行维护 localStorage 历史
  extractSessionId(url) {
    if (!url || !url.includes('/cuckoo/chat')) return null;
    return 'default';
  },

  // 判断 URL 是否属于本平台
  matchesUrl(url) {
    if (!url) return false;
    return url.includes('127.0.0.1:57890/cuckoo');
  },

  // 判断元素是否可见
  isElementVisible(el) {
    if (!el) return false;
    return el.offsetWidth > 0 && el.offsetHeight > 0;
  },

  // 查找输入框
  findInput() {
    for (const sel of this.inputSelectors) {
      const el = document.querySelector(sel);
      if (this.isElementVisible(el)) return el;
    }
    return null;
  },

  // 查找发送按钮
  findSendButton() {
    for (const sel of this.sendButtonSelectors) {
      const btn = document.querySelector(sel);
      if (this.isElementVisible(btn) && !btn.disabled) return btn;
    }
    return null;
  },

  // 提取用户信息
  extractUserInfo() {
    const el = document.querySelector(this.userInfoSelector);
    return el ? el.textContent.trim() : '';
  },

  // ========== 网络拦截模式 ==========
  // hook 置为空操作：回复事件由聊天页自身派发（见文件头说明）。
  useIntercept: true,
  getHookSource() {
    return '(function(){ /* sylvia-switch: page dispatches cuckoo-ai-response itself */ });';
  },
};
