<script setup lang="ts">
/**
 * Fleet overview: every connected session on one screen, plus (opt-in) the
 * last known state of hosts that are not connected right now.
 *
 * Live rows come from the global `fleet` layer — no extra SSH connections.
 * Offline rows come from the history database, and they say how old they are:
 * a monitoring view that hides the age of its numbers is worse than no view.
 */
import { computed, onMounted, onBeforeUnmount, ref, watch } from 'vue'
import { api, type Alert, type HistoryHost, type HistoryRange, type HostsFile, type Metrics, type PingRange } from '../api'
import { alertFor, fleetNow, sampleFor } from '../fleet'

const props = defineProps<{
  tabs: {
    id: string
    sid: string
    label: string
    hostId?: string | null
    status: 'connected' | 'closed' | 'connecting'
  }[]
  hosts: HostsFile
  /** Live sample interval; hidden tabs sample at 5× this. */
  interval: number
  activeSid?: string
}>()

const emit = defineEmits<{
  (e: 'close'): void
  (e: 'focus', sid: string): void
  (e: 'connect', hostId: string): void
  (e: 'command', p: { sid: string; text: string }): void
}>()

// --- 故障注入（#12）--------------------------------------------------------
// 只生成 tc netem 命令、按目标填入对应终端，回车由用户在终端里按（铁律：不自动执行）。
const injectOpen = ref(false)
const injectTarget = ref<'all' | 'group' | 'host'>('all')
const injectGroup = ref('')
const injectHost = ref('')
const injectType = ref<'delay' | 'loss' | 'blip' | 'clear'>('delay')
const injectMs = ref(200)
const injectLoss = ref(5)
const injectBlipEvery = ref(10)
const injectBlipDur = ref(3)
const injectBlipTimes = ref(3)
const injectNic = ref('eth0')
const injectRecover = ref(false)
const injectRecoverSecs = ref(120)
const injectMsg = ref('')

const connectedTabs = computed(() => props.tabs.filter((t) => t.status === 'connected'))
const connectedCount = computed(() => connectedTabs.value.length)
const groupOptions = computed(() => props.hosts.groups.filter(Boolean))
const injectGroupCount = computed(() => {
  if (!injectGroup.value) return 0
  return connectedTabs.value.filter((t) => t.hostId != null && groupOfHost(t.hostId) === injectGroup.value).length
})
const injectTargetCount = computed(() => {
  if (injectTarget.value === 'all') return connectedCount.value
  if (injectTarget.value === 'group') return injectGroupCount.value
  return injectHost.value ? 1 : 0
})
const injectCanRun = computed(
  () => injectTargetCount.value > 0 && (injectType.value !== 'blip' || (injectBlipDur.value > 0 && injectBlipTimes.value > 0)),
)

function injectCommand(): string {
  const nic = injectNic.value.trim() || 'eth0'
  const head =
    '# 故障注入（SSHBox 生成）· 回车由你执行；需要 root；网卡名先用 `ip link` 确认'
  if (injectType.value === 'clear') {
    return [
      head,
      `tc qdisc del dev ${nic} root 2>/dev/null; echo '已清除（原本没有规则则无输出）'; tc qdisc show dev ${nic} || true`,
    ].join('\n')
  }
  if (injectType.value === 'delay') {
    const ms = Math.max(1, Math.round(injectMs.value || 1))
    const rule = `delay ${ms}ms`
    return [head, netemLine(nic, rule)].join('\n')
  }
  if (injectType.value === 'loss') {
    const p = Math.min(100, Math.max(1, Math.round(injectLoss.value || 1)))
    return [head, netemLine(nic, `loss ${p}%`)].join('\n')
  }
  const every = Math.max(1, Math.round(injectBlipEvery.value || 1))
  const dur = Math.max(1, Math.round(injectBlipDur.value || 1))
  const times = Math.max(1, Math.round(injectBlipTimes.value || 1))
  return [
    head,
    `for i in $(seq 1 ${times}); do`,
    `  tc qdisc replace dev ${nic} root netem loss 100%`,
    `  sleep ${dur}`,
    `  tc qdisc del dev ${nic} root 2>/dev/null`,
    `  [ $i -lt ${times} ] && sleep ${every}`,
    `done`,
    `echo '闪断注入完成，网络已恢复'; tc qdisc show dev ${nic} || true`,
  ].join('\n')
}

function netemLine(nic: string, rule: string): string {
  if (injectRecover.value && injectRecoverSecs.value > 0) {
    return `tc qdisc replace dev ${nic} root netem ${rule} && sleep ${injectRecoverSecs.value} && tc qdisc del dev ${nic} root && echo '已自动恢复'`
  }
  return `tc qdisc replace dev ${nic} root netem ${rule}`
}

function runInject() {
  let targets = connectedTabs.value
  if (injectTarget.value === 'group') {
    const g = injectGroup.value || groupOptions.value[0] || ''
    targets = connectedTabs.value.filter((t) => t.hostId != null && groupOfHost(t.hostId) === g)
    injectGroup.value = g
  } else if (injectTarget.value === 'host') {
    targets = connectedTabs.value.filter((t) => t.sid === injectHost.value)
  }
  if (!targets.length) {
    injectMsg.value = '失败：目标范围内没有已连接的会话'
    return
  }
  const cmd = injectCommand()
  for (const t of targets) {
    emit('command', { sid: t.sid, text: cmd })
  }
  injectMsg.value = `已把命令填入 ${targets.length} 台主机的终端，回车由你按`
}

type Freshness = 'fresh' | 'stale' | 'dead' | 'unknown'

interface Row {
  key: string
  sid?: string
  hostId?: string | null
  title: string
  subtitle: string
  status: 'connected' | 'closed' | 'connecting' | 'offline'
  metrics?: Metrics
  ageSec?: number
  freshness: Freshness
  /** Last known values when there is no live sample (history rows). */
  lastCpu?: number
  lastMem?: number
  alert?: Alert
}

