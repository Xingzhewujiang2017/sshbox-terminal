<script setup lang="ts">
import { computed, onMounted, onBeforeUnmount, ref, watch } from 'vue'
import { listen } from '@tauri-apps/api/event'
import { isPermissionGranted, requestPermission, sendNotification } from '@tauri-apps/plugin-notification'
import { revealItemInDir } from '@tauri-apps/plugin-opener'
import { disposeFleet, forgetSession, initFleet } from './fleet'
import TerminalPane from './components/TerminalPane.vue'
import MonitorPanel from './components/MonitorPanel.vue'
import HistoryPanel from './components/HistoryPanel.vue'
import OverviewPanel from './components/OverviewPanel.vue'
import HostList from './components/HostList.vue'
import HostDialog from './components/HostDialog.vue'
import HostKeyDialog from './components/HostKeyDialog.vue'
import PasswordDialog from './components/PasswordDialog.vue'
import SettingsDialog from './components/SettingsDialog.vue'
import {
  applyTheme,
  watchSystem,
  type ResolvedTheme,
  type ThemeMode,
} from './theme'
import {
  api,
  SshboxError,
  uiLog,
  type Alert,
  type AppPaths,
  type ErrPayload,
  type Host,
  type HostsFile,
  type KnownHostEntry,
  type Settings,
} from './api'

type TabStatus = 'connected' | 'closed' | 'connecting'

interface Tab {
  /** Stable across reconnects so the xterm scrollback survives. */
  id: string
  sid: string
  label: string
  hostId?: string | null
  host?: Host
  status: TabStatus
  attempts: number
  nextRetryIn?: number
}

const hosts = ref<HostsFile>({ version: 1, groups: ['默认'], hosts: [] })
const settings = ref<Settings | null>(null)
const paths = ref<AppPaths | null>(null)
const knownHosts = ref<KnownHostEntry[]>([])

const tabs = ref<Tab[]>([])
const activeIdx = ref(0)
const monitorVisible = ref(true)
const busyHostId = ref<string | null>(null)
const banner = ref<{ kind: 'error' | 'info'; text: string } | null>(null)

const activeTab = computed(() => tabs.value[activeIdx.value])

// --- dialogs ---------------------------------------------------------------
const hostDialog = ref<{ open: boolean; host: Host | null }>({ open: false, host: null })
const settingsOpen = ref(false)
const historyOpen = ref(false)
const overviewOpen = ref(false)
const hostKeyPrompt = ref<{ payload: ErrPayload; retry: () => void } | null>(null)
const passwordPrompt = ref<{
  payload: ErrPayload
  hostLabel: string
  defaultSave: boolean
  attemptError?: string
  submit: (password: string, save: boolean) => void
} | null>(null)
const confirmState = ref<{ text: string; onOk: () => void } | null>(null)

const settingsRef = ref<InstanceType<typeof SettingsDialog> | null>(null)

/**
 * Native Windows toast. Best-effort: a denied permission must not break the UI.
 * Every outcome goes to the backend log, because a silent notification failure
 * is otherwise indistinguishable from "nothing happened".
 */
async function desktopNotify(title: string, body: string) {
  try {
    let granted = await isPermissionGranted()
    if (!granted) granted = (await requestPermission()) === 'granted'
    if (!granted) {
      void uiLog('桌面通知未授权，仅应用内提示', 'warn')
      return
    }
    sendNotification({ title, body })
    void uiLog(`桌面通知已发送: ${title} — ${body}`)
  } catch (e) {
    void uiLog(`桌面通知失败: ${(e as Error).message}`, 'warn')
  }
}

function toast(kind: 'error' | 'info', text: string) {
  banner.value = { kind, text }
  if (kind === 'info') window.setTimeout(() => (banner.value = null), 4000)
}

// --- data loading ----------------------------------------------------------
async function loadAll() {
  hosts.value = await api.hostsList()
  settings.value = await api.settingsGet()
  // 主题要在设置读回来之后校正一次（启动时用的是 localStorage 缓存值）
  themeMode.value = (settings.value.theme as ThemeMode) || 'dark'
  applyThemeNow()
  rearmSystemWatch()
  paths.value = await api.appPaths()
  knownHosts.value = await api.knownHostsList()
}

