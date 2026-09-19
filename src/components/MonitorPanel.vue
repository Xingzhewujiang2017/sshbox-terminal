<script setup lang="ts">
import { onMounted, onBeforeUnmount, ref, computed, nextTick } from 'vue'
import { listen } from '@tauri-apps/api/event'
import * as echarts from 'echarts'
import { api } from '../api'

interface StaticInfo {
  hostname: string
  os_pretty: string
  kernel: string
  arch: string
  cpu_model: string
  cpu_cores: number
  mem_total_kb: number
  uptime_secs: number
  disks: { mount: string; total_kb: number; used_kb: number; use_pct: number }[]
}
interface NetIf { name: string; rx_bps: number; tx_bps: number }
interface DiskIo { name: string; read_bps: number; write_bps: number }
interface ProcInfo { pid: number; name: string; state: string; cpu_pct: number; rss_kb: number }
interface Metrics {
  ts: number
  cpu_pct: number
  cpu_per_core: number[]
  mem_total_kb: number
  mem_used_kb: number
  mem_pct: number
  swap_total_kb: number
  swap_used_kb: number
  net: NetIf[]
  disk_io: DiskIo[]
  disks: StaticInfo['disks']
  load: number[]
  processes: ProcInfo[]
  proc_total: number
}

const props = defineProps<{ sid: string; active: boolean; interval: number }>()
const emit = defineEmits<{ (e: 'setInterval', secs: number): void }>()

const info = ref<StaticInfo | null>(null)
const metrics = ref<Metrics | null>(null)
const paused = ref(false)
const INTERVALS = [1, 2, 5, 10]

/** The chart window depends on the interval: 150 points is not always 5 min. */
const windowLabel = computed(() => {
  const secs = MAX_POINTS * (props.interval || 2)
  return secs >= 60 ? `近 ${Math.round(secs / 60)} 分钟` : `近 ${secs} 秒`
})

async function togglePause() {
  paused.value = !paused.value
  try {
    await api.monitorSetPaused(props.sid, paused.value)
  } catch {
    paused.value = !paused.value
  }
}

async function sampleNow() {
  try {
    await api.monitorSampleNow(props.sid)
  } catch {
    /* session already gone */
  }
}

// Rolling history: 5 min @ 2s = 150 points
const MAX_POINTS = 150
const history = ref<{
  t: string[]; cpu: number[]; mem: number[]
  rx: number[]; tx: number[]; dread: number[]; dwrite: number[]
}>({ t: [], cpu: [], mem: [], rx: [], tx: [], dread: [], dwrite: [] })

let unlistenStatic: (() => void) | null = null
let unlistenMetrics: (() => void) | null = null

const rootEl = ref<HTMLDivElement>()
const cpuEl = ref<HTMLDivElement>()
const netEl = ref<HTMLDivElement>()
const ioEl = ref<HTMLDivElement>()
let cpuChart: echarts.ECharts | null = null
let netChart: echarts.ECharts | null = null
let ioChart: echarts.ECharts | null = null
let ro: ResizeObserver | null = null

const AXIS = { fontSize: 9, color: '#6c7086' }
const LEGEND = { textStyle: { fontSize: 10 }, top: 0, itemHeight: 8, itemWidth: 12, icon: 'roundRect' }

function fmtBytes(b: number): string {
  if (b < 1024) return b.toFixed(0) + ' B/s'
  if (b < 1024 * 1024) return (b / 1024).toFixed(1) + ' KB/s'
  return (b / 1024 / 1024).toFixed(1) + ' MB/s'
}
function fmtKB(kb: number): string {
  if (kb < 1024 * 1024) return (kb / 1024).toFixed(1) + ' MB'
  return (kb / 1024 / 1024).toFixed(1) + ' GB'
}
function fmtUptime(s: number): string {
  const d = Math.floor(s / 86400)
  const h = Math.floor((s % 86400) / 3600)
  const m = Math.floor((s % 3600) / 60)
  return d > 0 ? `${d}天 ${h}小时 ${m}分` : h > 0 ? `${h}小时 ${m}分` : `${m}分钟`
}
/** Compact axis label for a rate (bytes/s). */
function fmtRateShort(b: number): string {
  if (b <= 0) return '0'
  if (b < 1024) return b.toFixed(0) + 'B'
  if (b < 1024 * 1024) return (b / 1024).toFixed(0) + 'K'
  return (b / 1024 / 1024).toFixed(1) + 'M'
}