const includeOffline = ref(false)
const sortBy = ref<'name' | 'cpu' | 'mem' | 'disk' | 'load' | 'status'>('name')
const historyHosts = ref<HistoryHost[]>([])
const historyError = ref('')

// --- 分组视角（集群测试） ---
// '' = 全部主机；组名 = 只看该组；'__none' = 未分组（含临时连接）。
const groupFilter = ref('')
/** 总览主视图：表格（默认）/ 卡片网格（#3，组内状态一眼看）。 */
const viewMode = ref<'table' | 'cards'>('table')
const groupOfHost = (id: string | null | undefined): string | null => {
  if (!id) return null
  const h = props.hosts.hosts.find((x) => x.id === id)
  return h ? h.group || '' : null
}

// --- helpers ---------------------------------------------------------------

function fmtBytes(b: number): string {
  if (b < 1024) return b.toFixed(0) + ' B/s'
  if (b < 1024 * 1024) return (b / 1024).toFixed(1) + ' KB/s'
  return (b / 1024 / 1024).toFixed(1) + ' MB/s'
}
function fmtAge(sec: number): string {
  if (!isFinite(sec)) return '—'
  // The 1s ticker can lag Date.now() by up to a second, which would otherwise
  // render as the nonsense "-0 秒前".
  sec = Math.max(0, sec)
  if (sec < 60) return sec.toFixed(0) + ' 秒前'
  if (sec < 3600) return (sec / 60).toFixed(0) + ' 分钟前'
  if (sec < 86400) return (sec / 3600).toFixed(1) + ' 小时前'
  return (sec / 86400).toFixed(1) + ' 天前'
}
const rowCpu = (r: Row) => r.metrics?.cpu_pct ?? r.lastCpu

function hostLabelOf(id: string | null | undefined): string {
  if (!id) return '未保存的主机'
  const h = props.hosts.hosts.find((x) => x.id === id)
  // 临时 key 现在带连接目标（临时-root@127.0.0.1:22），直接显示出来，
  // 否则总览里一排「临时连接」谁也认不出是哪台机器。
  if (!h && id.startsWith('临时-')) return `临时连接 ${id.slice(3)}`
  return h ? h.name || h.host : id
}
function subtitleOf(id: string | null | undefined): string {
  const h = props.hosts.hosts.find((x) => x.id === id)
  if (!h) return '快速连接（未保存）'
  return `${h.username}@${h.host}${h.port !== 22 ? ':' + h.port : ''}`
}

/**
 * How fresh is this row?
 *
 * The thresholds follow the *expected* cadence, not a fixed number: the active
 * tab samples every `interval`, every other tab at 5× that (the backend slows
 * hidden sessions down to save bandwidth), so a fixed "10 秒就算旧" rule would
 * paint healthy background sessions yellow.
 */
function freshnessOf(ageSec: number | undefined, sid: string | undefined): Freshness {
  if (ageSec == null) return 'unknown'
  const cadence = sid && sid === props.activeSid ? props.interval : props.interval * 5
  if (ageSec <= cadence * 1.6 + 1) return 'fresh'
  if (ageSec <= Math.max(cadence * 4, 90)) return 'stale'
  return 'dead'
}

// --- rows ------------------------------------------------------------------

/** 分组筛选后的行。'' 显示全部；选组时只有组内行 + 组内离线历史行。 */
const filteredRows = computed<Row[]>(() =>
  groupFilter.value
    ? rows.value.filter((r) =>
        groupFilter.value === '__none'
          ? !groupOfHost(r.hostId)
          : groupOfHost(r.hostId) === groupFilter.value,
      )
    : rows.value,
)

/** 组内已保存主机 id（历史对比的数据范围）。 */
const hostIdsInGroup = computed(() =>
  props.hosts.hosts.filter((h) => (h.group || '') === groupFilter.value).map((h) => h.id),
)

const rows = computed<Row[]>(() => {
  const out: Row[] = []
  const liveHostIds = new Set<string>()

  for (const t of props.tabs) {
    const sample = sampleFor(t.sid)
    const ageSec = sample ? (fleetNow.value - sample.at) / 1000 : undefined
    const m = sample?.metrics
    if (t.hostId) liveHostIds.add(t.hostId)
    out.push({
      key: `s-${t.id}`,
      sid: t.sid,
      hostId: t.hostId,
      title: t.label,
      subtitle: subtitleOf(t.hostId),
      status: t.status,
      metrics: m,
      ageSec,
      freshness: t.status === 'connected' ? freshnessOf(ageSec, t.sid) : 'unknown',
      alert: alertFor(t.sid)?.alert,
    })
  }

  if (includeOffline.value) {
    for (const h of historyHosts.value) {
      if (liveHostIds.has(h.host_id)) continue
      out.push({
        key: `h-${h.host_id}`,
        hostId: h.host_id.startsWith('临时-') ? null : h.host_id,
        title: hostLabelOf(h.host_id),
        subtitle: `${h.rows} 行历史数据`,
        status: 'offline',
        ageSec: h.newest ? fleetNow.value / 1000 - h.newest : undefined,
        freshness: 'unknown',
        lastCpu: h.last_cpu,
        lastMem: h.last_mem,
      })
    }
  }

  const visible = includeOffline.value
    ? out
    : out.filter((r) => r.status === 'connected' || r.status === 'connecting')

  const cpu = (r: Row) => r.metrics?.cpu_pct ?? r.lastCpu ?? -1
  const mem = (r: Row) => r.metrics?.mem_pct ?? r.lastMem ?? -1
  const rank = { connected: 0, connecting: 1, closed: 2, offline: 3 } as const
  return [...visible].sort((a, b) => {
      switch (sortBy.value) {
        case 'cpu':
          return cpu(b) - cpu(a)
        case 'mem':
          return mem(b) - mem(a)
        case 'disk':
          return (worstDisk(b.metrics) ?? -1) - (worstDisk(a.metrics) ?? -1)
        case 'load':
          return (b.metrics?.load?.[0] ?? -1) - (a.metrics?.load?.[0] ?? -1)
        case 'status':
          return rank[a.status] - rank[b.status] || a.title.localeCompare(b.title)
        default:
          return a.title.localeCompare(b.title)
      }
    })
})

