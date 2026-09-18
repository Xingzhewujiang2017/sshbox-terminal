<script setup lang="ts">
import { ref, watch } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import TerminalPane from './components/TerminalPane.vue'
import MonitorPanel from './components/MonitorPanel.vue'

interface Tab {
  sid: string
  label: string
}

const tabs = ref<Tab[]>([])
const activeIdx = ref(0)
const showConnect = ref(false)
const monitorVisible = ref(true)

const form = ref({
  host: '',
  port: 22,
  username: 'root',
  authMode: 'password' as 'password' | 'key',
  password: '',
  privateKey: '',
})
const connecting = ref(false)
const connectError = ref('')

async function doConnect() {
  connecting.value = true
  connectError.value = ''
  try {
    const sid = await invoke<string>('connect', {
      params: {
        host: form.value.host,
        port: form.value.port,
        username: form.value.username,
        password: form.value.authMode === 'password' ? form.value.password : null,
        private_key: form.value.authMode === 'key' ? form.value.privateKey : null,
        key_passphrase: null,
      },
    })
    tabs.value.push({ sid, label: `${form.value.username}@${form.value.host}` })
    activeIdx.value = tabs.value.length - 1
    showConnect.value = false
    form.value.password = ''
  } catch (e) {
    connectError.value = String(e)
  } finally {
    connecting.value = false
  }
}

async function closeTab(i: number) {
  const t = tabs.value[i]
  try { await invoke('disconnect', { sid: t.sid }) } catch {}
  tabs.value.splice(i, 1)
  if (activeIdx.value >= tabs.value.length) activeIdx.value = Math.max(0, tabs.value.length - 1)
}

// Handle session closed events (server dropped connection)
listen<{ sid: string }>('ssh://closed', (e) => {
  const i = tabs.value.findIndex(t => t.sid === e.payload.sid)
  if (i >= 0) tabs.value[i].label += ' [已断开]'
})

// Tell backend which session's monitor is visible (throttle hidden ones)
watch(activeIdx, (idx, old) => {
  if (tabs.value[idx]) invoke('monitor_set_visible', { sid: tabs.value[idx].sid, visible: true })
  if (old !== undefined && tabs.value[old]) invoke('monitor_set_visible', { sid: tabs.value[old].sid, visible: false })
})
</script>

<template>
  <div class="app">
    <!-- Sidebar -->
    <aside class="sidebar">
      <div class="logo">SSH<span>Box</span></div>
      <button class="connect-btn" @click="showConnect = true">＋ 新建连接</button>
      <div class="hint">M3: 主机列表将出现在这里</div>
    </aside>

    <!-- Main -->
    <main class="main">
      <!-- Tab bar -->
      <div class="tabbar">
        <div
          v-for="(t, i) in tabs"
          :key="t.sid"
          class="tab"
          :class="{ active: i === activeIdx }"
          @click="activeIdx = i"
        >
          <span class="tab-label">{{ t.label }}</span>
          <span class="tab-close" @click.stop="closeTab(i)">×</span>
        </div>
        <div class="tabbar-right">
          <button class="icon-btn" @click="monitorVisible = !monitorVisible" title="切换监控面板">
            {{ monitorVisible ? '◧' : '◨' }}
          </button>
        </div>
      </div>

      <!-- Content -->
      <div class="content">
        <div class="term-area">
          <div v-if="tabs.length === 0" class="empty">
            <div class="empty-title">SSHBox</div>
            <div>SSH 终端 + 虚拟机实时监控</div>
            <button class="connect-btn" @click="showConnect = true">连接一台主机</button>
          </div>
          <TerminalPane
            v-for="(t, i) in tabs"
            :key="t.sid"
            :sid="t.sid"
            :active="i === activeIdx"
          />
        </div>
        <div v-if="monitorVisible && tabs.length" class="monitor-area">
          <MonitorPanel :sid="tabs[activeIdx].sid" :active="true" />
        </div>
      </div>
    </main>

    <!-- Connect dialog -->
    <div v-if="showConnect" class="modal-mask" @click.self="showConnect = false">
      <div class="modal">
        <h3>新建 SSH 连接</h3>
        <label>主机地址</label>
        <div class="host-row">
          <input v-model="form.host" placeholder="192.168.x.x 或域名" autofocus />
          <input v-model.number="form.port" type="number" class="port" />
        </div>
        <label>用户名</label>
        <input v-model="form.username" />
        <label>认证方式</label>
        <div class="auth-row">
          <button :class="{ sel: form.authMode === 'password' }" @click="form.authMode = 'password'">密码</button>
          <button :class="{ sel: form.authMode === 'key' }" @click="form.authMode = 'key'">私钥</button>
        </div>
        <template v-if="form.authMode === 'password'">
          <label>密码</label>
          <input v-model="form.password" type="password" @keyup.enter="doConnect" />
        </template>
        <template v-else>
          <label>私钥内容 (OpenSSH 格式)</label>
          <textarea v-model="form.privateKey" rows="5" placeholder="-----BEGIN OPENSSH PRIVATE KEY-----"></textarea>
        </template>
        <div v-if="connectError" class="error">{{ connectError }}</div>
        <div class="modal-btns">
          <button @click="showConnect = false">取消</button>
          <button class="primary" :disabled="connecting || !form.host" @click="doConnect">
            {{ connecting ? '连接中…' : '连接' }}
          </button>
        </div>
      </div>
    </div>
  </div>