onMounted(async () => {
  // Subscribe to every session's metrics once, for the whole app: the monitor
  // panel only hears its own tab, which is why an overview needs this.
  await initFleet()
  try {
    await loadAll()
  } catch (e) {
    toast('error', `初始化失败: ${(e as Error).message}`)
  }
  listen<{ sid: string; host_id?: string; label?: string }>('ssh://closed', (ev) => {
    const t = tabs.value.find((x) => x.sid === ev.payload.sid)
    if (t) markClosed(t)
  })
  listen<{ sid: string; alert: Alert }>('ssh://alert', (ev) => {
    const a = ev.payload.alert
    const tab = tabs.value.find((x) => x.sid === ev.payload.sid)
    toast('error', `${a.title}${tab ? ' · ' + tab.label : ''}：${a.body}`)
    void desktopNotify(a.title, `${tab ? tab.label + ' · ' : ''}${a.body}`)
  })
  window.addEventListener('keydown', onKey)
})

onBeforeUnmount(() => unwatchSystem?.())

onBeforeUnmount(() => {
  window.removeEventListener('keydown', onKey)
  disposeFleet()
})

function onKey(e: KeyboardEvent) {
  if (!e.ctrlKey) return
  if (e.key === 't' || e.key === 'T') {
    e.preventDefault()
    hostDialog.value = { open: true, host: null }
  } else if (e.key === 'w' || e.key === 'W') {
    e.preventDefault()
    if (activeTab.value) closeTab(activeIdx.value)
  }
}

/** Jump to the tab holding a session (used by the overview). */
function focusTab(sid: string) {
  const i = tabs.value.findIndex((t) => t.sid === sid)
  if (i >= 0) {
    activeIdx.value = i
    overviewOpen.value = false
  }
}

/** Connect a saved host from the overview — one host, never a batch. */
function connectFromOverview(hostId: string) {
  const host = hosts.value.hosts.find((h) => h.id === hostId)
  if (!host) return
  overviewOpen.value = false
  void connectHost(host)
}

/** Show an exported file in Explorer. */
async function revealExport(path: string) {
  try {
    await revealItemInDir(path)
  } catch (e) {
    uiLog(`打开目录失败: ${String(e)}`, 'warn')
  }
}

// --- connecting ------------------------------------------------------------
function addTab(sid: string, host?: Host, label?: string, id?: string) {
  // History ownership is bound backend-side at connect time, so there is no
  // frontend round-trip to race with the first sample.
  const tab: Tab = {
    id: id ?? sid,
    sid,
    label: label ?? (host ? host.name || host.host : 'session'),
    hostId: host?.id ?? null,
    host,
    status: 'connected',
    attempts: 0,
  }
  if (id) {
    const i = tabs.value.findIndex((t) => t.id === id)
    if (i >= 0) {
      tabs.value[i] = { ...tabs.value[i], ...tab }
      activeIdx.value = i
      return
    }
  }
  tabs.value.push(tab)
  activeIdx.value = tabs.value.length - 1
}

async function connectHost(host: Host, opts: {
  password?: string
  keyPassphrase?: string
  acceptHostKey?: boolean
  savePassword?: boolean
  tabId?: string
} = {}) {
  busyHostId.value = host.id
  uiLog(
    `连接请求 ${host.username}@${host.host}:${host.port} 参数=${JSON.stringify({
      pw: !!opts.password,
      ph: !!opts.keyPassphrase,
      acceptKey: !!opts.acceptHostKey,
      save: !!opts.savePassword,
      reconnect: !!opts.tabId,
    })}`,
  )
  try {
    const sid = await api.connectHost({
      hostId: host.id,
      password: opts.password,
      keyPassphrase: opts.keyPassphrase,
      acceptHostKey: opts.acceptHostKey,
      savePassword: opts.savePassword,
    })
    addTab(sid, host, undefined, opts.tabId)
    uiLog(`连接成功 sid=${sid}`)
    banner.value = null
  } catch (e) {
    handleConnectError(e as SshboxError, host, opts)
  } finally {
    busyHostId.value = null
  }
}