const netTotals = computed(() => {
  if (!metrics.value) return { rx: 0, tx: 0 }
  return metrics.value.net.reduce(
    (a, n) => ({ rx: a.rx + n.rx_bps, tx: a.tx + n.tx_bps }),
    { rx: 0, tx: 0 }
  )
})

/** Percent chart (fixed 0-100 axis). */
function initPct(el: HTMLDivElement): echarts.ECharts {
  const c = echarts.init(el, 'dark')
  c.setOption({
    backgroundColor: 'transparent',
    grid: { left: 34, right: 10, top: 24, bottom: 20 },
    legend: { ...LEGEND, data: ['CPU %', '内存 %'] },
    tooltip: { trigger: 'axis', valueFormatter: (v: number) => v.toFixed(1) + '%' },
    xAxis: { type: 'category', data: [], axisLabel: AXIS },
    yAxis: { type: 'value', min: 0, max: 100, axisLabel: AXIS, splitLine: { lineStyle: { color: '#313244' } } },
    series: [
      { name: 'CPU %', type: 'line', data: [], smooth: true, showSymbol: false, lineStyle: { width: 1.5 }, areaStyle: { opacity: 0.15 } },
      { name: '内存 %', type: 'line', data: [], smooth: true, showSymbol: false, lineStyle: { width: 1.5 }, areaStyle: { opacity: 0.15 } },
    ],
  })
  return c
}

/** Rate chart (bytes/s, auto-scaled axis with human labels). */
function initRate(el: HTMLDivElement, names: string[], colors: string[]): echarts.ECharts {
  const c = echarts.init(el, 'dark')
  c.setOption({
    backgroundColor: 'transparent',
    grid: { left: 46, right: 10, top: 24, bottom: 20 },
    legend: { ...LEGEND, data: names },
    tooltip: { trigger: 'axis', valueFormatter: (v: number) => fmtBytes(v) },
    xAxis: { type: 'category', data: [], axisLabel: AXIS },
    yAxis: { type: 'value', min: 0, axisLabel: { ...AXIS, formatter: fmtRateShort }, splitLine: { lineStyle: { color: '#313244' } } },
    series: names.map((n, i) => ({
      name: n, type: 'line', data: [], smooth: true, showSymbol: false,
      lineStyle: { width: 1.5, color: colors[i] },
      itemStyle: { color: colors[i] },
      areaStyle: { opacity: 0.12, color: colors[i] },
    })),
  })
  return c
}

/**
 * Charts live inside `v-if="metrics"`, so the container elements do not exist
 * until the first sample arrives — init must happen after that render, never
 * in onMounted.
 */
function ensureCharts() {
  if (!cpuChart && cpuEl.value) cpuChart = initPct(cpuEl.value)
  if (!netChart && netEl.value) netChart = initRate(netEl.value, ['↓ 下行', '↑ 上行'], ['#a6e3a1', '#89b4fa'])
  if (!ioChart && ioEl.value) ioChart = initRate(ioEl.value, ['读', '写'], ['#fab387', '#cba6f7'])
}

function updateCharts() {
  const h = history.value
  cpuChart?.setOption({ xAxis: { data: h.t }, series: [{ data: h.cpu }, { data: h.mem }] })
  netChart?.setOption({ xAxis: { data: h.t }, series: [{ data: h.rx }, { data: h.tx }] })
  ioChart?.setOption({ xAxis: { data: h.t }, series: [{ data: h.dread }, { data: h.dwrite }] })
}

async function refresh() {
  await nextTick()
  ensureCharts()
  updateCharts()
}

