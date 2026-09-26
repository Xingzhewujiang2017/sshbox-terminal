import { createApp } from 'vue'
import { createPinia } from 'pinia'
import '@xterm/xterm/css/xterm.css'
import './assets/main.css'
import './assets/theme.css'
import { primeTheme } from './theme'
import { uiLog } from './api'

// 首帧前定色：settings.json 要异步读，先用上次的解析结果，避免闪一下深色再变白。
primeTheme()
import App from './App.vue'

// A2（v0.7.1）：前端全局异常兜底 ——「点了没反应 / 白屏」也要有据可查。
// unhandledrejection 会成风暴（一个坏轮询每秒 reject 一次）→ 按指纹节流：
// 同型错误 30 秒内只记第一次，窗口结束后的下一条带累计次数。
const lastLogged = new Map<string, { at: number; n: number }>()
function throttledLog(fingerprint: string, msg: string) {
  const now = Date.now()
  const prev = lastLogged.get(fingerprint)
  if (prev && now - prev.at < 30_000) {
    prev.n += 1
    return
  }
  if (prev && prev.n > 0) {
    void uiLog(`${msg}（近 30s 内同型异常共 ${prev.n + 1} 次）`, 'error')
  }
  lastLogged.set(fingerprint, { at: now, n: 0 })
  void uiLog(msg, 'error')
}

const app = createApp(App)
app.config.errorHandler = (err, _instance, info) => {
  throttledLog(`vue:${String(err)}:${info}`, `Vue 错误 ${String(err)}（${info}）`)
}
window.addEventListener('error', (e) => {
  throttledLog(`err:${e.message}`, `未捕获异常 ${e.message} @ ${e.filename}:${e.lineno}`)
})
window.addEventListener('unhandledrejection', (e) => {
  const reason =
    e.reason instanceof Error ? `${e.reason.name}: ${e.reason.message}` : String(e.reason)
  throttledLog(`rej:${reason}`, `未处理的 Promise 拒绝: ${reason}`)
})

app.use(createPinia()).mount('#app')