function handleConnectError(err: SshboxError, host: Host, opts: Record<string, unknown>) {
  const p = err.payload ?? ({ kind: 'unknown', message: err.message } as ErrPayload)
  uiLog(`连接失败 kind=${p.kind}: ${p.message}`, 'warn')
  switch (p.kind) {
    case 'host_key_unknown':
    case 'host_key_changed':
      uiLog('已打开主机密钥确认框')
      hostKeyPrompt.value = {
        payload: p,
        retry: () => {
          hostKeyPrompt.value = null
          connectHost(host, { ...opts, acceptHostKey: true })
        },
      }
      break
    case 'need_password':
    case 'auth_failed':
    case 'need_passphrase':
    case 'key_passphrase_wrong':
      uiLog('已打开密码/口令输入框')
      passwordPrompt.value = {
        payload: p,
        hostLabel: `${host.username}@${host.host}${host.port !== 22 ? ':' + host.port : ''}`,
        defaultSave: host.save_password,
        attemptError: p.kind === 'auth_failed' || p.kind === 'key_passphrase_wrong' ? p.message : undefined,
        submit: (password, save) => {
          passwordPrompt.value = null
          const isPass = p.kind === 'need_passphrase' || p.kind === 'key_passphrase_wrong'
          connectHost(host, {
            ...opts,
            ...(isPass ? { keyPassphrase: password } : { password, savePassword: save }),
          })
        },
      }
      break
    default:
      toast('error', p.message || '连接失败')
  }
}

async function trustHostKey() {
  const prompt = hostKeyPrompt.value
  if (!prompt) return
  const p = prompt.payload
  try {
    if (p.kind === 'host_key_changed' && p.host) {
      // Forget the stale entry first, otherwise verification keeps failing.
      await api.knownHostsRemove(p.host, p.port ?? 22)
      knownHosts.value = await api.knownHostsList()
    }
  } catch (e) {
    toast('error', `删除旧记录失败: ${(e as Error).message}`)
    return
  }
  prompt.retry()
}

// --- reconnect -------------------------------------------------------------
const RETRY_DELAYS = [2, 5, 10, 20]

function markClosed(t: Tab) {
  t.status = 'closed'
  const wantAuto = (settings.value?.auto_reconnect ?? true) && (t.host?.auto_reconnect ?? true) && !!t.host
  if (!wantAuto) return
  scheduleReconnect(t)
}

function scheduleReconnect(t: Tab) {
  const max = settings.value?.reconnect_max_attempts ?? 3
  if (!t.host || t.attempts >= max) {
    if (t.attempts > 0) toast('error', `${t.label} 自动重连失败（已尝试 ${t.attempts} 次）`)
    return
  }
  const delay = RETRY_DELAYS[Math.min(t.attempts, RETRY_DELAYS.length - 1)]
  t.attempts += 1
  t.status = 'connecting'
  t.nextRetryIn = delay
  const tick = window.setInterval(() => {
    if (t.nextRetryIn !== undefined && t.nextRetryIn > 0) t.nextRetryIn -= 1
  }, 1000)
  window.setTimeout(async () => {
    window.clearInterval(tick)
    t.nextRetryIn = undefined
    if (t.status !== 'connecting') return
    await connectHost(t.host!, { tabId: t.id })
  }, delay * 1000)
}

function manualReconnect(t: Tab) {
  t.attempts = 0
  t.status = 'connecting'
  connectHost(t.host!, { tabId: t.id })
}

// --- tabs ------------------------------------------------------------------
function closeTab(i: number) {
  const t = tabs.value[i]
  if (!t) return
  const doClose = async () => {
    if (t.status !== 'closed') {
      try {
        await api.disconnect(t.sid)
      } catch {
        /* session may already be gone */
      }
    }
    tabs.value.splice(i, 1)
    forgetSession(t.sid)
    if (activeIdx.value >= tabs.value.length) activeIdx.value = Math.max(0, tabs.value.length - 1)
  }
  if (settings.value?.confirm_on_close_tab && t.status === 'connected') {
    confirmState.value = { text: `关闭标签「${t.label}」？该 SSH 会话将断开。`, onOk: () => { confirmState.value = null; doClose() } }
  } else {
    doClose()
  }
}

watch(activeIdx, (idx, old) => {
  const now = tabs.value[idx]
  const prev = old !== undefined ? tabs.value[old] : undefined
  if (now?.sid) api.monitorSetVisible(now.sid, true).catch(() => {})
  if (prev?.sid) api.monitorSetVisible(prev.sid, false).catch(() => {})
})

// --- host CRUD -------------------------------------------------------------
async function saveHost(host: Host, password: string | null) {
  try {
    await api.hostSave(host, password ?? undefined)
    hosts.value = await api.hostsList()
    hostDialog.value = { open: false, host: null }
    toast('info', `已保存「${host.name || host.host}」`)
  } catch (e) {
    toast('error', `保存失败: ${(e as Error).message}`)
  }
}

async function deleteHost(host: Host) {
  confirmState.value = {
    text: `删除主机「${host.name || host.host}」？同时会删除已保存的密码。`,
    onOk: async () => {
      confirmState.value = null
      try {
        await api.hostDelete(host.id)
        hosts.value = await api.hostsList()
      } catch (e) {
        toast('error', `删除失败: ${(e as Error).message}`)
      }
    },
  }
}