onMounted(async () => {
  try {
    paused.value = await api.monitorPaused(props.sid)
  } catch {
    /* new session: never paused */
  }
  unlistenStatic = await listen<{ sid: string; info: StaticInfo }>('ssh://static', (e) => {
    if (e.payload.sid === props.sid) info.value = e.payload.info
  })
  unlistenMetrics = await listen<{ sid: string; metrics: Metrics }>('ssh://metrics', (e) => {
    if (e.payload.sid !== props.sid) return
    const m = e.payload.metrics
    metrics.value = m
    const h = history.value
    const now = new Date(m.ts * 1000)
    h.t.push(now.toTimeString().slice(0, 8))
    h.cpu.push(+m.cpu_pct.toFixed(1))
    h.mem.push(+m.mem_pct.toFixed(1))
    const nt = m.net.reduce((a, n) => ({ rx: a.rx + n.rx_bps, tx: a.tx + n.tx_bps }), { rx: 0, tx: 0 })
    h.rx.push(nt.rx)
    h.tx.push(nt.tx)
    const io = m.disk_io.reduce((a, d) => ({ r: a.r + d.read_bps, w: a.w + d.write_bps }), { r: 0, w: 0 })
    h.dread.push(io.r)
    h.dwrite.push(io.w)
    while (h.t.length > MAX_POINTS) {
      h.t.shift(); h.cpu.shift(); h.mem.shift()
      h.rx.shift(); h.tx.shift(); h.dread.shift(); h.dwrite.shift()
    }
    void refresh()
  })

  ro = new ResizeObserver(() => {
    cpuChart?.resize()
    netChart?.resize()
    ioChart?.resize()
  })
  if (rootEl.value) ro.observe(rootEl.value)
})

onBeforeUnmount(() => {
  unlistenStatic?.()
  unlistenMetrics?.()
  ro?.disconnect()
  cpuChart?.dispose()
  netChart?.dispose()
  ioChart?.dispose()
})
</script>

