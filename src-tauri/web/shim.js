/*
 * SSHBox Web 验证版的 Tauri 兼容层。
 *
 * 目的：前端一行不改就能在浏览器里跑。@tauri-apps/api 的所有调用最终都落到
 * `window.__TAURI_INTERNALS__` 上，所以在这里实现它即可：
 *   - invoke(cmd, args)        → POST /invoke
 *   - 事件（listen/emit）      → WebSocket /ws
 *   - 剪贴板 / 通知 / 打开链接 → 浏览器原生 API 或空实现
 *
 * 约定：服务端返回 {ok:true,data} 或 {ok:false,error}；error 原样抛给前端
 * （后端用 "SSHBOX_ERR:<json>" 表达可恢复错误，前端会自己解析）。
 */
(function () {
  'use strict'

  if (window.__TAURI_INTERNALS__) return // 已经注过就别重复

  var callbacks = new Map() // id -> { cb, once }
  var listeners = new Map() // eventName -> Set<handlerId>
  var nextCallbackId = 1
  var ws = null
  var wsOpen = false

  function transformCallback(cb, once) {
    var id = nextCallbackId++
    callbacks.set(id, { cb: cb, once: !!once })
    return id
  }

  function unregisterCallback(id) {
    callbacks.delete(id)
  }

  function runCallback(id, data) {
    var entry = callbacks.get(id)
    if (!entry) return
    try {
      entry.cb(data)
    } catch (err) {
      console.error('[shim] 回调出错', id, err)
    }
    if (entry.once) callbacks.delete(id)
  }

  function dispatch(eventName, payload) {
    var ids = listeners.get(eventName)
    if (!ids || ids.size === 0) return
    // Tauri 的 Event<T> 形状：{ event, id, payload }
    var evt = { event: eventName, id: 0, payload: payload }
    Array.from(ids).forEach(function (id) {
      runCallback(id, evt)
    })
  }

  function connect() {
    var proto = location.protocol === 'https:' ? 'wss://' : 'ws://'
    try {
      ws = new WebSocket(proto + location.host + '/ws')
    } catch (err) {
      console.error('[shim] WebSocket 打不开', err)
      return
    }
    ws.onopen = function () {
      wsOpen = true
      console.info('[shim] 事件通道已连接')
    }
    ws.onmessage = function (m) {
      var msg
      try {
        msg = JSON.parse(m.data)
      } catch (err) {
        return
      }
      if (msg && msg.event) dispatch(msg.event, msg.payload)
    }
    ws.onclose = function () {
      wsOpen = false
      setTimeout(connect, 1000) // 断线重连：后端重启时页面不用刷
    }
  }
  connect()

  function postInvoke(cmd, args) {
    return fetch('/invoke', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ cmd: cmd, args: args || {} })
    })
      .then(function (r) {
        if (!r.ok) throw 'HTTP ' + r.status
        return r.json()
      })
      .then(function (j) {
        if (!j.ok) throw j.error
        return j.data
      })
  }

  // 插件命令：能映射到浏览器原生 API 的就映射，其余返回空值让界面别崩
  function pluginCall(cmd, args) {
    var name = cmd.slice('plugin:'.length)
    if (name === 'clipboard-manager|write_text') {
      var text = args && (args.data != null ? args.data : args.text)
      return navigator.clipboard.writeText(String(text == null ? '' : text)).then(function () {
        return null
      })
    }
    if (name === 'clipboard-manager|read_text') {
      return navigator.clipboard.readText()
    }
    if (name === 'opener|open_url') {
      var url = args && (args.url || args.path)
      if (url) window.open(url, '_blank', 'noopener')
      return Promise.resolve(null)
    }
    if (name === 'app|version') return Promise.resolve('0.8.0-web')
    if (name === 'app|name') return Promise.resolve('SSHBox')
    return Promise.resolve(null)
  }

  function invoke(cmd, args, options) {
    args = args || {}
    if (cmd === 'plugin:event|listen') {
      var evName = args.event
      var handlerId = args.handler
      if (!listeners.has(evName)) listeners.set(evName, new Set())
      listeners.get(evName).add(handlerId)
      return Promise.resolve(handlerId)
    }
    if (cmd === 'plugin:event|unlisten') {
      var set = listeners.get(args.event)
      if (set) set.delete(args.eventId)
      return Promise.resolve(null)
    }
    if (cmd === 'plugin:event|emit' || cmd === 'plugin:event|emit_to') {
      return Promise.resolve(null)
    }
    if (cmd.indexOf('plugin:') === 0) return pluginCall(cmd, args)
    return postInvoke(cmd, args)
  }

  window.__TAURI_INTERNALS__ = {
    invoke: invoke,
    transformCallback: transformCallback,
    unregisterCallback: unregisterCallback,
    runCallback: runCallback,
    convertFileSrc: function (path) {
      return path
    },
    metadata: { currentWindow: { label: 'main' }, currentWebview: { label: 'main' } }
  }

  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = {
    unregisterListener: function (event, id) {
      var set = listeners.get(event)
      if (set) set.delete(id)
    }
  }

  console.info('[shim] Tauri 兼容层已就绪（' + (wsOpen ? '事件通道在线' : '事件通道连接中') + '）')
})()