function duplicateHost(host: Host) {
  hostDialog.value = { open: true, host: { ...host, id: '', name: `${host.name} 副本` } }
}

/* ---------- 主题 ----------
   三态：dark / light / system。CSS 侧靠 <html data-theme> 生效，
   xterm 与 ECharts 读 themeVersion 重新上色（见 theme.ts）。 */
const THEME_LABELS: Record<ThemeMode, string> = {
  dark: '深色',
  light: '浅色',
  system: '跟随系统',
}
const themeMode = ref<ThemeMode>('dark')
const resolvedTheme = ref<ResolvedTheme>('dark')
let unwatchSystem: (() => void) | null = null

/** 立即上色，并把解析结果同步给终端/图表。不落盘（落盘走 saveTheme）。 */
function applyThemeNow() {
  resolvedTheme.value = applyTheme(themeMode.value)
}

/** 跟随系统时，系统配色一变就跟着变（不写回 settings，mode 还是 system）。 */
function rearmSystemWatch() {
  unwatchSystem?.()
  unwatchSystem = null
  if (themeMode.value === 'system') {
    unwatchSystem = watchSystem(() => applyThemeNow())
  }
}

async function setTheme(mode: ThemeMode, persist = true) {
  themeMode.value = mode
  applyThemeNow()
  rearmSystemWatch()
  if (!persist || !settings.value) return
  await saveSettings({ ...settings.value, theme: mode })
}

/** 标题栏那个按钮：在深色和浅色之间翻。当前是"跟随系统"时，
 *  翻到系统当前色的反面（用户的意图是"我要另一个样子"，不是"我要脱离系统"）。 */
function toggleTheme() {
  setTheme(resolvedTheme.value === 'dark' ? 'light' : 'dark')
}

async function saveSettings(s: Settings) {
  try {
    await api.settingsSet(s)
    settings.value = s
    toast('info', '设置已保存')
  } catch (e) {
    toast('error', `保存设置失败: ${(e as Error).message}`)
  }
}

/** Quick interval switch from the monitor panel. The collect loop re-reads
 * settings every pass, so this needs no monitor restart. */
async function setSampleInterval(secs: number) {
  if (!settings.value) return
  const next = { ...settings.value, sample_interval_secs: secs }
  try {
    await api.settingsSet(next)
    settings.value = next
    toast('info', `采样间隔已改为 ${secs}s`)
  } catch (e) {
    toast('error', `改采样间隔失败: ${(e as Error).message}`)
  }
}

async function removeKnownHost(host: string, port: number) {
  try {
    const n = await api.knownHostsRemove(host, port)
    knownHosts.value = await api.knownHostsList()
    toast('info', `已删除 ${n} 条记录`)
  } catch (e) {
    toast('error', `删除失败: ${(e as Error).message}`)
  }
}

async function exportHosts(path: string, includeSecrets: boolean) {
  try {
    const n = await api.hostsExport(path, includeSecrets)
    settingsRef.value?.setIoMsg(`已导出 ${n} 台主机到 ${path}`)
  } catch (e) {
    settingsRef.value?.setIoMsg(`导出失败: ${(e as Error).message}`)
  }
}

async function importHosts(path: string) {
  try {
    const n = await api.hostsImport(path)
    hosts.value = await api.hostsList()
    settingsRef.value?.setIoMsg(`已导入 ${n} 台主机`)
  } catch (e) {
    settingsRef.value?.setIoMsg(`导入失败: ${(e as Error).message}`)
  }
}

async function restartMonitor() {
  const t = activeTab.value
  if (!t) return toast('error', '没有活动会话')
  try {
    await api.monitorRestart(t.sid)
    toast('info', '监控任务已重启')
  } catch (e) {
    toast('error', (e as Error).message)
  }
}

function statusDot(t: Tab) {
  return t.status === 'connected'
    ? 'var(--ctp-green)'
    : t.status === 'connecting'
      ? 'var(--ctp-yellow)'
      : 'var(--ctp-red)'
}
</script>

