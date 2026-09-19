<script setup lang="ts">
/**
 * History browser: pick a host + a window, see four charts built from stored
 * samples, export the raw rows as CSV.
 *
 * Charts are drawn from *bucketed averages* (the backend collapses the window
 * into ~600 points) while the CSV export carries every raw row — the chart is
 * for looking, the file is for keeping.
 */
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import * as echarts from 'echarts'
import { api, SshboxError, uiLog, type HistoryHost, type HistoryRange, type HistoryStats, type HostsFile } from '../api'

const props = defineProps<{
  hosts: HostsFile
  /** Host to preselect — usually the tab the user is looking at. */
  initialHostId?: string | null
}>()

const emit = defineEmits<{
  (e: 'close'): void
  (e: 'openPath', path: string): void
}>()

type Preset = '1h' | '6h' | '24h' | '7d' | '30d' | 'all' | 'custom'

const PRESETS: { key: Preset; label: string; secs: number }[] = [
  { key: '1h', label: '1 小时', secs: 3600 },
  { key: '6h', label: '6 小时', secs: 6 * 3600 },
  { key: '24h', label: '24 小时', secs: 24 * 3600 },
  { key: '7d', label: '7 天', secs: 7 * 86400 },
  { key: '30d', label: '30 天', secs: 30 * 86400 },
  { key: 'all', label: '全部', secs: 0 },
]

const AXIS = { fontSize: 9, color: '#6c7086' }
const LEGEND = { textStyle: { fontSize: 10 }, top: 0, itemHeight: 8, itemWidth: 12, icon: 'roundRect' }

const hostStats = ref<HistoryHost[]>([])
const dbStats = ref<HistoryStats | null>(null)
const selected = ref<string>('')
const preset = ref<Preset>('6h')
const customFrom = ref('')
const customTo = ref('')
const data = ref<HistoryRange | null>(null)
const loading = ref(false)
const error = ref('')
const exporting = ref(false)
const exportMsg = ref('')
const exportPath = ref('')

const cpuEl = ref<HTMLDivElement>()
const memEl = ref<HTMLDivElement>()
const netEl = ref<HTMLDivElement>()
const ioEl = ref<HTMLDivElement>()
const loadEl = ref<HTMLDivElement>()
let cpuChart: echarts.ECharts | null = null
let memChart: echarts.ECharts | null = null
let netChart: echarts.ECharts | null = null
let ioChart: echarts.ECharts | null = null
let loadChart: echarts.ECharts | null = null

// --- formatting ------------------------------------------------------------

