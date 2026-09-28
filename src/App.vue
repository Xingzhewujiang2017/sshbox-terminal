<script setup lang="ts">
import { computed, nextTick, onMounted, onBeforeUnmount, ref, watch, type Ref } from 'vue'
import { listen } from '@tauri-apps/api/event'
import { isPermissionGranted, requestPermission, sendNotification } from '@tauri-apps/plugin-notification'
import { revealItemInDir } from '@tauri-apps/plugin-opener'
import { disposeFleet, forgetSession, initFleet } from './fleet'
import { initTransfers, pendingDrop, transfers } from './transfers'
import { ai, ask, initAi, loadAiSettings, setTailProvider, setActiveSlot, aiReady, aiDisabledReason } from './ai'
import TerminalPane from './components/TerminalPane.vue'
import MonitorPanel from './components/MonitorPanel.vue'
import AiPanel from './components/AiPanel.vue'
import HistoryPanel from './components/HistoryPanel.vue'
import OverviewPanel from './components/OverviewPanel.vue'
import LabPanel from './components/LabPanel.vue'
import SftpPanel from './components/SftpPanel.vue'
import ForwardPanel from './components/ForwardPanel.vue'
import QuickCommands from './components/QuickCommands.vue'
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
    /** 连接代次：手动重试/新调度会让旧挂起连接的结果作废（防晚到结果污染状态） */
    connectGen?: number
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
const labOpen = ref(false)
const sftpOpen = ref(false)
const forwardOpen = ref(false)
/** 标签拖拽排序：拖到哪儿，标签就插到哪儿 */
const dragTab = ref<{ id: string; from: number; x: number } | null>(null)
const dragOverIdx = ref(-1)

/** 广播输入：把键盘输入同时发到选中的多个会话（生产机上要小心，所以有红色横幅） */
const broadcastOn = ref(false)
const broadcastSel = ref<string[]>([])
/** 进行中的传输数，显示在"文件"按钮上的小角标 */
const activeTransferCount = computed(() => transfers.value.filter((t) => t.state === 'running').length)
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
  // 传输进度是全局的：面板关掉再打开还要能看到进度条
  await initTransfers()
  // AI：订阅流式事件 + 读一遍配置（决定 AI 入口是否可用）
  await initAi()
  await loadAiSettings()
  setTailProvider(() => currentTerm()?.getTail?.() ?? '')
  void initDrop()
  try {
    await loadAll()
  } catch (e) {
    toast('error', `初始化失败: ${(e as Error).message}`)
  }
  listen<{ sid: string; host_id?: string; label?: string }>('ssh://closed', (ev) => {
    const t = tabs.value.find((x) => x.sid === ev.payload.sid)
    if (t) markClosed(t)
  })
  listen<{ sid: string; alert: Alert; resolved?: boolean }>('ssh://alert', (ev) => {
    // 解除事件不弹窗、不发系统通知：总览徽标熄灭 + 「告警 N」递减就是提示。
    if (ev.payload.resolved) return
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
  unlistenDrop?.()
})

/**
 * 外部文件拖入（从资源管理器拖进来）。
 *
 * Tauri v2 默认开着原生拖放，这会吞掉 webview 内部的 HTML5 拖拽事件——所以
 * 面板内部拖拽是自绘的（见 SftpPanel），外部拖入走这条原生事件：
 * - SFTP 面板开着 → 交给面板，上传到远端当前目录
 * - 否则 → 上传到远端 `~/sshbox-uploads/` 并把远端路径粘到终端里（MobaXterm 的顺手细节）
 */
let unlistenDrop: (() => void) | null = null

async function initDrop() {
  const { getCurrentWebview } = await import('@tauri-apps/api/webview')
  unlistenDrop = await getCurrentWebview().onDragDropEvent(async (ev) => {
    if (ev.payload.type !== 'drop') return
    const paths = ev.payload.paths ?? []
    if (!paths.length) return
    const tab = activeTab.value
    if (!tab) return
    if (sftpOpen.value) {
      pendingDrop.value = [...pendingDrop.value, ...paths]
      return
    }
    try {
      const remotes = await api.sftpUploadDrop(tab.sid, paths)
      if (remotes.length) {
        // 引号包住路径：远端 shell 里带空格/中文的路径才不会断成两个参数
        await api.termWrite(tab.sid, remotes.map((r) => `'${r}'`).join(' ') + ' ')
      }
      toast('info', `已上传 ${remotes.length} 个文件到远端 ~/sshbox-uploads/`)
    } catch (e) {
      toast('error', `上传失败: ${(e as Error).message}`)
    }
  })
}