<template>
  <div class="app">
    <HostList
      :data="hosts"
      :active-host-id="activeTab?.hostId"
      :busy-id="busyHostId"
      @new="hostDialog = { open: true, host: null }"
      @connect="connectHost($event)"
      @edit="hostDialog = { open: true, host: $event }"
      @duplicate="duplicateHost"
      @delete="deleteHost"
      @settings="settingsOpen = true"
    />

    <main class="main">
      <div class="tabbar">
        <div
          v-for="(t, i) in tabs"
          :key="t.id"
          class="tab"
          :class="{ active: i === activeIdx }"
          @click="activeIdx = i"
        >
          <span class="dot" :style="{ background: statusDot(t) }"></span>
          <span class="tab-label">{{ t.label }}</span>
          <span v-if="t.status === 'connecting'" class="retry">
            {{ t.nextRetryIn ? `重连 ${t.nextRetryIn}s` : '重连中…' }}
          </span>
          <span v-else-if="t.status === 'closed'" class="retry">已断开</span>
          <span class="tab-close" @click.stop="closeTab(i)">×</span>
        </div>
        <div class="tabbar-right">
          <button
            v-if="activeTab?.status === 'closed'"
            class="reconnect-btn"
            @click="manualReconnect(activeTab)"
          >重连</button>
          <button
            class="icon-btn"
            :title="`主题：${THEME_LABELS[themeMode]}（点击切到${resolvedTheme === 'dark' ? '浅色' : '深色'}）`"
            @click="toggleTheme"
          >{{ resolvedTheme === 'dark' ? '☾' : '☀' }}</button>
          <button class="icon-btn" title="总览（所有已连接主机）" @click="overviewOpen = true">
            总览
          </button>
          <button class="icon-btn" title="历史回看（落盘数据）" @click="historyOpen = true">
            历史
          </button>
          <button class="icon-btn" title="切换监控面板" @click="monitorVisible = !monitorVisible">
            {{ monitorVisible ? '◧' : '◨' }}
          </button>
        </div>
      </div>

      <div v-if="banner" class="banner" :class="banner.kind">
        <span>{{ banner.text }}</span>
        <button @click="banner = null">×</button>
      </div>

      <div class="content">
        <div class="term-area">
          <div v-if="!tabs.length" class="empty">
            <div class="empty-title">SSHBox</div>
            <div class="empty-sub">SSH 终端 + 虚拟机实时监控</div>
            <button class="connect-btn" @click="hostDialog = { open: true, host: null }">新建连接</button>
            <div class="empty-hint">左侧主机列表双击即可连接 · Ctrl+T 新建 · Ctrl+W 关闭</div>
          </div>
          <TerminalPane
            v-for="(t, i) in tabs"
            :key="t.id"
            :sid="t.sid"
            :active="i === activeIdx"
          />
        </div>
        <div v-if="monitorVisible && activeTab" class="monitor-area">
          <MonitorPanel
            :sid="activeTab.sid"
            :active="true"
            :interval="settings?.sample_interval_secs ?? 2"
            @set-interval="setSampleInterval"
          />
        </div>
      </div>
    </main>

    <OverviewPanel
      v-if="overviewOpen"
      :tabs="tabs"
      :hosts="hosts"
      :interval="settings?.sample_interval_secs ?? 2"
      :active-sid="activeTab?.sid"
      @close="overviewOpen = false"
      @focus="focusTab"
      @connect="connectFromOverview"
    />

    <HistoryPanel
      v-if="historyOpen"
      :hosts="hosts"
      :initial-host-id="activeTab?.hostId"
      @close="historyOpen = false"
      @open-path="revealExport"
    />

    <HostDialog
      v-if="hostDialog.open"
      :host="hostDialog.host"
      :groups="hosts.groups"
      :existing-groups="hosts.groups"
      @save="saveHost"
      @close="hostDialog = { open: false, host: null }"
    />

    <HostKeyDialog
      v-if="hostKeyPrompt"
      :payload="hostKeyPrompt.payload"
      @trust="trustHostKey"
      @cancel="hostKeyPrompt = null"
    />

    <PasswordDialog
      v-if="passwordPrompt"
      :payload="passwordPrompt.payload"
      :host-label="passwordPrompt.hostLabel"
      :default-save="passwordPrompt.defaultSave"
      :attempt-error="passwordPrompt.attemptError"
      @submit="(pw, save) => passwordPrompt?.submit(pw, save)"
      @cancel="passwordPrompt = null"
    />

    <SettingsDialog
      v-if="settingsOpen && settings"
      ref="settingsRef"
      :settings="settings"
      :paths="paths"
      :known-hosts="knownHosts"
      :host-count="hosts.hosts.length"
      @save="saveSettings"
      @theme="setTheme($event)"
      @close="settingsOpen = false"
      @remove-known-host="removeKnownHost"
      @export-hosts="exportHosts"
      @import-hosts="importHosts"
      @restart-monitor="restartMonitor"
    />

    <div v-if="confirmState" class="modal-mask" @click.self="confirmState = null">
      <div class="confirm">
        <div class="confirm-text">{{ confirmState.text }}</div>
        <div class="confirm-btns">
          <button @click="confirmState = null">取消</button>
          <button class="danger" @click="confirmState.onOk()">确定</button>
        </div>
      </div>
    </div>
  </div>
