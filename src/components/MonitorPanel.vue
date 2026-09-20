<script setup lang="ts">
import { onMounted, onBeforeUnmount, reactive, ref, computed, nextTick, watch } from 'vue'
import type { PingInfo } from '../api'
import { listen } from '@tauri-apps/api/event'
import * as echarts from 'echarts'
import { chartPalette, themeVersion } from '../theme'
import { api, type DiskUsage, type Metrics } from '../api'
import Pager from './Pager.vue'

interface StaticInfo {
  hostname: string
  os_pretty: string
  kernel: string
  arch: string
  cpu_model: string
  cpu_cores: number
  mem_total_kb: number
  uptime_secs: number
  disks: DiskUsage[]
}
interface FailedUnit { name: string; desc: string }
interface PortInfo { proto: string; port: number; addrs: string[] }
interface ContainerInfo { name: string; image: string; status: string }
interface ServiceInfo {
  failed: FailedUnit[]
  ports: PortInfo[]
  port_total: number
  containers: ContainerInfo[]
  docker_available: boolean
}
interface TempReading { chip: string; label: string; celsius: number }
interface FanReading { chip: string; label: string; rpm: number }
/** 每项都可能缺：AMD 不报温度，老卡不报功耗，缺就是 null，不是 0。 */
interface GpuInfo {
  name: string
  vendor: string
  util_pct: number | null
  mem_used_mb: number | null
  mem_total_mb: number | null
  temp_c: number | null
  power_w: number | null
}
interface HardwareInfo {
  temps: TempReading[]
  fans: FanReading[]
  gpus: GpuInfo[]
}

const props = defineProps<{ sid: string; active: boolean; interval: number; topN: number }>()
const emit = defineEmits<{
  (e: 'setInterval', secs: number): void
  (e: 'setTopN', n: number): void
}>()

// --- 列表分页 ---------------------------------------------------------------
// 网络接口 / 磁盘 / 磁盘 IO / 进程 / 端口都按 10 条一页；点「全部」才铺开。
// 一份状态按 section 名索引，省得给每张卡写一遍。
const PAGE_SIZE = 10
const page = reactive<Record<string, number>>({
  net: 1,
  disk: 1,
  io: 1,
  proc: 1,
  port: 1,
})
const showAll = reactive<Record<string, boolean>>({
  net: false,
  disk: false,
  io: false,
  proc: false,
  port: false,
})

/** 当前页的切片。切页越界（数据变少）时自动回到最后一页，不留空白页。 */
function paged<T>(key: string, list: T[]): T[] {
  if (showAll[key]) return list
  const pages = Math.max(1, Math.ceil(list.length / PAGE_SIZE))
  const cur = Math.min(page[key] ?? 1, pages)
  if (cur !== page[key]) page[key] = cur
  return list.slice((cur - 1) * PAGE_SIZE, cur * PAGE_SIZE)
}

/** 进程条数选项：0 = 全部（后端不截断）。 */
const PROC_N = [12, 50, 0] as const
function procNLabel(n: number) {
  return n === 0 ? '全部' : String(n)
}

const info = ref<StaticInfo | null>(null)
const metrics = ref<Metrics | null>(null)
const paused = ref(false)
const services = ref<ServiceInfo | null>(null)
const hardware = ref<HardwareInfo | null>(null)
const ping = ref<PingInfo | null>(null)
/** 时延/丢包迷你趋势（#1）：15s 一档，最多 48 点。 */
const pingHist = ref<{ lat: number[]; loss: number[] }>({ lat: [], loss: [] })
/** 传感器多起来（8 核 + 2 个 NVMe）会淹掉面板，默认只露最热的几个。 */
const TEMP_SHOWN = 5
const tempsExpanded = ref(false)
const hasHardware = computed(
  () => !!hardware.value &&
    (hardware.value.temps.length > 0 || hardware.value.gpus.length > 0 || hardware.value.fans.length > 0),
)
const shownTemps = computed(() => {
  const t = hardware.value?.temps ?? []
  return tempsExpanded.value ? t : t.slice(0, TEMP_SHOWN)
})
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
let unlistenServices: (() => void) | null = null
let unlistenHardware: (() => void) | null = null
let unlistenPing: (() => void) | null = null