const summary = computed(() => {
  // 选了组就统计组内（报表跟着当前视角走，不混全局数字）
  const list = groupFilter.value ? filteredRows.value : rows.value
  const online = list.filter((r) => r.status === 'connected').length
  const connecting = list.filter((r) => r.status === 'connecting').length
  const stale = list.filter((r) => r.status === 'connected' && r.freshness !== 'fresh').length
  const alarmed = list.filter((r) => r.alert).length
  const offline = groupFilter.value
    ? list.filter((r) => r.status === 'offline').length
    : includeOffline.value
      ? rows.value.filter((r) => r.status === 'offline').length
      : historyHosts.value.filter((h) => !props.tabs.some((t) => t.hostId === h.host_id)).length
  return {
    online,
    connecting,
    stale,
    alarmed,
    offline,
    hosts: groupFilter.value ? list.length : props.tabs.length,
    group: !!groupFilter.value,
  }
})

/** 选组时：组内最忙 / 最闲主机（按当前 CPU）。 */
const groupLeaders = computed(() => {
  if (!groupFilter.value || groupFilter.value === '__none') return null
  const live = filteredRows.value.filter((r) => r.metrics || r.lastCpu != null)
  if (live.length < 2) return null
  const cpu = (r: Row) => r.metrics?.cpu_pct ?? r.lastCpu ?? -1
  const sorted = [...live].sort((a, b) => cpu(b) - cpu(a))
  return { busy: sorted[0], idle: sorted[sorted.length - 1] }
})

/** Highest disk usage across mounts — what "is this box filling up" means. */
function worstDisk(m?: Metrics): number | undefined {
  if (!m?.disks?.length) return undefined
  return Math.max(...m.disks.map((d) => d.use_pct))
}
function netTotals(m?: Metrics): { rx: number; tx: number } {
  if (!m?.net?.length) return { rx: 0, tx: 0 }
  return m.net.reduce((a, n) => ({ rx: a.rx + n.rx_bps, tx: a.tx + n.tx_bps }), { rx: 0, tx: 0 })
}

/** 0.2% must not render as "0%" — that reads like "no data". */
function fmtPct(pct: number | undefined | null): string {
  if (pct == null) return '—'
  return pct < 10 ? pct.toFixed(1) + '%' : pct.toFixed(0) + '%'
}

function barClass(pct: number | undefined): string {
  if (pct == null) return ''
  if (pct >= 90) return 'hot'
  if (pct >= 75) return 'warm'
  return ''
}

// --- data ------------------------------------------------------------------

async function loadHistory() {
  historyError.value = ''
  try {
    historyHosts.value = await api.historyHosts()
  } catch (e) {
    historyError.value = String(e)
  }
}

// --- 组内历史对比（小多图矩阵，选组时出现） --------------------------------
const histKey = ref<'1h' | '24h' | '7d'>('1h')
const histData = ref<Record<string, HistoryRange>>({})
/** 时延/丢包对比（ping 表）。只有网关可达时有值，15s 一档。 */
const pingData = ref<Record<string, PingRange>>({})
/** 矩阵画哪种指标：cpu/cpu+内存 · net · disk · load · ping */
const matrixMetric = ref<'cpu' | 'net' | 'disk' | 'load' | 'ping'>('cpu')
const histBusy = ref(false)
const histError = ref('')
const HIST_WINDOWS = {
  '1h': { span: 3600, points: 60, label: '1 小时' },
  '24h': { span: 86400, points: 288, label: '24 小时' },
  '7d': { span: 7 * 86400, points: 168, label: '7 天' },
} as const

async function loadGroupHistory() {
  if (!groupFilter.value || groupFilter.value === '__none') {
    histData.value = {}
    pingData.value = {}
    return
  }
  const ids = hostIdsInGroup.value
  if (!ids.length) {
    histData.value = {}
    pingData.value = {}
    return
  }
  histBusy.value = true
  histError.value = ''
  const w = HIST_WINDOWS[histKey.value]
  const now = Date.now() / 1000
  try {
    // 指标与时延一起拉：本地库都在毫秒级，切指标不用再等
    const [m, p] = await Promise.all([
      api.historyRangeMulti(ids, now - w.span, now, w.points),
      api.pingRangeMulti(ids, now - w.span, now, w.points).catch(() => ({})),
    ])
    histData.value = m
    pingData.value = p
  } catch (e) {
    histError.value = String(e)
  } finally {
    histBusy.value = false
  }
}

watch(groupFilter, () => void loadGroupHistory())
watch(histKey, () => void loadGroupHistory())

