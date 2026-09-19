<script setup lang="ts">
/**
 * Fleet overview: every connected session on one screen, plus (opt-in) the
 * last known state of hosts that are not connected right now.
 *
 * Live rows come from the global `fleet` layer — no extra SSH connections.
 * Offline rows come from the history database, and they say how old they are:
 * a monitoring view that hides the age of its numbers is worse than no view.
 */
import { computed, onMounted, onBeforeUnmount, ref } from 'vue'
import { api, type Alert, type HistoryHost, type HostsFile, type Metrics } from '../api'
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
}>()

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
const sortBy = ref<'name' | 'cpu' | 'mem' | 'status'>('name')
const historyHosts = ref<HistoryHost[]>([])
const historyError = ref('')

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
function hostLabelOf(id: string | null | undefined): string {
  if (!id) return '未保存的主机'
  const h = props.hosts.hosts.find((x) => x.id === id)
  return h ? h.name || h.host : id.startsWith('临时-') ? '临时连接' : id
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
      case 'status':
        return rank[a.status] - rank[b.status] || a.title.localeCompare(b.title)
      default:
        return a.title.localeCompare(b.title)
    }
  })
})

const summary = computed(() => {
  const online = props.tabs.filter((t) => t.status === 'connected').length
  const connecting = props.tabs.filter((t) => t.status === 'connecting').length
  const stale = rows.value.filter((r) => r.status === 'connected' && r.freshness !== 'fresh').length
  const alarmed = rows.value.filter((r) => r.alert).length
  const offline = includeOffline.value
    ? rows.value.filter((r) => r.status === 'offline').length
    : historyHosts.value.filter((h) => !props.tabs.some((t) => t.hostId === h.host_id)).length
  return { online, connecting, stale, alarmed, offline, hosts: props.tabs.length }
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
        <span class="stat">在线 <b>{{ summary.online }}</b> / {{ summary.hosts }} 个会话</span>
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
          <option value="status">按状态</option>
        </select>
        <button class="btn ghost" @click="loadHistory">⟳ 刷新离线数据</button>
      </div>

      <div v-if="historyError" class="err">历史库读取失败：{{ historyError }}</div>

      <div v-if="!rows.length" class="empty">
        当前没有已连接的会话。双击左侧主机连接，或勾选「包含离线」看历史最后状态。
      </div>

      <div class="rows">
        <div v-for="r in rows" :key="r.key" class="row" :class="[r.status, r.freshness]">
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
}
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