const rootEl = ref<HTMLDivElement>()
const cpuEl = ref<HTMLDivElement>()
const netEl = ref<HTMLDivElement>()
const ioEl = ref<HTMLDivElement>()
let cpuChart: echarts.ECharts | null = null
let netChart: echarts.ECharts | null = null
let ioChart: echarts.ECharts | null = null
let ro: ResizeObserver | null = null

const AXIS = { fontSize: 9 }
const LEGEND = { top: 0, itemHeight: 8, itemWidth: 12, icon: 'roundRect' }

/** 主题相关的那部分图表配置。切换主题时只重发这些，不碰数据。
 *  画布是像素，读不到 CSS 变量，所以颜色必须运行时从 chartPalette() 取。 */
function colorOption(p: ReturnType<typeof chartPalette>, seriesColors: string[]) {
  return {
    textStyle: { color: p.text },
    legend: { textStyle: { fontSize: 10, color: p.axis } },
    tooltip: { backgroundColor: p.surface1, borderColor: p.split, textStyle: { color: p.text } },
    xAxis: { axisLabel: { color: p.axis }, axisLine: { lineStyle: { color: p.split } } },
    yAxis: {
      axisLabel: { color: p.axis },
      splitLine: { lineStyle: { color: p.split } },
      axisLine: { lineStyle: { color: p.split } },
    },
    series: seriesColors.map((c) => ({
      itemStyle: { color: c },
      lineStyle: { color: c },
      areaStyle: { color: c },
    })),
  }
}

/**
 * 轴的样式。**每个图都会重建 xAxis/yAxis**（要加 type/min/formatter），
 * 重建就把 colorOption 里展开的轴样式整个盖掉了 —— 包括网格线颜色，
 * 于是退回 ECharts 默认的浅灰，深色主题下比曲线本身还显眼。
 * 所以凡是要自己写 xAxis/yAxis 的图，都得把这份样式再展开一次。
 */
function axisStyle(p: ReturnType<typeof chartPalette>) {
  return {
    axisLabel: { color: p.axis },
    splitLine: { lineStyle: { color: p.split } },
    axisLine: { lineStyle: { color: p.split } },
  }
}

/** 三个图的系列配色（和下面仪表环保持一致：CPU 蓝、内存绿、读橙、写紫）。 */
function seriesColors(): string[][] {
  const p = chartPalette()
  return [
    [p.blue, p.green],
    [p.green, p.blue],
    [p.peach, p.mauve],
  ]
}

/** 主题变了：只重发颜色，不动已经攒下来的历史数据。 */
function repaint() {
  const p = chartPalette()
  const cols = seriesColors()
  cpuChart?.setOption(colorOption(p, cols[0]))
  netChart?.setOption(colorOption(p, cols[1]))
  ioChart?.setOption(colorOption(p, cols[2]))
}

/** 温度分档：<60 正常，60-79 偏热，>=80 该看一眼了。 */
function heat(c: number): string {
  return c >= 80 ? 'hot' : c >= 60 ? 'warm' : 'ok'
}
function utilHeat(p: number): string {
  return p >= 85 ? 'hot' : p >= 50 ? 'warm' : 'ok'
}
/**
 * 环形图与折线图的百分比必须同精度。环形原来固定 toFixed(0)，
 * 而折线画的是 toFixed(1)：CPU 0.3% 时环形写「0%」、折线却在 0.3 处，
 * 看着像两个数据源在打架，其实同源，只是精度不同。
 * 低于 10 保留一位（0.3% 圆成 0% 会被读成「没数据」），10 以上整数够用。
 */