/** 迷你折线 path —— 自绘 SVG，不引 ECharts，小多图矩阵要的就是轻。 */
function sparkPath(vals: number[], w: number, h: number): string {
  if (!vals.length) return ''
  const max = Math.max(...vals, 1)
  const step = w / Math.max(1, vals.length - 1)
  return vals
    .map((v, i) => `${i === 0 ? 'M' : 'L'}${(i * step).toFixed(1)},${(h - 3 - (h - 8) * (v / max)).toFixed(1)}`)
    .join(' ')
}
const cpuSeries = (r: HistoryRange) => r.buckets.map((b) => b.cpu_pct)
const memSeries = (r: HistoryRange) => r.buckets.map((b) => b.mem_pct)
const netSeries = (r: HistoryRange) => r.buckets.map((b) => b.net_rx)
const netTxSeries = (r: HistoryRange) => r.buckets.map((b) => b.net_tx)
const diskRSeries = (r: HistoryRange) => r.buckets.map((b) => b.disk_r)
const diskWSeries = (r: HistoryRange) => r.buckets.map((b) => b.disk_w)
const loadSeries = (r: HistoryRange) => r.buckets.map((b) => b.load1)
/** 时延/丢包：没测到网关的桶是 NULL，整点丢弃 —— 不画 0 ms 骗人。 */
const latSeries = (r?: PingRange) =>
  r?.buckets.filter((b) => b.latency_ms != null).map((b) => b.latency_ms as number) ?? []
const lossSeries = (r?: PingRange) =>
  r?.buckets.filter((b) => b.loss_pct != null).map((b) => b.loss_pct as number) ?? []
const histEmpty = computed(
  () => (matrixMetric.value === 'ping' ? Object.keys(pingData.value) : Object.keys(histData.value)).length === 0,
)

// --- 导出 CSV（#4） ---
const exportMsg = ref('')

async function downloadCsv(filename: string, rows: (string | number)[][]) {
  const esc = (v: string | number) => {
    const s = String(v ?? '')
    if (s.indexOf(',') >= 0 || s.indexOf('"') >= 0) {
      return '"' + s.split('"').join('""') + '"'
    }
    return s
  }
  // BOM 前缀，Excel 打开 UTF-8 中文不乱码
  const csv = '﻿' + rows.map((r) => r.map(esc).join(',')).join(String.fromCharCode(13, 10))
// WebView2 里 <a download> 会被静默拦截（点了没反应），所以走后端落盘，
  // 与历史回看的导出同一条路子（写 %APPDATA%\sshbox\exports\ 并返回路径）。
  try {
    const r = await api.exportCsvText(filename, csv)
    const kb = (r.bytes / 1024).toFixed(1)
    exportMsg.value = `已导出 ${r.rows} 行（${kb} KB）→ ${r.path}`
  } catch (e) {
    exportMsg.value = `导出失败：${(e as Error).message}`
  }
}

function csvDate(): string {
  const d = new Date()
  return `${d.getFullYear()}${String(d.getMonth() + 1).padStart(2, '0')}${String(d.getDate()).padStart(2, '0')}`
}

function exportSnapshotCsv() {
  const name = groupFilter.value === '__none' ? '未分组' : groupFilter.value || '全部'
  const rows: (string | number)[][] = [
    ['主机', '分组', '状态', '年龄(秒)', 'CPU%', '内存%', '磁盘%', '网络↓B/s', '网络↑B/s', '负载1', '进程数'],
  ]
  for (const r of filteredRows.value) {
    rows.push([
      r.title,
      groupOfHost(r.hostId) ?? '未保存',
      r.status,
      r.ageSec ?? '',
      r.metrics?.cpu_pct ?? r.lastCpu ?? '',
      r.metrics?.mem_pct ?? r.lastMem ?? '',
      worstDisk(r.metrics) ?? '',
      Math.round(netTotals(r.metrics).rx),
      Math.round(netTotals(r.metrics).tx),
      r.metrics?.load?.[0] ?? '',
      r.metrics?.proc_total ?? '',
    ])
  }
  downloadCsv(`SSHBox-总览-${name}-${csvDate()}.csv`, rows)
}

/** 只导出**当前已加载**的历史窗口（组内历史矩阵的数据），窗口见 histKey。 */
function exportHistoryCsv() {
  if (!Object.keys(histData.value).length) {
    exportMsg.value = '该组暂无历史数据 —— 先让组内主机在线跑一会儿，再点刷新'
    return
  }
  const rows: (string | number)[][] = [
    ['主机', '时间戳', 'CPU%', '内存%', '网络↓B/s', '网络↑B/s', '磁盘读B/s', '磁盘写B/s', '负载1'],
  ]
  for (const [hid, r] of Object.entries(histData.value)) {
    for (const b of r.buckets) {
      rows.push([hostLabelOf(hid), b.ts, b.cpu_pct, b.mem_pct, b.net_rx, b.net_tx, b.disk_r, b.disk_w, b.load1])
    }
  }
  downloadCsv(`SSHBox-组历史-${groupFilter.value}-${histKey.value}-${csvDate()}.csv`, rows)
}

let timer: ReturnType<typeof setInterval> | null = null
onMounted(async () => {
  await loadHistory()
  // Offline numbers drift, so refresh them while the panel is open.
  timer = setInterval(() => void loadHistory(), 30000)
})
onBeforeUnmount(() => {
  if (timer) clearInterval(timer)
})
</script>

