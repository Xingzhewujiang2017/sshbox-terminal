<script setup lang="ts">
import { onMounted, onBeforeUnmount, ref, computed } from 'vue'
import { listen } from '@tauri-apps/api/event'
import * as echarts from 'echarts'

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

const props = defineProps<{ sid: string; active: boolean }>()

const info = ref<StaticInfo | null>(null)
const metrics = ref<Metrics | null>(null)

// Rolling history (5 min @ 2s = 150 points)
const MAX_POINTS = 150
const history = ref<{ t: string[]; cpu: number[]; mem: number[]; rx: number[]; tx: number[] }>({
  t: [], cpu: [], mem: [], rx: [], tx: [],
})

let unlistenStatic: (() => void) | null = null
let unlistenMetrics: (() => void) | null = null
let chart: echarts.ECharts | null = null
const chartEl = ref<HTMLDivElement>()

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

const netTotals = computed(() => {
  if (!metrics.value) return { rx: 0, tx: 0 }
  return metrics.value.net.reduce(
    (a, n) => ({ rx: a.rx + n.rx_bps, tx: a.tx + n.tx_bps }),
    { rx: 0, tx: 0 }
  )
})

function updateChart() {
  if (!chart || !metrics.value) return
  const h = history.value
  chart.setOption({
    xAxis: { data: h.t },
    series: [
      { data: h.cpu },
      { data: h.mem },
    ],
  })
}

onMounted(async () => {
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
    while (h.t.length > MAX_POINTS) {
      h.t.shift(); h.cpu.shift(); h.mem.shift(); h.rx.shift(); h.tx.shift()
    }
    updateChart()
  })

  if (chartEl.value) {
    chart = echarts.init(chartEl.value, 'dark')
    chart.setOption({
      backgroundColor: 'transparent',
      grid: { left: 36, right: 12, top: 30, bottom: 24 },
      legend: { data: ['CPU %', '内存 %'], textStyle: { fontSize: 10 }, top: 0 },
      tooltip: { trigger: 'axis' },
      xAxis: { type: 'category', data: [], axisLabel: { fontSize: 9 } },
      yAxis: { type: 'value', min: 0, max: 100, axisLabel: { fontSize: 9 } },
      series: [
        { name: 'CPU %', type: 'line', data: [], smooth: true, showSymbol: false, lineStyle: { width: 1.5 }, areaStyle: { opacity: 0.15 } },
        { name: '内存 %', type: 'line', data: [], smooth: true, showSymbol: false, lineStyle: { width: 1.5 }, areaStyle: { opacity: 0.15 } },
      ],
    })
  }
})

onBeforeUnmount(() => {
  unlistenStatic?.()
  unlistenMetrics?.()
  chart?.dispose()
})
</script>

<template>
  <div class="monitor">
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

      <div class="net-box">
        <div class="section-title">网络</div>
        <div class="net-total">
          <span class="down">↓ {{ fmtBytes(netTotals.rx) }}</span>
          <span class="up">↑ {{ fmtBytes(netTotals.tx) }}</span>
        </div>
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
        <div v-for="io in metrics.disk_io" :key="io.name" class="io-row">
          <span class="ifname">{{ io.name }}</span>
          <span>R {{ fmtBytes(io.read_bps) }}</span>
          <span>W {{ fmtBytes(io.write_bps) }}</span>
        </div>
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

      <div class="chart-box" ref="chartEl"></div>

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
.net-box, .disk-box { background: #1e1e2e; border-radius: 8px; padding: 8px 10px; margin-bottom: 10px; }
.net-total { display: flex; gap: 16px; font-size: 14px; font-weight: 600; margin-bottom: 4px; }
.down { color: #a6e3a1; } .up { color: #89b4fa; }
.net-if, .io-row { display: flex; gap: 10px; color: #a6adc8; padding: 1px 0; }
.ifname { color: #6c7086; min-width: 56px; }
.disk-row { margin-bottom: 6px; }
.disk-head { display: flex; justify-content: space-between; margin-bottom: 2px; }
.bar { height: 6px; background: #313244; border-radius: 3px; overflow: hidden; }
.bar-fill { height: 100%; background: #89b4fa; border-radius: 3px; transition: width 0.5s; }
.bar-fill.warn { background: #f38ba8; }
.chart-box { height: 160px; margin-bottom: 8px; }
.load-box { color: #6c7086; text-align: center; padding: 4px 0; }
.proc-box { background: #1e1e2e; border-radius: 8px; padding: 10px; margin-bottom: 10px; }
.proc-count { color: #6c7086; font-weight: 400; font-size: 10px; margin-left: 6px; }
.proc-head, .proc-row { display: grid; grid-template-columns: 50px 1fr 52px 66px; gap: 6px; align-items: center; }
.proc-head { color: #6c7086; font-size: 10px; padding-bottom: 4px; border-bottom: 1px solid #313244; margin-bottom: 4px; }
.proc-row { padding: 2px 0; font-size: 11px; }
.proc-row .pid { color: #6c7086; }
.proc-row .pname { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.proc-row .pcpu { text-align: right; color: #a6e3a1; }
.proc-row .pcpu.hot { color: #f38ba8; font-weight: 600; }
.proc-row .pmem { text-align: right; color: #a6adc8; }
</style>