function pctText(v: number): string {
  return v < 10 ? v.toFixed(1) : v.toFixed(0)
}
/** VRAM 上百 GB 的卡按 MB 显示读不出来，超过 1 GB 换成 GB。 */
function fmtMB(mb: number | null): string {
  if (mb === null) return '—'
  return mb >= 1024 ? (mb / 1024).toFixed(1) + ' GB' : mb + ' MB'
}
/** 标签出现两次以上（两块 NVMe 都叫 Composite）就得靠芯片名区分。 */
const tempLabelCount = computed(() => {
  const n = new Map<string, number>()
  for (const t of hardware.value?.temps ?? []) n.set(t.label, (n.get(t.label) ?? 0) + 1)
  return n
})
/** "temp1" 这种标签本身没有信息量，得靠芯片名区分；thermal zone 的 type
 *  已经是人话（x86_pkg_temp），再拼上 "thermal" 只是噪音。 */
function tempLabel(t: TempReading): string {
  const dup = (tempLabelCount.value.get(t.label) ?? 0) > 1
  return t.label.startsWith('temp') || dup ? `${t.chip} ${t.label}` : t.label
}

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

/** 迷你折线 path（#1 时延趋势用）：自绘 SVG，归一化到各自最大值。 */
function spark(vals: number[], w: number, h: number): string {
  if (!vals.length) return ''
  const max = Math.max(...vals, 1)
  const step = w / Math.max(1, vals.length - 1)
  return vals
    .map((v, i) => `${i === 0 ? 'M' : 'L'}${(i * step).toFixed(1)},${(h - 2 - (h - 6) * (v / max)).toFixed(1)}`)
    .join(' ')
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
  const p = chartPalette()
  const c = echarts.init(el)
  c.setOption({
    backgroundColor: 'transparent',
    ...colorOption(p, seriesColors()[0]),
    grid: { left: 34, right: 10, top: 24, bottom: 20 },
    legend: { ...LEGEND, data: ['CPU %', '内存 %'] },
    tooltip: { trigger: 'axis', valueFormatter: (v: number) => v.toFixed(1) + '%' },
    xAxis: { type: 'category', data: [], axisLabel: AXIS, axisLine: { lineStyle: { color: p.split } } },
    yAxis: { type: 'value', min: 0, max: 100, ...axisStyle(p) },
    // 系列在这里重建，会盖掉上面 colorOption 展开的 series，所以颜色必须显式带上，
    // 否则这张图会退回 ECharts 自带调色板，切主题时颜色会跳一下。
    series: [['CPU %', p.blue], ['内存 %', p.green]].map(([name, color]) => ({
      name,
      type: 'line',
      data: [],
      smooth: true,
      showSymbol: false,
      lineStyle: { width: 1.5, color },
      itemStyle: { color },
      areaStyle: { opacity: 0.15, color },
    })),
  })
  return c
}