<template>
  <div class="mask" @click.self="emit('close')">
    <div class="panel">
      <div class="head">
        <span class="title">总览</span>
        <span class="sub">已连接会话实时数据 · 无额外连接</span>
        <button class="close" @click="emit('close')">×</button>
      </div>

      <div class="bar">
              <select v-model="groupFilter" class="sel group-sel" title="按分组查看（集群测试视角）">
                <option value="">全部主机</option>
                <option v-for="g in props.hosts.groups.filter(Boolean)" :key="g" :value="g">{{ g }}</option>
                <option value="__none">未分组</option>
              </select>
              <span class="stat">
                <template v-if="summary.group">组内在线 <b>{{ summary.online }}</b> / {{ summary.hosts }} 台</template>
                <template v-else>在线 <b>{{ summary.online }}</b> / {{ summary.hosts }} 个会话</template>
              </span>
              <template v-if="groupLeaders">
                <span class="stat dim">最忙 {{ groupLeaders.busy.title }}（{{ fmtPct(rowCpu(groupLeaders.busy)) }}）</span>
                <span class="stat dim">最闲 {{ groupLeaders.idle.title }}（{{ fmtPct(rowCpu(groupLeaders.idle)) }}）</span>
              </template>
              <span v-if="summary.connecting" class="stat warn">连接中 {{ summary.connecting }}</span>
              <span v-if="summary.stale" class="stat warn">数据陈旧 {{ summary.stale }}</span>
              <span v-if="summary.alarmed" class="stat hot">告警 {{ summary.alarmed }}</span>
              <span class="stat dim">离线 {{ summary.offline }}</span>
              <label class="chk">
                <input v-model="includeOffline" type="checkbox" />
                <span>包含离线（历史最后状态）</span>
              </label>
              <select v-model="sortBy" class="sel">
                              <option value="name">按名称</option>
                              <option value="cpu">按 CPU</option>
                              <option value="mem">按内存</option>
                              <option value="disk">按磁盘</option>
                              <option value="load">按负载</option>
                              <option value="status">按状态</option>
                            </select>
                            <span class="view-seg">
                              <button :class="{ on: viewMode === 'table' }" @click="viewMode = 'table'">表格</button>
                              <button :class="{ on: viewMode === 'cards' }" @click="viewMode = 'cards'">卡片</button>
                            </span>
              <button class="btn ghost" @click="loadHistory">⟳ 刷新离线数据</button>
                      <button class="btn ghost" @click="injectOpen = !injectOpen" title="生成 tc netem 注入命令，填入各组终端（只填不执行）">故障注入</button>
                      <button class="btn ghost" @click="exportSnapshotCsv" title="导出当前视图为 CSV（含分组）">导出 CSV</button>
                      <button
                        v-if="Object.keys(histData).length"
                        class="btn ghost"
                        @click="exportHistoryCsv"
                        title="导出组内历史序列为 CSV（当前时间窗口）"
                      >
                        组历史 CSV
                                              </button>
                                            </div>
                              <div v-if="exportMsg" class="export-msg" :class="{ err: exportMsg.startsWith('导出失败') }">{{ exportMsg }}</div>

                              <div v-if="historyError" class="err">历史库读取失败：{{ historyError }}</div>

      <div v-if="!rows.length" class="empty">
        当前没有已连接的会话。双击左侧主机连接，或勾选「包含离线」看历史最后状态。
      </div>

      <!-- 卡片视图（#3）：每台一块卡片，CPU/内存/磁盘条 + 网络 + 状态灯 -->
      <div v-if="viewMode === 'cards' && rows.length" class="grid ov-cards">
        <div v-for="r in filteredRows" :key="r.key" class="ov-card" :class="[r.status, r.freshness]">
          <div class="card-top">
            <span class="dot" :class="[r.status, r.freshness]"></span>
            <span class="card-name">{{ r.title }}</span>
            <span v-if="r.alert" class="badge hot" :title="r.alert.body">⚠ {{ r.alert.title }}</span>
            <span class="spacer"></span>
            <span v-if="r.ageSec != null" class="dim" :class="{ warn: r.status === 'offline' || r.freshness !== 'fresh' }">{{ fmtAge(r.ageSec) }}</span>
            <span v-else class="dim">等待采样</span>
          </div>
          <div class="sub2 dim">{{ r.subtitle }}<span v-if="r.status === 'offline'" class="dim"> · 历史最后状态</span></div>
          <div class="m">
            <span class="k">CPU</span>
            <div class="track"><div class="fill" :class="barClass(r.metrics?.cpu_pct ?? r.lastCpu)" :style="{ width: Math.min(100, r.metrics?.cpu_pct ?? r.lastCpu ?? 0) + '%' }"></div></div>
            <span class="v">{{ fmtPct(r.metrics?.cpu_pct ?? r.lastCpu) }}</span>
          </div>
          <div class="m">
            <span class="k">内存</span>
            <div class="track"><div class="fill" :class="barClass(r.metrics?.mem_pct ?? r.lastMem)" :style="{ width: Math.min(100, r.metrics?.mem_pct ?? r.lastMem ?? 0) + '%' }"></div></div>
            <span class="v">{{ fmtPct(r.metrics?.mem_pct ?? r.lastMem) }}</span>
          </div>
          <div class="m">
            <span class="k">磁盘</span>
            <div class="track"><div class="fill" :class="barClass(worstDisk(r.metrics))" :style="{ width: Math.min(100, worstDisk(r.metrics) ?? 0) + '%' }"></div></div>
            <span class="v">{{ fmtPct(worstDisk(r.metrics)) }}</span>
          </div>
          <div class="card-net dim">
            <span v-if="r.metrics">↓ {{ fmtBytes(netTotals(r.metrics).rx) }} ↑ {{ fmtBytes(netTotals(r.metrics).tx) }} · 负载 {{ r.metrics.load?.[0]?.toFixed(2) ?? '—' }} · 进程 {{ r.metrics.proc_total }}</span>
            <span v-else>—</span>
          </div>
          <div class="acts">
            <button v-if="r.sid" class="btn" @click="emit('focus', r.sid!)">切到</button>
            <button v-else-if="r.hostId" class="btn" @click="emit('connect', r.hostId!)">连接</button>
          </div>
        </div>
      </div>

      <div v-if="viewMode === 'table'" class="rows">
        <div v-for="r in filteredRows" :key="r.key" class="row" :class="[r.status, r.freshness]">
          <div class="left">
            <span class="dot" :class="[r.status, r.freshness]"></span>
            <div class="names">
              <div class="name">
                {{ r.title }}
                <span v-if="r.alert" class="badge hot" :title="r.alert.body">⚠ {{ r.alert.title }}</span>
              </div>
              <div class="sub2">
                {{ r.subtitle }}
                <!-- Age first, for live *and* offline rows: "最后状态" without a
                     timestamp invites reading stale numbers as current. -->
                <span
                  v-if="r.ageSec != null"
                  class="dim"
                  :class="{ warn: r.status === 'offline' || r.freshness !== 'fresh' }"
                >
                  · {{ fmtAge(r.ageSec) }}
                </span>
                <span v-else class="dim">· 等待首个采样</span>
                <span v-if="r.status === 'offline'" class="dim">· 历史最后状态</span>
              </div>
            </div>
          </div>

          <div class="metrics">
            <div class="m">
              <span class="k">CPU</span>
              <div class="track"><div class="fill" :class="barClass(r.metrics?.cpu_pct ?? r.lastCpu)" :style="{ width: Math.min(100, r.metrics?.cpu_pct ?? r.lastCpu ?? 0) + '%' }"></div></div>
              <span class="v">{{ fmtPct(r.metrics?.cpu_pct ?? r.lastCpu) }}</span>
            </div>
            <div class="m">
              <span class="k">内存</span>
              <div class="track"><div class="fill" :class="barClass(r.metrics?.mem_pct ?? r.lastMem)" :style="{ width: Math.min(100, r.metrics?.mem_pct ?? r.lastMem ?? 0) + '%' }"></div></div>
              <span class="v">{{ fmtPct(r.metrics?.mem_pct ?? r.lastMem) }}</span>
            </div>
            <div class="m">
              <span class="k">磁盘</span>
              <div class="track"><div class="fill" :class="barClass(worstDisk(r.metrics))" :style="{ width: Math.min(100, worstDisk(r.metrics) ?? 0) + '%' }"></div></div>
              <span class="v">{{ fmtPct(worstDisk(r.metrics)) }}</span>
            </div>
          </div>

          <div class="extra">
            <span v-if="r.metrics" class="dim">
              ↓ {{ fmtBytes(netTotals(r.metrics).rx) }} ↑ {{ fmtBytes(netTotals(r.metrics).tx) }}
            </span>
            <span v-if="r.metrics" class="dim">
              负载 {{ r.metrics.load?.[0]?.toFixed(2) ?? '—' }} · 进程 {{ r.metrics.proc_total }}
            </span>
            <span v-else class="dim">—</span>
          </div>

          <div class="acts">
                      <button v-if="r.sid" class="btn" @click="emit('focus', r.sid!)">切到</button>
                      <button v-else-if="r.hostId" class="btn" @click="emit('connect', r.hostId!)">连接</button>
                    </div>
                  </div>
                </div>

                <!-- 组内历史对比：选组时出现。小多图矩阵，每台一格，谁掉队一眼可见 -->
                <div v-if="groupFilter && groupFilter !== '__none'" class="hist">
                  <div class="hist-head">
                    <span class="hist-title">
                      组内历史对比
                      <span class="dim">· {{ hostIdsInGroup.length }} 台 · {{ HIST_WINDOWS[histKey].label }}</span>
                    </span>
                    <select v-model="histKey" class="sel">
                                          <option value="1h">1 小时</option>
                                          <option value="24h">24 小时</option>
                                          <option value="7d">7 天</option>
                                        </select>
                                        <select v-model="matrixMetric" class="sel" title="矩阵对比的指标">
                                          <option value="cpu">CPU / 内存</option>
                                          <option value="net">网络</option>
                                          <option value="disk">磁盘</option>
                                          <option value="load">负载</option>
                                          <option value="ping">时延 / 丢包</option>
                                        </select>
                                        <button class="btn ghost" @click="loadGroupHistory">⟳</button>
                  </div>
                  <div v-if="histBusy" class="dim">读取历史中…</div>
                  <div v-if="histError" class="err">历史读取失败：{{ histError }}</div>
                  <div v-if="!histBusy && histEmpty" class="dim">
                                      {{ matrixMetric === 'ping'
                                        ? '组内还没有时延数据 —— 连接主机后每 15 秒自动记录一次（需测到默认网关）'
                                        : '组内还没有历史数据 —— 连接主机并开启历史后，这里会显示每台主机的曲线。' }}
                                    </div>
                                    <div class="grid">
                                      <div v-for="(r, hid) in histData" :key="hid" class="cell">
                                        <div class="cell-name">{{ hostLabelOf(hid) }}</div>
                                        <svg viewBox="0 0 220 58" preserveAspectRatio="none" width="100%" height="58">
                                          <template v-if="matrixMetric === 'cpu'">
                                            <path :d="sparkPath(cpuSeries(r), 220, 58)" fill="none" stroke="var(--ctp-blue)" stroke-width="1.3" />
                                            <path :d="sparkPath(memSeries(r), 220, 58)" fill="none" stroke="var(--ctp-green)" stroke-width="1.3" stroke-dasharray="3 2" />
                                          </template>
                                          <template v-else-if="matrixMetric === 'net'">
                                            <path :d="sparkPath(netSeries(r), 220, 58)" fill="none" stroke="var(--ctp-yellow)" stroke-width="1.3" />
                                            <path :d="sparkPath(netTxSeries(r), 220, 58)" fill="none" stroke="var(--ctp-peach)" stroke-width="1.3" stroke-dasharray="3 2" />
                                          </template>
                                          <template v-else-if="matrixMetric === 'disk'">
                                            <path :d="sparkPath(diskRSeries(r), 220, 58)" fill="none" stroke="var(--ctp-mauve)" stroke-width="1.3" />
                                            <path :d="sparkPath(diskWSeries(r), 220, 58)" fill="none" stroke="var(--ctp-maroon)" stroke-width="1.3" stroke-dasharray="3 2" />
                                          </template>
                                          <template v-else-if="matrixMetric === 'load'">
                                            <path :d="sparkPath(loadSeries(r), 220, 58)" fill="none" stroke="var(--ctp-teal)" stroke-width="1.3" />
                                          </template>
                                          <template v-else>
                                            <path :d="sparkPath(latSeries(pingData[hid]), 220, 58)" fill="none" stroke="var(--ctp-sky)" stroke-width="1.3" />
                                            <path :d="sparkPath(lossSeries(pingData[hid]), 220, 58)" fill="none" stroke="var(--ctp-red)" stroke-width="1.3" stroke-dasharray="3 2" />
                                          </template>
                                        </svg>
                                        <div class="cell-legend">
                                          <template v-if="matrixMetric === 'cpu'"><span class="lg">─ CPU</span><span class="lg dim">┄ 内存</span></template>
                                          <template v-else-if="matrixMetric === 'net'"><span class="lg">─ 收</span><span class="lg dim">┄ 发</span></template>
                                          <template v-else-if="matrixMetric === 'disk'"><span class="lg">─ 读</span><span class="lg dim">┄ 写</span></template>
                                          <template v-else-if="matrixMetric === 'load'"><span class="lg">─ 负载1</span></template>
                                          <template v-else><span class="lg">─ 时延ms</span><span class="lg dim">┄ 丢包%</span></template>
                                        </div>
                                      </div>
                                    </div>
                </div>

                          <!-- 故障注入（#12）：生成 tc netem 命令，只填入各自终端的命令行，绝不自动执行 -->
                          <div v-if="injectOpen" class="inject">
                            <div class="inject-head">
                              <span class="hist-title">故障注入 <span class="dim">· 只生成命令填入终端，回车由你按；需要目标机 root</span></span>
                            </div>
                            <div class="inject-row">
                              <span class="inj-label">目标</span>
                              <label class="check"><input type="radio" value="all" v-model="injectTarget" /> 全部已连接（{{ connectedCount }}）</label>
                              <label class="check"><input type="radio" value="group" v-model="injectTarget" /> 组</label>
                              <select v-if="injectTarget === 'group'" v-model="injectGroup" class="sel" :disabled="!groupOptions.length">
                                <option v-for="g in groupOptions" :key="g" :value="g">{{ g }}</option>
                              </select>
                              <label class="check"><input type="radio" value="host" v-model="injectTarget" /> 单台</label>
                              <select v-if="injectTarget === 'host'" v-model="injectHost" class="sel" :disabled="!connectedTabs.length">
                                <option v-for="t in connectedTabs" :key="t.sid" :value="t.sid">{{ t.label }}</option>
                              </select>
                            </div>
                            <div class="inject-row">
                              <span class="inj-label">类型</span>
                              <label class="check"><input type="radio" value="delay" v-model="injectType" /> 时延</label>
                              <input v-if="injectType === 'delay'" v-model.number="injectMs" type="number" min="1" class="num" /> ms
                              <label class="check"><input type="radio" value="loss" v-model="injectType" /> 丢包</label>
                              <input v-if="injectType === 'loss'" v-model.number="injectLoss" type="number" min="1" max="100" class="num" /> %
                              <label class="check"><input type="radio" value="blip" v-model="injectType" /> 闪断</label>
                              <template v-if="injectType === 'blip'">
                                每 <input v-model.number="injectBlipEvery" type="number" min="1" class="num" /> 秒断
                                <input v-model.number="injectBlipDur" type="number" min="1" class="num" /> 秒，共
                                <input v-model.number="injectBlipTimes" type="number" min="1" class="num" /> 次
                              </template>
                              <label class="check"><input type="radio" value="clear" v-model="injectType" /> 清除规则</label>
                            </div>
                            <div class="inject-row">
                              <span class="inj-label">网卡</span>
                              <input v-model="injectNic" class="num" style="width: 96px" />
                              <template v-if="injectType === 'delay' || injectType === 'loss'">
                                <label class="check"><input type="checkbox" v-model="injectRecover" /> 到期自动恢复</label>
                                <input v-if="injectRecover" v-model.number="injectRecoverSecs" type="number" min="1" class="num" /> 秒后清除
                              </template>
                            </div>
                            <div class="inject-row">
                              <button class="btn" :disabled="!injectCanRun" @click="runInject">生成并填入终端（{{ injectTargetCount }} 台）</button>
                              <span v-if="injectMsg" class="dim" :class="{ err: injectMsg.startsWith('失败') }">{{ injectMsg }}</span>
                            </div>
                          </div>
              </div>
            </div>
          </template>

              <style scoped>