function fmtBytes(b: number): string {
  if (b < 1024) return b.toFixed(0) + ' B/s'
  if (b < 1024 * 1024) return (b / 1024).toFixed(1) + ' KB/s'
  return (b / 1024 / 1024).toFixed(1) + ' MB/s'
}
function fmtRateShort(b: number): string {
  if (b <= 0) return '0'
  if (b < 1024) return b.toFixed(0) + 'B'
  if (b < 1024 * 1024) return (b / 1024).toFixed(0) + 'K'
  return (b / 1024 / 1024).toFixed(1) + 'M'
}
function fmtSize(bytes: number): string {
  if (bytes < 1024) return bytes + ' B'
  if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + ' KB'
  return (bytes / 1024 / 1024).toFixed(1) + ' MB'
}
function fmtDur(secs: number): string {
  if (!isFinite(secs) || secs <= 0) return '—'
  if (secs < 60) return secs.toFixed(0) + ' 秒'
  if (secs < 3600) return (secs / 60).toFixed(0) + ' 分钟'
  if (secs < 86400) return (secs / 3600).toFixed(1) + ' 小时'
  return (secs / 86400).toFixed(1) + ' 天'
}
function stamp(ts: number): string {
  if (!ts) return '—'
  const d = new Date(ts * 1000)
  const p = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`
}
function stampFull(ts: number): string {
  if (!ts) return '—'
  const d = new Date(ts * 1000)
  const p = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`
}
/** `datetime-local` wants local wall time without a timezone suffix. */
function toLocalInput(ts: number): string {
  const d = new Date(ts * 1000)
  const p = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}`
}
function fromLocalInput(v: string): number {
  const t = Date.parse(v)
  return isNaN(t) ? 0 : t / 1000
}

/** Human label for a host id, resolving saved hosts when possible. */
function hostLabel(id: string): string {
  const h = props.hosts.hosts.find((x) => x.id === id)
  if (h) return `${h.name} (${h.username}@${h.host})`
  if (id.startsWith('临时-')) return `临时连接 ${id.slice(3)}（未保存的主机）`
  return id
}

// --- window ----------------------------------------------------------------

/**
 * The window the user is looking at, in unix seconds.
 *
 * Deliberately a plain function, not a `computed`: `Date.now()` is not
 * reactive, so a computed would cache the first window forever and every later
 * 查询 would silently re-query a stale range.
 */
function currentWindow(): { from: number; to: number } {
  const now = Date.now() / 1000
  if (preset.value === 'custom') {
    const f = fromLocalInput(customFrom.value)
    const t = fromLocalInput(customTo.value) || now
    return f > 0 ? { from: f, to: t } : { from: now - 3600, to: now }
  }
  if (preset.value === 'all') {
    const s = data.value
    if (s && s.oldest > 0) return { from: s.oldest, to: (s.newest || now) + 1 }
    return { from: now - 3600, to: now }
  }
  const secs = PRESETS.find((p) => p.key === preset.value)?.secs ?? 3600
  return { from: now - secs, to: now }
}

/** The window actually behind the numbers on screen (≠ "now" after a while). */
const queried = ref<{ from: number; to: number } | null>(null)

const coverage = computed(() => {
  const s = data.value
  if (!s || !s.oldest) return null
  return { oldest: s.oldest, newest: s.newest, span: s.newest - s.oldest }
})

// --- loading ---------------------------------------------------------------

async function loadHosts() {
  try {
    hostStats.value = await api.historyHosts()
  } catch (e) {
    error.value = e instanceof SshboxError ? e.message : String(e)
    return
  }
  try {
    dbStats.value = await api.historyStats()
  } catch {
    dbStats.value = null
  }
  const ids = hostStats.value.map((h) => h.host_id)
  if (props.initialHostId && ids.includes(props.initialHostId)) {
    selected.value = props.initialHostId
  } else if (!ids.includes(selected.value)) {
    selected.value = ids[0] ?? ''
  }
  if (!selected.value) error.value = '还没有任何历史数据 —— 连上主机跑一会儿再来'
}

async function load() {
  if (!selected.value) return
  loading.value = true
  error.value = ''
  const { from, to } = currentWindow()
  try {
    data.value = await api.historyRange(selected.value, from, to, 600)
    queried.value = { from, to }
    // Row count / file size move as the monitor writes, so refresh them with
    // every query instead of showing whatever the panel saw at mount.
    dbStats.value = await api.historyStats().catch(() => dbStats.value)
    if (preset.value === 'custom' && !customFrom.value) {
      customFrom.value = toLocalInput(from)
      customTo.value = toLocalInput(to)
    }
    void uiLog(
      `历史查询 ${selected.value} [${stampFull(from)} ~ ${stampFull(to)}] → ${data.value.buckets.length} 点 / ${data.value.raw_points} 原始行`
    )
  } catch (e) {
    data.value = null
    error.value = e instanceof SshboxError ? e.message : String(e)
  } finally {
    loading.value = false
    await nextTick()
    draw()
  }
}

// --- charts ----------------------------------------------------------------

function initPct(el: HTMLDivElement, names: string[], colors: string[]): echarts.ECharts {
  const c = echarts.init(el, 'dark')
  c.setOption({
    backgroundColor: 'transparent',
    grid: { left: 34, right: 10, top: 24, bottom: 20 },
    legend: { ...LEGEND, data: names },
    tooltip: {
      trigger: 'axis',
      valueFormatter: (v: number) => (v == null ? '—' : v.toFixed(1) + '%'),
    },
    xAxis: { type: 'category', data: [], axisLabel: AXIS },
    yAxis: {
      type: 'value',
      min: 0,
      max: 100,
      axisLabel: AXIS,
      splitLine: { lineStyle: { color: '#313244' } },
    },
    series: names.map((n, i) => ({
      name: n,
      type: 'line',
      data: [],
      smooth: true,
      showSymbol: false,
      lineStyle: { width: 1.5, color: colors[i] },
      itemStyle: { color: colors[i] },
      areaStyle: { opacity: 0.15, color: colors[i] },
    })),
  })
  return c
}

function initRate(el: HTMLDivElement, names: string[], colors: string[]): echarts.ECharts {
  const c = echarts.init(el, 'dark')
  c.setOption({
    backgroundColor: 'transparent',
    grid: { left: 46, right: 10, top: 24, bottom: 20 },
    legend: { ...LEGEND, data: names },
    tooltip: { trigger: 'axis', valueFormatter: (v: number) => fmtBytes(v) },
    xAxis: { type: 'category', data: [], axisLabel: AXIS },
    yAxis: {
      type: 'value',
      min: 0,
      axisLabel: { ...AXIS, formatter: fmtRateShort },
      splitLine: { lineStyle: { color: '#313244' } },
    },
    series: names.map((n, i) => ({
      name: n,
      type: 'line',
      data: [],
      smooth: true,
      showSymbol: false,
      lineStyle: { width: 1.5, color: colors[i] },
      itemStyle: { color: colors[i] },
      areaStyle: { opacity: 0.12, color: colors[i] },
    })),
  })
  return c
}

/** Load average: 1-minute value, auto-scaled (no meaningful fixed max). */
function initLoad(el: HTMLDivElement): echarts.ECharts {
  const c = echarts.init(el, 'dark')
  c.setOption({
    backgroundColor: 'transparent',
    grid: { left: 34, right: 10, top: 24, bottom: 20 },
    legend: { ...LEGEND, data: ['负载 (1 分钟)'] },
    tooltip: { trigger: 'axis', valueFormatter: (v: number) => (v == null ? '—' : v.toFixed(2)) },
    xAxis: { type: 'category', data: [], axisLabel: AXIS },
    yAxis: { type: 'value', min: 0, axisLabel: AXIS, splitLine: { lineStyle: { color: '#313244' } } },
    series: [
      {
        name: '负载 (1 分钟)',
        type: 'line',
        data: [],
        smooth: true,
        showSymbol: false,
        lineStyle: { width: 1.5, color: '#f9e2af' },
        itemStyle: { color: '#f9e2af' },
        areaStyle: { opacity: 0.12, color: '#f9e2af' },
      },
    ],
  })
  return c
}

function draw() {
  const s = data.value
  if (!s) return
  if (!cpuChart && cpuEl.value) cpuChart = initPct(cpuEl.value, ['CPU %'], ['#89b4fa'])
  if (!memChart && memEl.value) memChart = initPct(memEl.value, ['内存 %'], ['#a6e3a1'])
  if (!netChart && netEl.value) netChart = initRate(netEl.value, ['↓ 下行', '↑ 上行'], ['#a6e3a1', '#89b4fa'])
  if (!ioChart && ioEl.value) ioChart = initRate(ioEl.value, ['读', '写'], ['#fab387', '#cba6f7'])
  if (!loadChart && loadEl.value) loadChart = initLoad(loadEl.value)

  // Bucket ts → local clock label. Buckets can be hours wide, so the label
  // carries the date too, otherwise a 30-day view reads "13:05" fifty times.
  const wide = s.bucket_secs >= 3600
  const labels = s.buckets.map((b) => (wide ? stamp(b.ts) : stampFull(b.ts).slice(5)))

  cpuChart?.setOption({ xAxis: { data: labels }, series: [{ data: s.buckets.map((b) => +b.cpu_pct.toFixed(2)) }] })
  memChart?.setOption({ xAxis: { data: labels }, series: [{ data: s.buckets.map((b) => +b.mem_pct.toFixed(2)) }] })
  netChart?.setOption({
    xAxis: { data: labels },
    series: [{ data: s.buckets.map((b) => Math.round(b.net_rx)) }, { data: s.buckets.map((b) => Math.round(b.net_tx)) }],
  })
  ioChart?.setOption({
    xAxis: { data: labels },
    series: [{ data: s.buckets.map((b) => Math.round(b.disk_r)) }, { data: s.buckets.map((b) => Math.round(b.disk_w)) }],
  })
  loadChart?.setOption({
    xAxis: { data: labels },
    series: [{ data: s.buckets.map((b) => +b.load1.toFixed(2)) }],
  })
  for (const c of [cpuChart, memChart, netChart, ioChart, loadChart]) c?.resize()
}

function resizeAll() {
  for (const c of [cpuChart, memChart, netChart, ioChart, loadChart]) c?.resize()
}

// --- export ----------------------------------------------------------------

async function exportCsv() {
  if (!selected.value) return
  exporting.value = true
  exportMsg.value = ''
  const { from, to } = currentWindow()
  try {
    const r = await api.historyExport(selected.value, from, to, null)
    exportPath.value = r.path
    exportMsg.value = `已导出 ${r.rows} 行（${fmtSize(r.bytes)}）`
    void uiLog(`历史导出: ${r.path} (${r.rows} 行)`)
  } catch (e) {
    exportMsg.value = e instanceof SshboxError ? e.message : String(e)
  } finally {
    exporting.value = false
  }
}

// --- lifecycle -------------------------------------------------------------

// `booted` keeps the initial load single-shot: setting `selected` inside
// loadHosts() would otherwise fire the watcher *and* the explicit load().
let booted = false
watch([selected, preset], () => {
  if (booted) void load()
})
watch([customFrom, customTo], () => {
  if (preset.value === 'custom') void load()
})

let ro: ResizeObserver | null = null
onMounted(async () => {
  const now = Date.now() / 1000
  customFrom.value = toLocalInput(now - 3600)
  customTo.value = toLocalInput(now)
  await loadHosts()
  await load()
  booted = true
  ro = new ResizeObserver(() => resizeAll())
  if (cpuEl.value?.parentElement) ro.observe(cpuEl.value.parentElement)
})
onBeforeUnmount(() => {
  ro?.disconnect()
  for (const c of [cpuChart, memChart, netChart, ioChart, loadChart]) c?.dispose()
})
</script>

<template>
  <div class="mask" @click.self="emit('close')">
    <div class="panel">
      <div class="head">
        <span class="title">历史回看</span>
        <span class="sub">按本机时间 · 落盘数据来自监控采样</span>
        <button class="close" @click="emit('close')">×</button>
      </div>

      <div class="bar">
        <select v-model="selected" class="sel">
          <option v-if="!hostStats.length" value="">（暂无数据）</option>
          <option v-for="h in hostStats" :key="h.host_id" :value="h.host_id">
            {{ hostLabel(h.host_id) }} · {{ h.rows }} 行
          </option>
        </select>

        <div class="presets">
          <button
            v-for="p in PRESETS"
            :key="p.key"
            class="pill"
            :class="{ on: preset === p.key }"
            @click="preset = p.key"
          >
            {{ p.label }}
          </button>
          <button class="pill" :class="{ on: preset === 'custom' }" @click="preset = 'custom'">自定义</button>
        </div>

        <button class="btn" :disabled="loading || !selected" @click="load()">
          {{ loading ? '查询中…' : '查询' }}
        </button>
      </div>

      <div v-if="preset === 'custom'" class="bar custom">
        <label>从 <input v-model="customFrom" type="datetime-local" class="dt" /></label>
        <label>到 <input v-model="customTo" type="datetime-local" class="dt" /></label>
      </div>

      <div class="meta">
        <template v-if="data">
          <span v-if="queried">
            窗口 <b>{{ stamp(queried.from) }}</b> → <b>{{ stampFull(queried.to) }}</b>
          </span>
          <span>原始 <b>{{ data.raw_points }}</b> 行</span>
          <span>聚合 <b>{{ data.buckets.length }}</b> 点</span>
          <span>每点 <b>{{ fmtDur(data.bucket_secs) }}</b></span>
          <span v-if="coverage">
            已存数据 <b>{{ stamp(coverage.oldest) }}</b> → <b>{{ stamp(coverage.newest) }}</b>
            （{{ fmtDur(coverage.span) }}）
          </span>
        </template>
        <span v-if="dbStats" class="db">
          库 {{ fmtSize(dbStats.bytes) }} · 共 {{ dbStats.rows }} 行 · {{ dbStats.hosts }} 台主机 · 保留
          {{ dbStats.retention_days }} 天
        </span>
      </div>

      <div v-if="error" class="err">{{ error }}</div>

      <div v-if="data && !data.buckets.length && !error" class="empty-win">
        这个时段没有数据。{{ coverage ? `已存数据从 ${stamp(coverage.oldest)} 开始。` : '' }}
      </div>

      <div class="charts">
        <div class="chart-row"><div class="chart-box" ref="cpuEl"></div></div>
        <div class="chart-row"><div class="chart-box" ref="memEl"></div></div>
        <div class="chart-row"><div class="chart-box" ref="netEl"></div></div>
        <div class="chart-row"><div class="chart-box" ref="ioEl"></div></div>
        <div class="chart-row"><div class="chart-box" ref="loadEl"></div></div>
      </div>

      <div class="foot">
        <button class="btn" :disabled="exporting || !selected" @click="exportCsv">
          {{ exporting ? '导出中…' : '导出 CSV（原始行）' }}
        </button>
        <span v-if="exportMsg" class="ok">{{ exportMsg }}</span>
        <span v-if="exportPath" class="path" :title="exportPath">{{ exportPath }}</span>
        <button v-if="exportPath" class="btn ghost" @click="emit('openPath', exportPath)">打开文件夹</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.mask {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.5);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 50;
}
.panel {
  width: min(1100px, 94vw);
  max-height: 92vh;
  overflow-y: auto;
  background: #1e1e2e;
  border: 1px solid #313244;
  border-radius: 8px;
  padding: 14px 16px 16px;
  color: #cdd6f4;
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
  color: #6c7086;
  font-size: 11px;
}
.close {
  margin-left: auto;
  background: none;
  border: none;
  color: #6c7086;
  font-size: 18px;
  cursor: pointer;
  line-height: 1;
}
.bar {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
  margin-bottom: 8px;
}
.bar.custom {
  color: #a6adc8;
}
.sel,
.dt {
  background: #181825;
  color: #cdd6f4;
  border: 1px solid #313244;
  border-radius: 4px;
  padding: 4px 6px;
  font-size: 11px;
}
.sel {
  min-width: 240px;
  max-width: 340px;
}
.presets {
  display: flex;
  gap: 4px;
}
.pill {
  background: #181825;
  color: #a6adc8;
  border: 1px solid #313244;
  border-radius: 10px;
  padding: 3px 9px;
  font-size: 11px;
  cursor: pointer;
}
.pill.on {
  background: #89b4fa;
  color: #11111b;
  border-color: #89b4fa;
}
.btn {
  background: #313244;
  color: #cdd6f4;
  border: 1px solid #45475a;
  border-radius: 4px;
  padding: 4px 10px;
  font-size: 11px;
  cursor: pointer;
}
.btn:disabled {
  opacity: 0.5;
  cursor: default;
}
.btn.ghost {
  background: transparent;
}
.meta {
  display: flex;
  gap: 14px;
  flex-wrap: wrap;
  color: #a6adc8;
  font-size: 11px;
  padding: 6px 0;
  border-top: 1px solid #313244;
  border-bottom: 1px solid #313244;
  margin-bottom: 8px;
}
.meta b {
  color: #cdd6f4;
}
.meta .db {
  margin-left: auto;
  color: #6c7086;
}
.err {
  background: rgba(243, 139, 168, 0.12);
  border: 1px solid #f38ba8;
  color: #f38ba8;
  border-radius: 4px;
  padding: 6px 8px;
  margin-bottom: 8px;
}
.empty-win {
  color: #6c7086;
  padding: 8px 0;
}
.charts {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.chart-box {
  height: 132px;
  width: 100%;
}
.foot {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 10px;
  flex-wrap: wrap;
}
.ok {
  color: #a6e3a1;
}
.path {
  color: #6c7086;
  font-size: 10px;
  max-width: 420px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