</template>

<style>
* { margin: 0; padding: 0; }
html, body, #app { height: 100%; overflow: hidden; }
body { font-family: 'Segoe UI', 'Microsoft YaHei', sans-serif; background: #11111b; color: #cdd6f4; }
</style>

<style scoped>
.app { display: flex; height: 100vh; }
.sidebar {
  width: 180px; background: #11111b; border-right: 1px solid #313244;
  padding: 12px; display: flex; flex-direction: column; gap: 10px;
}
.logo { font-size: 18px; font-weight: 700; color: #cdd6f4; }
.logo span { color: #89b4fa; }
.connect-btn {
  background: #89b4fa; color: #11111b; border: none; border-radius: 6px;
  padding: 8px 12px; font-size: 13px; font-weight: 600; cursor: pointer;
}
.connect-btn:hover { background: #b4befe; }
.hint { color: #45475a; font-size: 11px; }
.main { flex: 1; display: flex; flex-direction: column; min-width: 0; }
.tabbar {
  display: flex; background: #181825; border-bottom: 1px solid #313244;
  align-items: center; min-height: 36px;
}
.tab {
  display: flex; align-items: center; gap: 6px; padding: 8px 12px;
  font-size: 12px; color: #a6adc8; cursor: pointer; border-right: 1px solid #313244;
  max-width: 220px;
}
.tab.active { background: #1e1e2e; color: #cdd6f4; border-top: 2px solid #89b4fa; }
.tab-label { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.tab-close { color: #6c7086; font-size: 14px; }
.tab-close:hover { color: #f38ba8; }
.tabbar-right { margin-left: auto; padding-right: 8px; }
.icon-btn { background: none; border: none; color: #a6adc8; cursor: pointer; font-size: 16px; }
.content { flex: 1; display: flex; min-height: 0; }
.term-area { flex: 1; min-width: 0; position: relative; background: #1e1e2e; }
.monitor-area { width: 300px; border-left: 1px solid #313244; min-width: 220px; }
.empty {
  height: 100%; display: flex; flex-direction: column; gap: 12px;
  align-items: center; justify-content: center; color: #6c7086;
}
.empty-title { font-size: 28px; font-weight: 700; color: #89b4fa; }
.modal-mask {
  position: fixed; inset: 0; background: rgba(0,0,0,0.6);
  display: flex; align-items: center; justify-content: center; z-index: 100;
}
.modal {
  background: #1e1e2e; border: 1px solid #313244; border-radius: 10px;
  padding: 20px; width: 400px; display: flex; flex-direction: column; gap: 6px;
}
.modal h3 { margin-bottom: 8px; }
.modal label { font-size: 11px; color: #a6adc8; margin-top: 6px; }
.modal input, .modal textarea {
  background: #11111b; border: 1px solid #313244; border-radius: 6px;
  color: #cdd6f4; padding: 8px; font-size: 13px; width: 100%; box-sizing: border-box;
  font-family: inherit;
}
.modal input:focus, .modal textarea:focus { outline: 1px solid #89b4fa; }
.host-row { display: flex; gap: 8px; }
.host-row .port { width: 80px; flex-shrink: 0; }
.auth-row { display: flex; gap: 8px; }
.auth-row button {
  flex: 1; background: #11111b; border: 1px solid #313244; color: #a6adc8;
  border-radius: 6px; padding: 6px; cursor: pointer; font-size: 12px;
}
.auth-row button.sel { border-color: #89b4fa; color: #89b4fa; }
.error { color: #f38ba8; font-size: 12px; margin-top: 8px; word-break: break-all; }
.modal-btns { display: flex; gap: 8px; justify-content: flex-end; margin-top: 12px; }
.modal-btns button {
  padding: 8px 16px; border-radius: 6px; border: 1px solid #313244;
  background: #11111b; color: #a6adc8; cursor: pointer; font-size: 13px;
}
.modal-btns .primary { background: #89b4fa; color: #11111b; border: none; font-weight: 600; }
.modal-btns .primary:disabled { opacity: 0.5; cursor: not-allowed; }
</style>