.mask {
  position: fixed;
  inset: 0;
  background: var(--mask);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 50;
}
.panel {
  width: min(1180px, 95vw);
  max-height: 92vh;
  overflow-y: auto;
  background: var(--ctp-base);
  border: 1px solid var(--ctp-surface0);
  border-radius: 8px;
  padding: 14px 16px 16px;
  color: var(--ctp-text);
  font-size: 12px;
}
.head {
  display: flex;
  align-items: baseline;
  gap: 10px;
  margin-bottom: 10px;
}
.title {
  font-size: 15px;
  font-weight: 600;
}
.sub {
  color: var(--ctp-overlay0);
  font-size: 11px;
}
.close {
  margin-left: auto;
  background: none;
  border: none;
  color: var(--ctp-overlay0);
  font-size: 18px;
  cursor: pointer;
  line-height: 1;
}
.bar {
  display: flex;
  align-items: center;
  gap: 12px;
  flex-wrap: wrap;
  padding: 6px 0;
  border-top: 1px solid var(--ctp-surface0);
  border-bottom: 1px solid var(--ctp-surface0);
  margin-bottom: 8px;
}
.stat b {
  color: var(--ctp-text);
}
.stat.warn {
  color: var(--ctp-yellow);
}
.stat.hot {
  color: var(--ctp-red);
}
.stat.dim,
.dim {
  color: var(--ctp-overlay0);
}
.chk {
  display: flex;
  align-items: center;
  gap: 4px;
  color: var(--ctp-subtext0);
}
.sel {
  background: var(--ctp-mantle);
  color: var(--ctp-text);
  border: 1px solid var(--ctp-surface0);
  border-radius: 4px;
  padding: 3px 6px;
  font-size: 11px;
  max-width: 180px;
}
.group-sel { font-weight: 600; }
.hist { border-top: 1px solid var(--ctp-surface0); margin-top: 10px; padding-top: 8px; }
.hist-head { display: flex; align-items: center; gap: 10px; margin-bottom: 8px; }
.hist-title { font-weight: 600; }
.inject { border-top: 1px solid var(--ctp-surface0); margin-top: 12px; padding-top: 8px; }
.inject-head { margin-bottom: 4px; }
.inject-row { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; margin-top: 8px; }
.inject-row .num { width: 64px; }
.inj-label { color: var(--ctp-subtext0); font-size: 12px; }
.grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(240px, 1fr)); gap: 10px; }
.cell { border: 1px solid var(--ctp-surface0); border-radius: 6px; padding: 6px 8px; }
.cell-name { font-size: 11px; margin-bottom: 2px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.view-seg { display: inline-flex; gap: 2px; }
.view-seg button { background: none; border: 1px solid var(--ctp-surface0); color: var(--ctp-overlay0); border-radius: 4px; font-size: 11px; padding: 2px 8px; cursor: pointer; }
.view-seg button.on { color: var(--ctp-blue); border-color: var(--ctp-blue); background: var(--ctp-surface0); }
.ov-cards { margin-top: 10px; grid-template-columns: repeat(auto-fill, minmax(250px, 1fr)); }
.ov-card { border: 1px solid var(--ctp-surface0); border-radius: 8px; padding: 8px 10px; }
.ov-card.offline { opacity: 0.8; }
.ov-card .card-top { display: flex; align-items: center; gap: 6px; }
.ov-card .card-name { font-weight: 600; font-size: 12.5px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.ov-card .spacer { flex: 1; }
.ov-card .sub2 { margin-top: 2px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.ov-card .m { display: flex; align-items: center; gap: 6px; margin-top: 6px; }
.ov-card .m .k { width: 30px; color: var(--ctp-overlay0); font-size: 10.5px; }
.ov-card .m .track { flex: 1; height: 6px; background: var(--ctp-surface0); border-radius: 3px; overflow: hidden; }
.ov-card .m .fill { height: 100%; border-radius: 3px; }
.ov-card .m .v { width: 42px; text-align: right; font-size: 10.5px; color: var(--ctp-subtext0); }
.ov-card .card-net { margin-top: 7px; font-size: 11px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.ov-card .acts { margin-top: 8px; display: flex; gap: 6px; }
.cell-legend { display: flex; gap: 10px; font-size: 10px; margin-top: 2px; }
.lg { color: var(--ctp-blue); }
.lg.dim { color: var(--ctp-overlay0); }
.bar .btn {
  margin-left: auto;
}
.btn {
  background: var(--ctp-surface0);
  color: var(--ctp-text);
  border: 1px solid var(--ctp-surface1);
  border-radius: 4px;
  padding: 3px 9px;
  font-size: 11px;
  cursor: pointer;
}
.btn.ghost {
  background: transparent;
}
.err {
  background: var(--danger-soft);
  border: 1px solid var(--ctp-red);
  color: var(--ctp-red);
  border-radius: 4px;
  padding: 6px 8px;
  margin-bottom: 8px;
}
.empty {
  color: var(--ctp-overlay0);
  padding: 14px 0;
}
.rows {
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.row {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 7px 8px;
  border: 1px solid var(--ctp-surface0);
  border-radius: 5px;
  background: var(--ctp-mantle);
}
.row.closed,
.row.offline {
  opacity: 0.72;
}
.row.stale {
  border-color: var(--ctp-yellow);
}
.row.dead {
  border-color: var(--ctp-red);
}
.left {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 260px;
  flex: 1;
}
.dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--ctp-overlay0);
  flex: none;
}
.dot.connected {
  background: var(--ctp-green);
}
.dot.connected.stale {
  background: var(--ctp-yellow);
}
.dot.connected.dead {
  background: var(--ctp-red);
}
.dot.connecting {
  background: var(--ctp-yellow);
}
.names {
  min-width: 0;
}
.name {
  font-weight: 600;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.sub2 {
  font-size: 10px;
  color: var(--ctp-subtext0);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.sub2 .warn {
  color: var(--ctp-yellow);
}
.badge {
  font-size: 10px;
  font-weight: 400;
  padding: 0 4px;
  border-radius: 3px;
}
.badge.hot {
  color: var(--ctp-red);
  border: 1px solid var(--ctp-red);
}
.metrics {
  display: flex;
  gap: 10px;
  flex: none;
}
.m {
  display: flex;
  align-items: center;
  gap: 5px;
}
.m .k {
  font-size: 10px;
  color: var(--ctp-overlay0);
  width: 26px;
}
.track {
  width: 74px;
  height: 6px;
  background: var(--ctp-surface0);
  border-radius: 3px;
  overflow: hidden;
}
.fill {
  height: 100%;
  background: var(--ctp-blue);
}
.fill.warm {
  background: var(--ctp-yellow);
}
.fill.hot {
  background: var(--ctp-red);
}
.m .v {
  font-size: 10px;
  color: var(--ctp-text);
  width: 40px;
  text-align: right;
}
.extra {
  display: flex;
  flex-direction: column;
  gap: 2px;
  font-size: 10px;
  flex: none;
  width: 170px;
}
.acts {
  flex: none;
}
</style>