/** Rate chart (bytes/s, auto-scaled axis with human labels). */
function initRate(el: HTMLDivElement, names: string[], colors: string[]): echarts.ECharts {
  const p = chartPalette()
  const c = echarts.init(el)
  c.setOption({
    backgroundColor: 'transparent',
    ...colorOption(p, colors),
    grid: { left: 46, right: 10, top: 24, bottom: 20 },
    legend: { ...LEGEND, data: names },
    tooltip: { trigger: 'axis', valueFormatter: (v: number) => fmtBytes(v) },
    xAxis: { type: 'category', data: [], axisLabel: AXIS, axisLine: { lineStyle: { color: p.split } } },
    yAxis: { type: 'value', min: 0, ...axisStyle(p), axisLabel: { ...AXIS, formatter: fmtRateShort } },
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
  const cols = seriesColors()
  if (!cpuChart && cpuEl.value) cpuChart = initPct(cpuEl.value)
  if (!netChart && netEl.value) netChart = initRate(netEl.value, ['↓ 下行', '↑ 上行'], cols[1])
  if (!ioChart && ioEl.value) ioChart = initRate(ioEl.value, ['读', '写'], cols[2])
}

/** 主题一变立刻重上色，不等下一个采样点。 */
watch(themeVersion, () => repaint())

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
  unlistenServices = await listen<{ sid: string; services: ServiceInfo }>('ssh://services', (e) => {
    if (e.payload.sid === props.sid) services.value = e.payload.services
  })
  unlistenHardware = await listen<{ sid: string; hardware: HardwareInfo }>('ssh://hardware', (e) => {
    if (e.payload.sid === props.sid) hardware.value = e.payload.hardware
  })
  // 一次性事件可能在我们订阅之前就发过了（密码弹窗那条路径必然如此），
  // 所以订阅之后补拉一次后端缓存 —— 否则表头永远停在"采集系统信息中…"。
  try {
    const snap = (await api.monitorStatic(props.sid)) as StaticInfo | null
    if (snap && !info.value) info.value = snap
  } catch {
    /* 还没采到就等事件 */
  }
  unlistenPing = await listen<{ sid: string; ping: PingInfo | null }>('ssh://ping', (e) => {
    if (e.payload.sid === props.sid) ping.value = e.payload.ping
    // 时延/丢包迷你趋势（#1）：慢采集 15s 一档，最多留 48 点（约 12 分钟）。
    // 没测到网关（null）时整次跳过 —— 不画 0ms 骗人。
    if (e.payload.ping) {
      const h = pingHist.value
      h.lat.push(+e.payload.ping.rtt_avg.toFixed(2))
      h.loss.push(+e.payload.ping.loss_pct.toFixed(1))
      if (h.lat.length > 48) {
        h.lat.shift()
        h.loss.shift()
      }
    }
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
  unlistenServices?.()
  unlistenHardware?.()
  unlistenPing?.()
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
          <div
            class="ring"
            :style="{ '--pct': metrics.cpu_pct + '%', '--color': metrics.cpu_pct > 85 ? 'var(--ctp-red)' : 'var(--ctp-blue)' }"
            :title="`CPU 瞬时 ${metrics.cpu_pct.toFixed(1)}%（与下面折线图最后一个点同一份数据）`"
          >
            <div class="ring-val">{{ pctText(metrics.cpu_pct) }}%</div>
          </div>
          <div class="gauge-label">CPU</div>
        </div>
        <div class="gauge">
          <div
            class="ring"
            :style="{ '--pct': metrics.mem_pct + '%', '--color': metrics.mem_pct > 85 ? 'var(--ctp-red)' : 'var(--ctp-green)' }"
            :title="`内存瞬时 ${metrics.mem_pct.toFixed(1)}%（与下面折线图最后一个点同一份数据）`"
          >
            <div class="ring-val">{{ pctText(metrics.mem_pct) }}%</div>
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
        <div v-if="ping" class="net-quality">
          <span>时延 {{ ping.rtt_avg.toFixed(2) }} ms</span>
          <span v-if="ping.jitter > 0">抖动 {{ ping.jitter.toFixed(2) }} ms</span>
          <span :class="{ bad: ping.loss_pct >= 5 }">丢包 {{ ping.loss_pct.toFixed(0) }}%</span>
          <span class="unit">到 {{ ping.target }}</span>
        </div>
        <svg
          v-if="pingHist.lat.length >= 2"
          viewBox="0 0 260 40"
          preserveAspectRatio="none"
          width="100%"
          height="40"
          class="ping-spark"
        >
          <path :d="spark(pingHist.lat, 260, 40)" fill="none" stroke="var(--ctp-blue)" stroke-width="1.3" />
          <path :d="spark(pingHist.loss, 260, 40)" fill="none" stroke="var(--ctp-red)" stroke-width="1.3" stroke-dasharray="3 2" />
        </svg>
        <div v-if="pingHist.lat.length >= 2" class="ping-legend">
          时延<svg viewBox="0 0 20 6" width="20" height="6"><path d="M0 3 L20 3" stroke="var(--ctp-blue)" stroke-width="2" /></svg>
          丢包%<svg viewBox="0 0 20 6" width="20" height="6"><path d="M0 3 L20 3" stroke="var(--ctp-red)" stroke-width="2" stroke-dasharray="3 2" /></svg>
          · 近 {{ pingHist.lat.length }} 次（每 15 秒）
        </div>
        <div class="net-total">
          <span class="down">↓ {{ fmtBytes(netTotals.rx) }}</span>
          <span class="up">↑ {{ fmtBytes(netTotals.tx) }}</span>
        </div>
        <div class="chart-box" ref="netEl"></div>
        <div v-for="n in paged('net', metrics.net)" :key="n.name" class="net-if">
          <span class="ifname">{{ n.name }}</span>
          <span>↓{{ fmtBytes(n.rx_bps) }}</span>
          <span>↑{{ fmtBytes(n.tx_bps) }}</span>
        </div>
        <Pager
          :total="metrics.net.length"
          :page="page.net"
          :all="showAll.net"
          unit="张网卡"
          @update:page="page.net = $event"
          @update:all="showAll.net = $event"
        />
      </div>

      <div class="disk-box">
        <div class="section-title">磁盘</div>
        <div v-for="d in paged('disk', metrics.disks)" :key="d.mount" class="disk-row">
          <div class="disk-head">
            <span>{{ d.mount }}</span>
            <span>{{ fmtKB(d.used_kb) }} / {{ fmtKB(d.total_kb) }}</span>
          </div>
          <div class="bar"><div class="bar-fill" :class="{ warn: d.use_pct > 85 }" :style="{ width: d.use_pct + '%' }"></div></div>
        </div>
        <Pager
          :total="metrics.disks.length"
          :page="page.disk"
          :all="showAll.disk"
          unit="个挂载点"
          @update:page="page.disk = $event"
          @update:all="showAll.disk = $event"
        />
        <template v-if="metrics.disk_io.length">
          <div class="section-title">磁盘 IO <span class="unit">{{ windowLabel }}</span></div>
          <div class="chart-box" ref="ioEl"></div>
          <div v-for="io in paged('io', metrics.disk_io)" :key="io.name" class="io-row">
            <span class="ifname">{{ io.name }}</span>
            <span>R {{ fmtBytes(io.read_bps) }}</span>
            <span>W {{ fmtBytes(io.write_bps) }}</span>
          </div>
          <Pager
            :total="metrics.disk_io.length"
            :page="page.io"
            :all="showAll.io"
            unit="块设备"
            @update:page="page.io = $event"
            @update:all="showAll.io = $event"
          />
        </template>
      </div>

      <div v-if="services" class="svc-box">
        <div class="section-title">服务与端口 <span class="unit">每 15 秒刷新</span></div>

        <template v-if="services.failed.length">
          <div class="fail-head">⚠ {{ services.failed.length }} 个单元处于失败状态</div>
          <div v-for="u in services.failed" :key="u.name" class="fail-row">
            <span class="fail-name">{{ u.name }}</span>
            <span class="fail-desc" :title="u.desc">{{ u.desc }}</span>
          </div>
        </template>
        <div v-else class="svc-ok">✓ systemd 没有失败单元</div>

        <div class="svc-sub">监听端口 {{ services.port_total }} 个</div>
        <div class="port-wrap">
          <span
            v-for="p in paged('port', services.ports)"
            :key="p.proto + p.port"
            class="port-chip"
            :title="p.addrs.join('\n')"
          >{{ p.port }}/{{ p.proto }}</span>
          <span v-if="!services.port_total" class="svc-sub muted">（没读到监听端口）</span>
        </div>
        <Pager
          :total="services.ports.length"
          :page="page.port"
          :all="showAll.port"
          unit="个端口"
          @update:page="page.port = $event"
          @update:all="showAll.port = $event"
        />

        <template v-if="services.docker_available">
          <div class="svc-sub">容器 {{ services.containers.length }} 个</div>
          <div v-for="c in services.containers" :key="c.name" class="ctr-row">
            <span class="ctr-name">{{ c.name }}</span>
            <span class="ctr-status" :title="c.image">{{ c.status }}</span>
          </div>
          <div v-if="!services.containers.length" class="svc-sub muted">（docker 在跑，没有容器）</div>
        </template>
        <div v-else class="svc-sub muted">未检测到 docker（无守护进程或无权限）</div>
      </div>

      <div v-if="hasHardware" class="svc-box">
        <div class="section-title">温度与 GPU <span class="unit">每 15 秒刷新</span></div>

        <div v-for="g in hardware!.gpus" :key="g.vendor + g.name" class="gpu-row">
          <span class="gpu-name" :title="g.name">{{ g.name }}</span>
          <template v-if="g.util_pct !== null">
            <span class="gpu-bar">
              <i :class="utilHeat(g.util_pct)" :style="{ width: Math.min(100, g.util_pct) + '%' }"></i>
            </span>
            <span class="gpu-pct">{{ g.util_pct.toFixed(0) }}%</span>
          </template>
          <span v-if="g.mem_total_mb" class="gpu-vram">VRAM {{ fmtMB(g.mem_used_mb) }} / {{ fmtMB(g.mem_total_mb) }}</span>
          <span v-if="g.temp_c !== null" class="temp-chip" :class="heat(g.temp_c)">{{ g.temp_c.toFixed(0) }}°C</span>
          <span v-if="g.power_w !== null" class="gpu-w">{{ g.power_w.toFixed(0) }} W</span>
        </div>

        <div class="port-wrap">
          <span
            v-for="t in shownTemps"
            :key="t.chip + t.label"
            class="temp-chip"
            :class="heat(t.celsius)"
            :title="`${t.chip} · ${t.label}`"
          >{{ tempLabel(t) }} {{ t.celsius.toFixed(0) }}°C</span>
          <button
            v-if="hardware!.temps.length > TEMP_SHOWN"
            class="temp-more"
            @click="tempsExpanded = !tempsExpanded"
          >{{ tempsExpanded ? '收起' : `+${hardware!.temps.length - TEMP_SHOWN} 个传感器` }}</button>
        </div>

        <div v-if="hardware!.fans.length" class="svc-sub">
          风扇
          <span v-for="f in hardware!.fans" :key="f.chip + f.label" class="fan-chip">
            {{ f.label }} {{ f.rpm }} RPM
          </span>
        </div>
      </div>

      <div class="proc-box">
        <div class="section-title">
          进程<span class="proc-count">共 {{ metrics.proc_total }} 个 · 按瞬时 CPU 排序</span>
          <span class="proc-n">
            <button
              v-for="n in PROC_N"
              :key="n"
              class="chip mini"
              :class="{ on: props.topN === n }"
              :title="n === 0 ? '取回全部进程（列表仍按 10 条一页）' : `只取 CPU 前 ${n} 个`"
              @click="emit('setTopN', n)"
            >{{ procNLabel(n) }}</button>
          </span>
        </div>
        <div class="proc-head"><span>PID</span><span>名称</span><span>CPU</span><span>内存</span></div>
        <div v-for="p in paged('proc', metrics.processes)" :key="p.pid" class="proc-row">
          <span class="pid">{{ p.pid }}</span>
          <span class="pname" :title="p.name">{{ p.name }}</span>
          <span class="pcpu" :class="{ hot: p.cpu_pct > 50 }">{{ p.cpu_pct.toFixed(1) }}%</span>
          <span class="pmem">{{ fmtKB(p.rss_kb) }}</span>
        </div>
        <Pager
          :total="metrics.processes.length"
          :page="page.proc"
          :all="showAll.proc"
          unit="个进程"
          @update:page="page.proc = $event"
          @update:all="showAll.proc = $event"
        />
        <div v-if="metrics.proc_total > metrics.processes.length" class="svc-sub muted">
          只取了前 {{ metrics.processes.length }} 个；点上方「全部」取回所有 {{ metrics.proc_total }} 个进程
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
  background: var(--ctp-mantle);
  color: var(--ctp-text);
  font-size: 12px;
  padding: 10px;
  box-sizing: border-box;
}
.bar { display: flex; align-items: center; gap: 4px; margin-bottom: 8px; flex-wrap: wrap; }
.bar-label { color: var(--ctp-overlay0); font-size: 11px; margin-right: 2px; }
.bar-spacer { flex: 1; }
.chip {
  background: var(--ctp-base); color: var(--ctp-subtext0); border: 1px solid var(--ctp-surface0); border-radius: 5px;
  font-size: 11px; padding: 2px 7px; cursor: pointer; font-family: inherit;
}
.chip:hover { border-color: var(--ctp-surface2); color: var(--ctp-text); }
.chip.on { background: var(--ctp-blue); border-color: var(--ctp-blue); color: var(--on-accent); font-weight: 600; }
.chip.warn { background: var(--ctp-yellow); border-color: var(--ctp-yellow); color: var(--on-accent); font-weight: 600; }
.chip.mini { padding: 0 5px; font-size: 10px; border-radius: 4px; }
.paused-banner {
  background: var(--ctp-surface0); color: var(--ctp-yellow); border-radius: 6px; padding: 5px 8px;
  margin-bottom: 8px; font-size: 11px; text-align: center;
}
.placeholder { color: var(--ctp-overlay0); padding: 20px 0; text-align: center; }
.static-box { background: var(--ctp-base); border-radius: 8px; padding: 10px; margin-bottom: 10px; }
.host { font-size: 15px; font-weight: 600; margin-bottom: 6px; color: var(--ctp-blue); }
.row { display: flex; gap: 6px; padding: 2px 0; }
.row span { color: var(--ctp-overlay0); min-width: 36px; }
.gauges { display: flex; gap: 10px; justify-content: space-around; margin-bottom: 10px; }
.gauge { text-align: center; }
.ring {
  width: 72px; height: 72px; border-radius: 50%;
  background: conic-gradient(var(--color) var(--pct), var(--ctp-surface0) 0);
  display: flex; align-items: center; justify-content: center;
  margin: 0 auto;
}
.ring::before { content: ''; position: absolute; }
.ring-val {
  width: 56px; height: 56px; border-radius: 50%; background: var(--ctp-mantle);
  display: flex; align-items: center; justify-content: center;
  font-size: 14px; font-weight: 600;
}
.gauge-label { margin-top: 4px; color: var(--ctp-subtext0); font-size: 11px; }
.section-title { font-weight: 600; color: var(--ctp-subtext0); margin: 8px 0 4px; font-size: 11px; text-transform: uppercase; letter-spacing: 0.5px; }
.section-title .unit { color: var(--ctp-surface2); font-weight: 400; text-transform: none; letter-spacing: 0; margin-left: 6px; }
.card, .net-box, .disk-box { background: var(--ctp-base); border-radius: 8px; padding: 8px 10px; margin-bottom: 10px; }
.net-total { display: flex; gap: 16px; font-size: 14px; font-weight: 600; margin-bottom: 4px; }
.net-quality { display: flex; gap: 12px; font-size: 12px; color: var(--muted); margin-bottom: 6px; }
.net-quality .bad { color: #e5484d; font-weight: 600; }
.ping-spark { display: block; margin: 2px 0; background: var(--ctp-mantle); border-radius: 4px; }
.ping-legend { display: flex; align-items: center; gap: 4px; font-size: 10px; color: var(--ctp-overlay0); margin-bottom: 4px; }
.down { color: var(--ctp-green); } .up { color: var(--ctp-blue); }
.net-if, .io-row { display: flex; gap: 10px; color: var(--ctp-subtext0); padding: 1px 0; }
.ifname { color: var(--ctp-overlay0); min-width: 56px; }
.disk-row { margin-bottom: 6px; }
.disk-head { display: flex; justify-content: space-between; margin-bottom: 2px; }
.bar { height: 6px; background: var(--ctp-surface0); border-radius: 3px; overflow: hidden; }
.bar-fill { height: 100%; background: var(--ctp-blue); border-radius: 3px; transition: width 0.5s; }
.bar-fill.warn { background: var(--ctp-red); }
.chart-box { height: 116px; width: 100%; }
.load-box { color: var(--ctp-overlay0); text-align: center; padding: 4px 0; }
.svc-box { background: var(--ctp-base); border-radius: 8px; padding: 10px; margin-bottom: 10px; }
.svc-ok { color: var(--ctp-green); font-size: 11px; padding: 2px 0 4px; }
.svc-sub { color: var(--ctp-subtext0); font-size: 11px; margin: 6px 0 3px; }
.svc-sub.muted { color: var(--ctp-surface2); }
.fail-head { color: var(--ctp-red); font-size: 11px; font-weight: 600; margin-bottom: 3px; }
.fail-row { display: flex; gap: 8px; padding: 1px 0; font-size: 11px; }
.fail-name { color: var(--ctp-red); min-width: 108px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.fail-desc { color: var(--ctp-overlay0); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.port-wrap { display: flex; flex-wrap: wrap; gap: 4px; }
.port-chip {
  background: var(--ctp-surface0); color: var(--ctp-text); border-radius: 4px;
  padding: 1px 5px; font-size: 10px; font-family: ui-monospace, monospace;
}
.port-more { color: var(--ctp-overlay0); font-size: 10px; padding: 1px 3px; }
.ctr-row { display: flex; gap: 8px; padding: 1px 0; font-size: 11px; }
.ctr-name { color: var(--ctp-blue); min-width: 108px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.ctr-status { color: var(--ctp-green); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.gpu-row { display: flex; align-items: center; gap: 6px; padding: 2px 0; font-size: 11px; flex-wrap: wrap; }
.gpu-name { color: var(--ctp-blue); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 140px; }
.gpu-bar { display: inline-block; width: 46px; height: 6px; background: var(--ctp-surface0); border-radius: 3px; overflow: hidden; }
.gpu-bar i { display: block; height: 100%; border-radius: 3px; }
.gpu-bar i.ok { background: var(--ctp-green); }
.gpu-bar i.warm { background: var(--ctp-yellow); }
.gpu-bar i.hot { background: var(--ctp-red); }
.gpu-pct { color: var(--ctp-text); font-family: ui-monospace, monospace; }
.gpu-vram { color: var(--ctp-overlay0); }
.gpu-w { color: var(--ctp-overlay0); font-family: ui-monospace, monospace; }
.temp-chip {
  background: var(--ctp-surface0); border-radius: 4px; padding: 1px 5px;
  font-size: 10px; font-family: ui-monospace, monospace;
}
.temp-chip.ok { color: var(--ctp-green); }
.temp-chip.warm { color: var(--ctp-yellow); }
.temp-chip.hot { color: var(--ctp-red); }
.temp-more {
  background: transparent; border: 1px solid var(--ctp-surface1); color: var(--ctp-overlay0);
  border-radius: 4px; padding: 0 5px; font-size: 10px; cursor: pointer;
}
.temp-more:hover { color: var(--ctp-text); border-color: var(--ctp-overlay0); }
.fan-chip { color: var(--ctp-subtext0); font-family: ui-monospace, monospace; margin-left: 6px; }
.proc-box { background: var(--ctp-base); border-radius: 8px; padding: 10px; margin-bottom: 10px; }
.proc-count { color: var(--ctp-overlay0); font-weight: 400; font-size: 10px; margin-left: 6px; text-transform: none; letter-spacing: 0; }
.proc-n { float: right; display: inline-flex; gap: 4px; align-items: center; text-transform: none; letter-spacing: 0; font-weight: 400; }
.proc-n::before { content: '取前'; color: var(--ctp-overlay0); font-size: 10px; }
.proc-head, .proc-row { display: grid; grid-template-columns: 50px 1fr 52px 66px; gap: 6px; align-items: center; }
.proc-head { color: var(--ctp-overlay0); font-size: 10px; padding-bottom: 4px; border-bottom: 1px solid var(--ctp-surface0); margin-bottom: 4px; }
.proc-row { padding: 2px 0; font-size: 11px; }
.proc-row .pid { color: var(--ctp-overlay0); }
.proc-row .pname { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.proc-row .pcpu { text-align: right; color: var(--ctp-green); }
.proc-row .pcpu.hot { color: var(--ctp-red); font-weight: 600; }
.proc-row .pmem { text-align: right; color: var(--ctp-subtext0); }
</style>