<template>
  <div class="monitor" ref="rootEl">
    <div class="bar">
      <span class="bar-label">采样</span>
      <button
        v-for="s in INTERVALS"
        :key="s"
        class="chip"
        :class="{ on: props.interval === s }"
        @click="emit('setInterval', s)"
      >{{ s }}s</button>
      <span class="bar-spacer"></span>
      <button class="chip" :class="{ warn: paused }" :title="paused ? '继续采集' : '暂停采集'" @click="togglePause">
        {{ paused ? '▶ 继续' : '❙❙ 暂停' }}
      </button>
      <button class="chip" title="立即采集一次" @click="sampleNow">⟳</button>
    </div>
    <div v-if="paused" class="paused-banner">监控已暂停 · 数据不再更新，点「继续」恢复</div>

    <template v-if="info">
      <div class="static-box">
        <div class="host">{{ info.hostname }}</div>
        <div class="row"><span>系统</span>{{ info.os_pretty }}</div>
        <div class="row"><span>内核</span>{{ info.kernel }} ({{ info.arch }})</div>
        <div class="row"><span>CPU</span>{{ info.cpu_model }} × {{ info.cpu_cores }}</div>
        <div class="row"><span>内存</span>{{ fmtKB(info.mem_total_kb) }}</div>
        <div class="row"><span>运行</span>{{ fmtUptime(info.uptime_secs) }}</div>
      </div>
    </template>
    <div v-else class="placeholder">采集系统信息中…</div>

    <template v-if="metrics">
      <div class="gauges">
        <div class="gauge">
          <div class="ring" :style="{ '--pct': metrics.cpu_pct + '%', '--color': metrics.cpu_pct > 85 ? '#f38ba8' : '#89b4fa' }">
            <div class="ring-val">{{ metrics.cpu_pct.toFixed(0) }}%</div>
          </div>
          <div class="gauge-label">CPU</div>
        </div>
        <div class="gauge">
          <div class="ring" :style="{ '--pct': metrics.mem_pct + '%', '--color': metrics.mem_pct > 85 ? '#f38ba8' : '#a6e3a1' }">
            <div class="ring-val">{{ metrics.mem_pct.toFixed(0) }}%</div>
          </div>
          <div class="gauge-label">{{ fmtKB(metrics.mem_used_kb) }} / {{ fmtKB(metrics.mem_total_kb) }}</div>
        </div>
      </div>

      <div class="card">
        <div class="section-title">CPU / 内存 <span class="unit">{{ windowLabel }}</span></div>
        <div class="chart-box" ref="cpuEl"></div>
      </div>

      <div class="net-box">
        <div class="section-title">网络 <span class="unit">{{ windowLabel }}</span></div>
        <div class="net-total">
          <span class="down">↓ {{ fmtBytes(netTotals.rx) }}</span>
          <span class="up">↑ {{ fmtBytes(netTotals.tx) }}</span>
        </div>
        <div class="chart-box" ref="netEl"></div>
        <div v-for="n in metrics.net" :key="n.name" class="net-if">
          <span class="ifname">{{ n.name }}</span>
          <span>↓{{ fmtBytes(n.rx_bps) }}</span>
          <span>↑{{ fmtBytes(n.tx_bps) }}</span>
        </div>
      </div>

      <div class="disk-box">
        <div class="section-title">磁盘</div>
        <div v-for="d in metrics.disks" :key="d.mount" class="disk-row">
          <div class="disk-head">
            <span>{{ d.mount }}</span>
            <span>{{ fmtKB(d.used_kb) }} / {{ fmtKB(d.total_kb) }}</span>
          </div>
          <div class="bar"><div class="bar-fill" :class="{ warn: d.use_pct > 85 }" :style="{ width: d.use_pct + '%' }"></div></div>
        </div>
        <template v-if="metrics.disk_io.length">
          <div class="section-title">磁盘 IO <span class="unit">{{ windowLabel }}</span></div>
          <div class="chart-box" ref="ioEl"></div>
          <div v-for="io in metrics.disk_io" :key="io.name" class="io-row">
            <span class="ifname">{{ io.name }}</span>
            <span>R {{ fmtBytes(io.read_bps) }}</span>
            <span>W {{ fmtBytes(io.write_bps) }}</span>
          </div>
        </template>
      </div>

      <div class="proc-box">
        <div class="section-title">
          进程<span class="proc-count">共 {{ metrics.proc_total }} 个 · 按瞬时 CPU 排序</span>
        </div>
        <div class="proc-head"><span>PID</span><span>名称</span><span>CPU</span><span>内存</span></div>
        <div v-for="p in metrics.processes" :key="p.pid" class="proc-row">
          <span class="pid">{{ p.pid }}</span>
          <span class="pname" :title="p.name">{{ p.name }}</span>
          <span class="pcpu" :class="{ hot: p.cpu_pct > 50 }">{{ p.cpu_pct.toFixed(1) }}%</span>
          <span class="pmem">{{ fmtKB(p.rss_kb) }}</span>
        </div>
      </div>

      <div v-if="metrics.load.length" class="load-box">
        负载: {{ metrics.load.map(l => l.toFixed(2)).join(' / ') }}
        <span v-if="metrics.swap_total_kb > 0"> · Swap: {{ fmtKB(metrics.swap_used_kb) }}/{{ fmtKB(metrics.swap_total_kb) }}</span>
      </div>
    </template>
    <div v-else class="placeholder">等待监控数据…</div>
  </div>
</template>