// --- AI（v0.5） --------------------------------------------------------------
/** 终端实例按 sid 存：AI 要读「当前会话」的选中文本和最近输出。 */
type TermHandle = {
  getSelection?: () => string
  getTail?: (n?: number) => string
  copySelection?: () => Promise<boolean>
  pasteClipboard?: () => Promise<boolean>
  selectAllText?: () => void
  clearScreen?: () => void
}
const termRefs = new Map<string, TermHandle>()
function setTermRef(sid: string, el: unknown) {
  if (el) termRefs.set(sid, el as TermHandle)
  else termRefs.delete(sid)
}

/**
 * 右键菜单状态。**sid 一起记下来**：菜单要作用在「右键的那块终端」上，
 * 而不是「当前激活的标签」—— 多标签下这两者不一定是一回事。
 */
const aiMenu = ref<{ x: number; y: number; selection: string; sid: string } | null>(null)
const reportBusy = ref(false)

function toggleAi() {
  ai.open = !ai.open
  if (ai.open && !aiReady()) ai.error = aiDisabledReason()
}

/** 终端右键：记下坐标、选中文本和属于哪个会话，弹菜单。 */
function onTermContext(payload: { x: number; y: number; selection: string }, sid: string) {
  aiMenu.value = { ...payload, sid }
}

const aiMenuEl = ref<HTMLElement>()

// 菜单定位后按视口边界回移：终端靠边/靠底右键时菜单别被切掉一半
watch(aiMenu, async (v) => {
  if (!v) return
  await nextTick()
  const el = aiMenuEl.value
  if (!el) return
  const r = el.getBoundingClientRect()
  const pad = 8
  let { x, y } = v
  if (x + r.width > window.innerWidth - pad) x = window.innerWidth - r.width - pad
  if (y + r.height > window.innerHeight - pad) y = window.innerHeight - r.height - pad
  if (x !== v.x || y !== v.y) aiMenu.value = { ...v, x, y }
})

function termBanner(kind: 'error' | 'info', text: string) {
  banner.value = { kind, text }
  if (kind === 'info') window.setTimeout(() => (banner.value = null), 4000)
}

/** 菜单里的复制/粘贴/全选/清屏都作用于右键的那块终端。 */
async function copyFromTerm() {
  const sid = aiMenu.value?.sid ?? ''
  aiMenu.value = null
  const ok = await termRefs.get(sid)?.copySelection?.()
  termBanner(ok ? 'info' : 'error', ok ? '已复制到剪贴板' : '没有选中内容 —— 先拖选一段再复制')
}

async function pasteToTerm() {
  const sid = aiMenu.value?.sid ?? ''
  aiMenu.value = null
  const ok = await termRefs.get(sid)?.pasteClipboard?.()
  // pasteClipboard 返回 false 有三种原因：剪贴板空 / 系统不给读 / 广播模式下弹了确认框等用户点。
  // 早先一律报「剪贴板是空的」—— 广播时那句话是假的，用户会以为剪贴板坏了。
  const tab = tabs.value.find((t) => t.sid === sid)
  const bc = !!tab && broadcastOn.value && broadcastSel.value.includes(tab.id)
  if (ok) termBanner('info', '已粘贴（没有回车，确认后自己按）')
  else if (bc) termBanner('info', '广播模式：粘贴只作用于当前终端，请在弹窗里点确认')
  else termBanner('error', '剪贴板是空的，或系统不允许读取')
}

function selectAllInTerm() {
  const sid = aiMenu.value?.sid ?? ''
  aiMenu.value = null
  termRefs.get(sid)?.selectAllText?.()
}

function clearTerm() {
  const sid = aiMenu.value?.sid ?? ''
  aiMenu.value = null
  termRefs.get(sid)?.clearScreen?.()
}

function currentTerm() {
  const sid = activeTab.value?.sid
  return sid ? termRefs.get(sid) : undefined
}

function openAiFor(kind: 'explain' | 'command', selection: string, prompt = '', sid?: string) {
  aiMenu.value = null
  ai.open = true
  if (!aiReady()) {
    ai.error = aiDisabledReason()
    return
  }
  void ask(kind, {
    selection,
    prompt,
    sid: sid ?? activeTab.value?.sid ?? undefined,
  })
}

function explainSelection() {
  const sid = aiMenu.value?.sid
  openAiFor('explain', aiMenu.value?.selection ?? '', '', sid)
}