</template>

<style>
* { margin: 0; padding: 0; }
html, body, #app { height: 100%; overflow: hidden; }
body { font-family: 'Segoe UI', 'Microsoft YaHei', sans-serif; background: var(--ctp-crust); color: var(--ctp-text); }
</style>

<style scoped>
.app { display: flex; height: 100vh; }
.main { flex: 1; display: flex; flex-direction: column; min-width: 0; }
.tabbar {
  display: flex; background: var(--ctp-mantle); border-bottom: 1px solid var(--ctp-surface0);
  align-items: center; min-height: 36px;
}
.tab {
  display: flex; align-items: center; gap: 6px; padding: 8px 12px;
  font-size: 12px; color: var(--ctp-subtext0); cursor: pointer; border-right: 1px solid var(--ctp-surface0);
  max-width: 260px;
}
.tab.active { background: var(--ctp-base); color: var(--ctp-text); border-top: 2px solid var(--ctp-blue); }
.dot { width: 6px; height: 6px; border-radius: 50%; flex-shrink: 0; }
.tab-label { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.retry { font-size: 10px; color: var(--ctp-yellow); }
.tab-close { color: var(--ctp-overlay0); font-size: 14px; }
.tab-close:hover { color: var(--ctp-red); }
.tabbar-right { margin-left: auto; padding-right: 8px; display: flex; align-items: center; gap: 8px; }
.icon-btn { background: none; border: none; color: var(--ctp-subtext0); cursor: pointer; font-size: 16px; }
.reconnect-btn {
  background: var(--ctp-green); color: var(--on-accent); border: none; border-radius: 5px;
  padding: 3px 10px; font-size: 11.5px; font-weight: 600; cursor: pointer;
}
.banner {
  display: flex; align-items: center; justify-content: space-between; gap: 10px;
  padding: 7px 12px; font-size: 12px; border-bottom: 1px solid var(--ctp-surface0);
}
.banner.error { background: var(--banner-error-bg); color: var(--ctp-red); }
.banner.info { background: var(--banner-info-bg); color: var(--ctp-green); }
.banner button { background: none; border: none; color: inherit; cursor: pointer; font-size: 14px; }
.content { flex: 1; display: flex; min-height: 0; }
.term-area { flex: 1; min-width: 0; position: relative; background: var(--ctp-base); }
.monitor-area { width: 320px; border-left: 1px solid var(--ctp-surface0); min-width: 240px; }
.empty {
  height: 100%; display: flex; flex-direction: column; gap: 12px;
  align-items: center; justify-content: center; color: var(--ctp-overlay0);
}
.empty-title { font-size: 28px; font-weight: 700; color: var(--ctp-blue); }
.empty-sub { font-size: 13px; }
.empty-hint { font-size: 11px; color: var(--ctp-surface1); }
.connect-btn {
  background: var(--ctp-blue); color: var(--on-accent); border: none; border-radius: 6px;
  padding: 8px 16px; font-size: 13px; font-weight: 600; cursor: pointer;
}
.connect-btn:hover { background: var(--ctp-lavender); }
.modal-mask {
  position: fixed; inset: 0; background: var(--mask);
  display: flex; align-items: center; justify-content: center; z-index: 120;
}
.confirm {
  background: var(--ctp-base); border: 1px solid var(--ctp-surface0); border-radius: 10px;
  padding: 18px; width: 380px; display: flex; flex-direction: column; gap: 14px;
}
.confirm-text { font-size: 13px; color: var(--ctp-text); line-height: 1.6; }
.confirm-btns { display: flex; gap: 8px; justify-content: flex-end; }
.confirm-btns button {
  padding: 7px 16px; border-radius: 6px; border: 1px solid var(--ctp-surface0);
  background: var(--ctp-crust); color: var(--ctp-subtext0); cursor: pointer; font-size: 13px;
}
.confirm-btns .danger { background: var(--ctp-red); color: var(--on-accent); border: none; font-weight: 600; }
</style>