<style scoped>
.monitor {
  height: 100%;
  overflow-y: auto;
  background: #181825;
  color: #cdd6f4;
  font-size: 12px;
  padding: 10px;
  box-sizing: border-box;
}
.bar { display: flex; align-items: center; gap: 4px; margin-bottom: 8px; flex-wrap: wrap; }
.bar-label { color: #6c7086; font-size: 11px; margin-right: 2px; }
.bar-spacer { flex: 1; }
.chip {
  background: #1e1e2e; color: #a6adc8; border: 1px solid #313244; border-radius: 5px;
  font-size: 11px; padding: 2px 7px; cursor: pointer; font-family: inherit;
}
.chip:hover { border-color: #585b70; color: #cdd6f4; }
.chip.on { background: #89b4fa; border-color: #89b4fa; color: #11111b; font-weight: 600; }
.chip.warn { background: #f9e2af; border-color: #f9e2af; color: #11111b; font-weight: 600; }
.paused-banner {
  background: #313244; color: #f9e2af; border-radius: 6px; padding: 5px 8px;
  margin-bottom: 8px; font-size: 11px; text-align: center;
}
.placeholder { color: #6c7086; padding: 20px 0; text-align: center; }
.static-box { background: #1e1e2e; border-radius: 8px; padding: 10px; margin-bottom: 10px; }
.host { font-size: 15px; font-weight: 600; margin-bottom: 6px; color: #89b4fa; }
.row { display: flex; gap: 6px; padding: 2px 0; }
.row span { color: #6c7086; min-width: 36px; }
.gauges { display: flex; gap: 10px; justify-content: space-around; margin-bottom: 10px; }
.gauge { text-align: center; }
.ring {
  width: 72px; height: 72px; border-radius: 50%;
  background: conic-gradient(var(--color) var(--pct), #313244 0);
  display: flex; align-items: center; justify-content: center;
  margin: 0 auto;
}
.ring::before { content: ''; position: absolute; }
.ring-val {
  width: 56px; height: 56px; border-radius: 50%; background: #181825;
  display: flex; align-items: center; justify-content: center;
  font-size: 14px; font-weight: 600;
}
.gauge-label { margin-top: 4px; color: #a6adc8; font-size: 11px; }
.section-title { font-weight: 600; color: #a6adc8; margin: 8px 0 4px; font-size: 11px; text-transform: uppercase; letter-spacing: 0.5px; }
.section-title .unit { color: #585b70; font-weight: 400; text-transform: none; letter-spacing: 0; margin-left: 6px; }
.card, .net-box, .disk-box { background: #1e1e2e; border-radius: 8px; padding: 8px 10px; margin-bottom: 10px; }
.net-total { display: flex; gap: 16px; font-size: 14px; font-weight: 600; margin-bottom: 4px; }
.down { color: #a6e3a1; } .up { color: #89b4fa; }
.net-if, .io-row { display: flex; gap: 10px; color: #a6adc8; padding: 1px 0; }
.ifname { color: #6c7086; min-width: 56px; }
.disk-row { margin-bottom: 6px; }
.disk-head { display: flex; justify-content: space-between; margin-bottom: 2px; }
.bar { height: 6px; background: #313244; border-radius: 3px; overflow: hidden; }
.bar-fill { height: 100%; background: #89b4fa; border-radius: 3px; transition: width 0.5s; }
.bar-fill.warn { background: #f38ba8; }
.chart-box { height: 116px; width: 100%; }
.load-box { color: #6c7086; text-align: center; padding: 4px 0; }
.proc-box { background: #1e1e2e; border-radius: 8px; padding: 10px; margin-bottom: 10px; }
.proc-count { color: #6c7086; font-weight: 400; font-size: 10px; margin-left: 6px; text-transform: none; letter-spacing: 0; }
.proc-head, .proc-row { display: grid; grid-template-columns: 50px 1fr 52px 66px; gap: 6px; align-items: center; }
.proc-head { color: #6c7086; font-size: 10px; padding-bottom: 4px; border-bottom: 1px solid #313244; margin-bottom: 4px; }
.proc-row { padding: 2px 0; font-size: 11px; }
.proc-row .pid { color: #6c7086; }
.proc-row .pname { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.proc-row .pcpu { text-align: right; color: #a6e3a1; }
.proc-row .pcpu.hot { color: #f38ba8; font-weight: 600; }
.proc-row .pmem { text-align: right; color: #a6adc8; }
</style>