function askCommandFromSelection() {
  const sel = aiMenu.value?.selection ?? ''
  const sid = aiMenu.value?.sid
  aiMenu.value = null
  ai.open = true
  // 选中内容当"需求描述"的默认值，用户可以在面板里改
  void ask('command', { prompt: sel.trim() || '', sid: sid ?? activeTab.value?.sid ?? undefined })
}

/** 把 AI 生成的命令填进终端 —— **不自动回车**，由用户确认。
 *
 * 命令是给某一台主机生成的（`sid`），所以优先填回那个会话并切到它的标签：
  * 用户在别的标签上双击历史里的命令时，不该把 A 机的命令贴到 B 机。
  * `execute: true`（总览故障注入的「立即执行」）：不切标签，直接向该会话
  * pty 写入命令 + 回车 —— 自动执行，界面停留在总览。 */
 function insertToTerminal(p: { text: string; sid?: string; execute?: boolean }) {
   const body = (p.text ?? '').trim()
   if (!body) return
   const want = (p.sid ?? '').trim()
   let tab = activeTab.value
   if (want && tab?.sid !== want) {
     const target = tabs.value.find((t) => t.sid === want)
     if (!target) {
       toast('error', '生成这条命令的会话已关闭，没有填入终端')
       return
     }
     tab = target
     if (!p.execute) focusTab(want)
   }
   if (!tab) {
     toast('error', '还没有连接的主机 —— 先双击左侧主机建立会话，再插入命令')
     return
   }
   if (p.execute) {
     void api.termWrite(tab.sid, body + '\n')
     toast('info', `已执行：${body.split('\n').pop()?.slice(0, 40)}`)
   } else {
       void api.termWrite(tab.sid, body)
       toast('info', `命令已填入 ${tab.label}，确认后按回车执行`)
     }
   }

/** 一键巡检报告。hours: 1/24/168，默认 24。 */
async function makeReport(hours = reportHours.value) {
  const tab = activeTab.value
  if (!tab) return
  reportPickOpen.value = false
  reportBusy.value = true
  try {
    const r = await api.reportGenerate(tab.sid, hours)
    const crit = r.critical > 0 ? `，其中 ${r.critical} 项严重` : ''
    const aiNote = r.ai_used ? '，含 AI 结论' : r.ai_error ? `（AI 结论跳过：${r.ai_error}）` : ''
    toast('info', `报告已生成：${r.findings} 项结论${crit}${aiNote}`)
    reportResult.value = r
  } catch (e) {
    toast('error', `生成报告失败：${(e as Error).message}`)
  } finally {
    reportBusy.value = false
  }
}

const reportResult = ref<import('./api').ReportResult | null>(null)
const reportHours = ref(24)
/** 报告按钮的弹窗：点「报告」先选时间范围再生成。 */
const reportPickOpen = ref(false)

function onKey(e: KeyboardEvent) {
  // Esc 优先退广播：这是最容易误操作的模式，先给它
  if (e.key === 'Escape' && broadcastOn.value) {
    toggleBroadcast()
    return
  }
  // Esc 关掉最上面那层浮层。总览/历史/设置/转发/SFTP 都是全屏浮层，只有鼠标能关
  // （点遮罩或 ×）—— 键盘用户按 Esc 没反应，会以为界面卡住了。从最上层往下找。
  if (e.key === 'Escape') {
    if (reportPickOpen.value) {
      reportPickOpen.value = false
      return
    }
    for (const [open, close] of [
      [settingsOpen, () => (settingsOpen.value = false)],
      [overviewOpen, () => (overviewOpen.value = false)],
      [historyOpen, () => (historyOpen.value = false)],
      [forwardOpen, () => (forwardOpen.value = false)],
      [sftpOpen, () => (sftpOpen.value = false)],
    ] as [Ref<boolean>, () => void][]) {
      if (open.value) {
        close()
        e.preventDefault()
        return
      }
    }
  }
  if (!e.ctrlKey) return
  if (e.shiftKey && (e.key === 'B' || e.key === 'b')) {
    e.preventDefault()
    toggleBroadcast()
    return
  }
  if (e.shiftKey && (e.key === 'P' || e.key === 'p')) {
    e.preventDefault()
    window.dispatchEvent(new Event('sshbox-quickcmd-toggle'))
    return
  }
  if (e.shiftKey && (e.key === 'F' || e.key === 'f')) {
    e.preventDefault()
    if (activeTab.value) sftpOpen.value = !sftpOpen.value
    return
  }
  if (e.shiftKey && (e.key === 'I' || e.key === 'i')) {
    e.preventDefault()
    toggleAi()
    return
  }
  if (e.shiftKey && (e.key === 'T' || e.key === 't')) {
    e.preventDefault()
    if (activeTab.value) forwardOpen.value = !forwardOpen.value
    return
  }
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

// --- 标签拖拽排序 ---------------------------------------------------------

/**
 * 自绘拖拽：Tauri v2 默认开着原生拖放，Windows 上它会吞掉 webview 内的
 * HTML5 拖拽事件（draggable/dragstart 收不到），所以这里用指针事件自己算。
 * 按下超过 4px 才算拖，避免和"点一下切换标签"打架。
 */
function onTabDown(i: number, ev: MouseEvent) {
  if (ev.button !== 0) return
  const id = tabs.value[i]?.id
  if (!id) return
  const startX = ev.clientX
  let dragging = false

  const move = (e: MouseEvent) => {
    if (!dragging && Math.abs(e.clientX - startX) < 4) return
    dragging = true
    dragTab.value = { id, from: i, x: e.clientX }
    dragOverIdx.value = tabIndexAt(e.clientX)
  }
  const up = () => {
    window.removeEventListener('mousemove', move)
    window.removeEventListener('mouseup', up)
    const target = dragOverIdx.value
    if (dragging && target >= 0 && target !== i) {
      const list = [...tabs.value]
      const [moved] = list.splice(i, 1)
      list.splice(target, 0, moved)
      tabs.value = list
      activeIdx.value = target
      uiLog(`标签「${moved.label}」从 ${i} 移到 ${target}`)
    }
    dragTab.value = null
    dragOverIdx.value = -1
  }
  window.addEventListener('mousemove', move)
  window.addEventListener('mouseup', up)
}

/** 按 X 坐标找插入位置：拿每个标签的中点做分界。 */
function tabIndexAt(x: number): number {
  const els = [...document.querySelectorAll('.tab')] as HTMLElement[]
  for (let i = 0; i < els.length; i++) {
    const r = els[i].getBoundingClientRect()
    if (x < r.left + r.width / 2) return i
  }
  return els.length - 1
}

// --- 广播输入 -------------------------------------------------------------

function toggleBroadcast() {
  broadcastOn.value = !broadcastOn.value
  if (broadcastOn.value && !broadcastSel.value.length && activeTab.value) {
    // 默认选中当前标签，避免"开了广播但一个都没选"的哑状态
    broadcastSel.value = [activeTab.value.id]
  }
  if (!broadcastOn.value) broadcastSel.value = []
  uiLog(`广播输入${broadcastOn.value ? '开启' : '关闭'}，选中 ${broadcastSel.value.length} 个会话`)
}

function toggleBroadcastTab(id: string) {
  const i = broadcastSel.value.indexOf(id)
  if (i >= 0) broadcastSel.value.splice(i, 1)
  else broadcastSel.value.push(id)
}

/** 终端输入：广播模式下发给所有选中会话，否则只发给当前会话。 */
function onTermData(tab: Tab, data: string) {
  if (broadcastOn.value && broadcastSel.value.includes(tab.id)) {
    for (const id of broadcastSel.value) {
      const t = tabs.value.find((x) => x.id === id)
      if (t && t.status === 'connected') void api.termWrite(t.sid, data).catch(() => {})
    }
    return
  }
  void api.termWrite(tab.sid, data).catch(() => {})
}

/** 快捷命令：广播开着就跟着广播走（批量运维时很自然）。 */
function onQuickSend(text: string) {
  const tab = activeTab.value
  if (!tab) return
  if (broadcastOn.value && broadcastSel.value.includes(tab.id)) {
    for (const id of broadcastSel.value) {
      const t = tabs.value.find((x) => x.id === id)
      if (t && t.status === 'connected') void api.termWrite(t.sid, text).catch(() => {})
    }
    return
  }
  void api.termWrite(tab.sid, text).catch(() => {})
}

/**
 * 启动主机上标记了 auto_start 的转发规则。
 *
 * 失败只提示不阻断：某条规则端口被占不该影响连接本身。
 */
async function autoStartForwards(sid: string) {
  try {
    const rules = await api.forwardList(sid)
    for (const r of rules.filter((x) => x.auto_start && !x.running)) {
      try {
        await api.forwardStart(sid, r)
        uiLog(`自动启动转发 ${r.listen_host}:${r.listen_port} → ${r.target_host}:${r.target_port}`)
      } catch (e) {
        toast('error', `转发自动启动失败: ${(e as Error).message}`)
      }
    }
  } catch {
    /* 手输的临时会话没有 host_id，读不到规则很正常 */
  }
}

// --- connecting ------------------------------------------------------------
function addTab(sid: string, host?: Host, label?: string, id?: string) {
  // 连上就把这台机器上标记"自动启动"的转发拉起来（重连也走这条路）
  void autoStartForwards(sid)
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
      // 代次：重连路径（tabId）上每次发起连接都 +1，旧连接晚到的成功结果作废
      const myGen = (() => {
        if (!opts.tabId) return 0
        const tab = tabs.value.find((x) => x.id === opts.tabId)
        const g = (tab?.connectGen ?? 0) + 1
        if (tab) tab.connectGen = g
        return g
      })()
      const sid = await api.connectHost({
        hostId: host.id,
        password: opts.password,
        keyPassphrase: opts.keyPassphrase,
        acceptHostKey: opts.acceptHostKey,
        savePassword: opts.savePassword,
      })
      if (opts.tabId) {
        const cur = tabs.value.find((x) => x.id === opts.tabId)
        if (!cur || cur.connectGen !== myGen) {
          uiLog('本轮连接结果已过期（有更新的重试），忽略以免覆盖新状态')
          return
        }
      }
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
          // 重连路径的失败（含 25s 超时）不能让它停在"重连中"：转回已断开 + 继续调度
          if (opts.tabId) {
            const t = tabs.value.find((x) => x.id === opts.tabId)
            if (t) {
              t.status = 'closed'
              t.nextRetryIn = undefined
              const wantAuto = (settings.value?.auto_reconnect ?? true) && (t.host?.auto_reconnect ?? true) && !!t.host
              if (wantAuto) scheduleReconnect(t)
            }
          }
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

// 页签切换 → AI 槽联动（所有切标签路径都走 activeIdx，一处 watch 全覆盖）
watch(activeIdx, (i) => {
  setActiveSlot(tabs.value[i]?.id ?? '')
})

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
  t.nextRetryIn = undefined
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
    // 会话没了：SFTP 通道缓存和本地转发监听都一起收掉
    void api.sftpForget(t.sid).catch(() => {})
    void api.forwardStopAll(t.sid).catch(() => {})
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

/** 侧边栏拖动调整分组顺序：写回 hosts.json 的 groups 数组。 */
async function reorderGroups(order: string[]) {
  try {
    await api.groupsReorder(order)
    hosts.value = await api.hostsList()
    toast('info', '分组顺序已保存')
  } catch (e) {
    toast('error', `保存分组顺序失败: ${(e as Error).message}`)
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

/** 进程列表条数（0 = 全部）。改完要让监控任务重来一次，否则要等下一轮才生效。 */
async function setProcessTopN(n: number) {
  if (!settings.value) return
  const next = { ...settings.value, process_top_n: n }
  try {
    await api.settingsSet(next)
    settings.value = next
    if (activeTab.value) await api.monitorRestart(activeTab.value.sid).catch(() => {})
    toast('info', n === 0 ? '进程列表已改为「全部」' : `进程列表已改为前 ${n} 个`)
  } catch (e) {
    toast('error', `改进程条数失败: ${(e as Error).message}`)
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
      @reorder-groups="reorderGroups"
    />

    <main class="main">
      <div class="tabbar">
        <div
          v-for="(t, i) in tabs"
          :key="t.id"
          class="tab"
          :class="{
            active: i === activeIdx,
            bc: broadcastOn && broadcastSel.includes(t.id),
            dragging: dragTab?.id === t.id,
            'drop-target': dragTab && dragOverIdx === i && dragTab.id !== t.id,
          }"
          :title="'按住拖动可调整标签顺序'"
          @click="activeIdx = i"
          @mousedown="onTabDown(i, $event)"
        >
          <input
            v-if="broadcastOn"
            class="bc-check"
            type="checkbox"
            :checked="broadcastSel.includes(t.id)"
            title="勾选后这条会话接收广播输入"
            @click.stop="toggleBroadcastTab(t.id)"
          />
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
                      v-if="activeTab?.status === 'closed' || activeTab?.status === 'connecting'"
                      class="reconnect-btn"
                      :title="activeTab?.status === 'connecting' ? '卡在重连中可手动重试（后端连接 25s 超时兜底）' : '重新连接该主机'"
                      @click="manualReconnect(activeTab)"
                    >{{ activeTab?.status === 'connecting' ? '立即重试' : '重连' }}</button>
          <button
            class="icon-btn"
            :title="`主题：${THEME_LABELS[themeMode]}（点击切到${resolvedTheme === 'dark' ? '浅色' : '深色'}）`"
            @click="toggleTheme"
          >{{ resolvedTheme === 'dark' ? '☾' : '☀' }}</button>
          <button
            class="icon-btn"
            title="文件传输（SFTP 双栏，Ctrl+Shift+F）"
            @click="sftpOpen = !sftpOpen"
          >
            文件<span v-if="activeTransferCount" class="badge">{{ activeTransferCount }}</span>
          </button>
          <button
            class="icon-btn"
            :class="{ 'bc-on': broadcastOn }"
            :title="
              broadcastOn
                ? '关闭广播输入（Esc）'
                : '多会话广播输入：一次输入发给多个会话（Ctrl+Shift+B）'
            "
            @click="toggleBroadcast"
          >
            广播
          </button>
          <button
            class="icon-btn"
            title="端口转发（Ctrl+Shift+T）"
            @click="forwardOpen = !forwardOpen"
          >
            转发
          </button>
          <button class="icon-btn" title="总览（所有已连接主机）" @click="overviewOpen = true">
                      总览
                    </button>
                    <button class="icon-btn" title="故障演练台（注入/实时观测/就地执行，不跳页）" @click="labOpen = true">
                      演练台
                    </button>
          <button class="icon-btn" title="历史回看（落盘数据）" @click="historyOpen = true">
            历史
          </button>
          <button
            class="icon-btn"
            :class="{ 'bc-on': ai.open }"
            :title="aiReady() ? 'AI 助手（Ctrl+Shift+I）' : aiDisabledReason()"
            @click="toggleAi()"
          >
            AI
          </button>
          <span class="report-wrap">
                      <button
                        class="icon-btn"
                        :title="activeTab ? '生成这台机器的巡检报告（Markdown + HTML）' : '先连接一台主机'"
                        :disabled="!activeTab || reportBusy"
                        @click="reportBusy ? 0 : (reportPickOpen = !reportPickOpen)"
                      >
                        {{ reportBusy ? '生成中…' : '报告' }}
                      </button>
                      <div v-if="reportPickOpen" class="report-pick">
                        <div class="rp-title">报告时间范围</div>
                        <button class="rp-opt" @click="makeReport(1)">最近 1 小时</button>
                        <button class="rp-opt" @click="makeReport(24)">最近 24 小时</button>
                        <button class="rp-opt" @click="makeReport(168)">最近 7 天</button>
                        <button class="rp-opt cancel" @click="reportPickOpen = false">取消</button>
                        <div class="rp-note">三档数据都会存进报告，打开后可随意切换；想回顾更长时间段，这里就选 7 天</div>
                      </div>
                    </span>
          <button class="icon-btn" title="切换监控面板" @click="monitorVisible = !monitorVisible">
            {{ monitorVisible ? '◧' : '◨' }}
          </button>
        </div>
      </div>

      <div v-if="banner" class="banner" :class="banner.kind">
        <span>{{ banner.text }}</span>
        <button @click="banner = null">×</button>
      </div>

      <div v-if="!labOpen" class="content">
        <div class="term-area">
          <QuickCommands
            v-if="activeTab"
            :commands="settings?.quick_commands ?? []"
            :host="activeTab.host?.host"
            :user="activeTab.host?.username"
            :port="activeTab.host?.port"
            @send="onQuickSend"
          />
          <div v-if="broadcastOn" class="bc-bar">
            <b>广播输入已开启</b> —— 键盘输入会同时发给 {{ broadcastSel.length }} 个会话
            <span class="bc-hint">在标签上勾选目标；Esc 退出</span>
          </div>
          <div class="term-stack">
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
                      :broadcast-count="broadcastOn ? broadcastSel.length : 0"
                      :ref="(el) => setTermRef(t.sid, el)"
                      @data="(d) => onTermData(t, d)"
                      @context="(p) => onTermContext(p, t.sid)"
                    />
          </div>
        </div>
        <AiPanel
                  v-if="ai.open"
                  :sid="activeTab?.sid"
                  :active-label="activeTab?.label"
                  @insert="insertToTerminal"
                  @close="ai.open = false"
                />

        <div v-if="monitorVisible && activeTab" class="monitor-area">
          <!-- key 绑定 sid：切标签 / 关标签时重建面板。否则组件被复用，
               静态信息（主机名等一次性事件）和图表历史都还是上一台主机的，
               看起来就是「监控没跟着切」。 -->
          <MonitorPanel
                      :key="activeTab.sid"
                      :sid="activeTab.sid"
                      :active="true"
                      :interval="settings?.sample_interval_secs ?? 2"
                      :top-n="settings?.process_top_n ?? 12"
                      @set-interval="setSampleInterval"
                      @set-top-n="setProcessTopN"
                      @close="monitorVisible = false"
                    />
        </div>
      </div>
    </main>

        <LabPanel
              v-if="labOpen"
              class="lab-page"
              :tabs="tabs"
              :hosts="hosts"
              @close="labOpen = false"
              @enter="monitorVisible = false"
              @command="insertToTerminal"
              @connect="connectFromOverview"
            />

        <div v-if="reportResult" class="report-card">
      <div class="rc-title">
        巡检报告已生成
        <span class="rc-sub">{{ reportResult.findings }} 项结论<template v-if="reportResult.critical">，{{ reportResult.critical }} 项严重</template></span>
      </div>
      <div class="rc-path mono">{{ reportResult.markdown_path }}</div>
      <div class="rc-actions">
        <button class="rc-btn primary" @click="api.reportReveal(reportResult.markdown_path)">打开文件夹</button>
        <button class="rc-btn" @click="api.reportReveal(reportResult.html_path)">定位 HTML</button>
        <button class="rc-btn" @click="reportResult = null">关闭</button>
      </div>
      <div v-if="reportResult.ai_error" class="rc-warn">AI 结论未生成：{{ reportResult.ai_error }}</div>
    </div>

    <div
          v-if="aiMenu"
          ref="aiMenuEl"
          class="ai-ctx"
          :style="{ left: aiMenu.x + 'px', top: aiMenu.y + 'px' }"
          @click.stop
        >
      <!-- 复制粘贴是基本盘，永远排在最上面；AI 是增强，永远在分隔线下面。
           没选中时「复制」置灰并说明原因，不做一个点了没反应的按钮。 -->
      <div class="ai-ctx-item" :class="{ dim: !aiMenu.selection.trim() }" @click="copyFromTerm()">
        复制<span class="ai-ctx-key">Ctrl+C</span>
      </div>
      <div class="ai-ctx-item" @click="pasteToTerm()">
        粘贴<span class="ai-ctx-key">Ctrl+V</span>
      </div>
      <div class="ai-ctx-sep"></div>
      <div class="ai-ctx-item" @click="explainSelection()">
        解释这段（AI）{{ aiMenu.selection.trim() ? '' : '· 最近的输出' }}
      </div>
      <div class="ai-ctx-item" @click="askCommandFromSelection()">用自然语言生成命令…（AI）</div>
      <div class="ai-ctx-sep"></div>
      <div class="ai-ctx-item" @click="selectAllInTerm()">全选</div>
      <div class="ai-ctx-item" @click="clearTerm()">清屏</div>
      <div class="ai-ctx-sep"></div>
      <div class="ai-ctx-item" @click="aiMenu = null">取消</div>
    </div>

    <SftpPanel
      v-if="!labOpen && sftpOpen && activeTab"
      :sid="activeTab.sid"
      :label="`${activeTab.label} · ${activeTab.host ? activeTab.host.username + '@' + activeTab.host.host : ''}`"
      @close="sftpOpen = false"
    />

    <ForwardPanel
      v-if="!labOpen && forwardOpen && activeTab"
      :sid="activeTab.sid"
      :label="`${activeTab.label} · ${activeTab.host ? activeTab.host.username + '@' + activeTab.host.host : ''}`"
      @close="forwardOpen = false"
    />

    <OverviewPanel
          v-if="!labOpen && overviewOpen"
          :tabs="tabs"
          :hosts="hosts"
          :interval="settings?.sample_interval_secs ?? 2"
          :active-sid="activeTab?.sid"
          @close="overviewOpen = false"
                    @focus="focusTab"
                    @connect="connectFromOverview"
                  />

    <HistoryPanel
      v-if="!labOpen && historyOpen"
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
      v-if="!labOpen && settingsOpen && settings"
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
      @ai-changed="loadAiSettings"
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
.badge {
  display: inline-block;
  min-width: 15px;
  margin-left: 4px;
  padding: 0 4px;
  border-radius: 8px;
  background: var(--ctp-blue);
  color: var(--on-accent);
  font-size: 10px;
  line-height: 15px;
  text-align: center;
}

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
.report-wrap { position: relative; }
.report-pick {
  position: absolute;
  top: calc(100% + 6px);
  right: 0;
  z-index: 60;
  background: var(--ctp-mantle);
  border: 1px solid var(--ctp-surface1);
  border-radius: 8px;
  box-shadow: 0 6px 20px rgba(0, 0, 0, 0.35);
  padding: 6px;
  min-width: 140px;
}
.report-pick .rp-title { font-size: 10px; color: var(--ctp-overlay0); padding: 4px 8px 6px; }
.report-pick .rp-opt {
  display: block;
  width: 100%;
  text-align: left;
  background: none;
  border: none;
  color: var(--ctp-text);
  font-size: 12px;
  padding: 6px 8px;
  border-radius: 5px;
  cursor: pointer;
}
.report-pick .rp-opt:hover { background: var(--ctp-surface0); }
.report-pick .rp-opt.cancel { color: var(--ctp-overlay0); border-top: 1px solid var(--ctp-surface0); border-radius: 0 0 5px 5px; margin-top: 4px; }
.report-pick .rp-note {
  font-size: 10px; color: var(--ctp-overlay0); line-height: 1.5;
  padding: 6px 8px 4px; border-top: 1px solid var(--ctp-surface0); margin-top: 4px;
}
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
/* 终端右键菜单（解释这段 / 生成命令） */
.ai-ctx {
  position: fixed; z-index: 220; background: var(--ctp-base); border: 1px solid var(--ctp-surface0);
  border-radius: 6px; padding: 4px; min-width: 190px; box-shadow: 0 6px 20px var(--mask);
}
.ai-ctx-item { padding: 6px 10px; font-size: 12px; color: var(--ctp-text); border-radius: 4px; cursor: pointer; }
.ai-ctx-item:hover { background: var(--ctp-surface0); }
.ai-ctx-item.dim { color: var(--ctp-overlay0); }
.ai-ctx-item.dim:hover { background: transparent; cursor: default; }
.ai-ctx-key { float: right; margin-left: 16px; color: var(--ctp-overlay0); font-size: 11px; }
.ai-ctx-sep { height: 1px; background: var(--ctp-surface0); margin: 4px 2px; }
/* 报告生成后的浮层卡片 */
.report-card {
  position: fixed; right: 18px; bottom: 18px; z-index: 220; width: 420px;
  background: var(--ctp-base); border: 1px solid var(--ctp-surface1); border-radius: 8px;
  padding: 10px 12px; box-shadow: 0 8px 28px var(--mask);
}
.rc-title { font-size: 12.5px; color: var(--ctp-text); font-weight: 600; display: flex; align-items: baseline; gap: 8px; }
.rc-sub { font-size: 11px; color: var(--ctp-overlay0); font-weight: 400; }
.rc-path { font-size: 10.5px; color: var(--ctp-overlay0); margin: 6px 0; word-break: break-all; }
.rc-actions { display: flex; gap: 6px; }
.rc-btn {
  background: var(--ctp-crust); border: 1px solid var(--ctp-surface1); color: var(--ctp-subtext0);
  border-radius: 4px; font-size: 11px; padding: 3px 9px; cursor: pointer;
}
.rc-btn.primary { background: var(--ctp-blue); color: var(--on-accent); border: none; font-weight: 600; }
.rc-warn { font-size: 10.5px; color: var(--ctp-yellow); margin-top: 6px; line-height: 1.5; }
.term-area {
  flex: 1;
  min-width: 0;
  background: var(--ctp-base);
  display: flex;
  flex-direction: column;
  min-height: 0;
}
.term-stack { flex: 1; min-height: 0; position: relative; }
.bc-bar {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 5px 10px;
  background: var(--ctp-red);
  color: var(--on-accent);
  font-size: 12px;
}
.bc-hint { opacity: 0.85; }
.icon-btn.bc-on {
  background: var(--ctp-red);
  color: var(--on-accent);
  border-color: var(--ctp-red);
}
.tab.bc {
  box-shadow: inset 0 -2px 0 var(--ctp-red);
}
.tab.dragging {
  opacity: 0.55;
}
.tab.drop-target {
  outline: 2px solid var(--ctp-blue);
  outline-offset: -2px;
}
.bc-check { margin-right: 4px; }
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
