<script setup lang="ts">
/**
 * 故障演练台 v2（Fault Lab）—— 单机/集群故障测试工作台，全程不跳页。
 *
 * v2 变化（按用户拍板）：
 *  1. 顶栏分组下拉（全部/组），主机列表只显示 名称 + IP，双击展开该主机终端（不关演练台）；
 *  2. 底部每台两块 ECharts 大图（CPU/内存、时延/丢包，同监控平台），窗口可切 3分/15分/本次；
 *  3. 趋势数据 = 3s 采样（ping_now 快 ping + fleet 最新 CPU/内存合成一行），注入画时间线标记；
 *  4. 导出两份 CSV（采样数据 + 注入事件），落盘 exports/；
 *  5. 打开演练台时 App 折叠监控面板（emit 'enter'）。
 *
 * 安全：注入/执行的每个动作都需用户手动确认；AI/agent 不自动触发。
 */
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import * as echarts from 'echarts'
import { chartPalette, themeVersion } from '../theme'
import { api, type LabExec, type Metrics } from '../api'
import { sampleFor } from '../fleet'
import TerminalPane from './TerminalPane.vue'

interface LabTab {
  sid: string
  label: string
  hostId?: string | null
  status: string
}

interface HostItem {
  id: string
  group?: string
  name?: string
  host?: string
}

interface HostsLike {
  groups: string[]
  hosts: HostItem[]
}

interface Series {
  t: string[]
  cpu: number[]
  mem: number[]
  lat: number[]
  loss: number[]
  inject: { atIdx: number; recAt: number; kind: string }[]
}

const emit = defineEmits<{
  (e: 'close'): void
  (e: 'enter'): void
  (e: 'command', p: { sid: string; text: string; execute?: boolean }): void
}>()

const props = defineProps<{ tabs: LabTab[]; hosts: HostsLike }>()

const connectedTabs = computed(() => props.tabs.filter((t) => t.status === 'connected'))

// --- 分组与主机列表 ----------------------------------------------------------
/** '' = 全部；组名 = 该组；'__none' = 未分组/临时连接。 */
const groupSel = ref('')
const groupOptions = computed(() => {
  const g = props.hosts.groups.filter(Boolean)
  return ['', ...g, '__none']
})
const groupLabel = (g: string) => (g === '' ? '全部' : g === '__none' ? '未分组' : g)
const groupOfHost = (id: string | null | undefined): string | null => {
  if (!id) return null
  const h = props.hosts.hosts.find((x) => x.id === id)
  return h ? h.group || '' : null
}
const hostIp = (t: LabTab): string => {
  const h = props.hosts.hosts.find((x) => x.id === t.hostId)
  return h?.host ?? ''
}
const shownTabs = computed(() =>
  connectedTabs.value.filter((t) => {
    if (groupSel.value === '') return true
    const g = groupOfHost(t.hostId) ?? ''
    return groupSel.value === '__none' ? g === '' : g === groupSel.value
  })
)

// --- 目标勾选 ---------------------------------------------------------------
const selected = ref<Set<string>>(new Set())
function toggle(sid: string) {
  const s = new Set(selected.value)
  if (s.has(sid)) s.delete(sid)
  else s.add(sid)
  selected.value = s
}
function selectAll() {
  selected.value = new Set(shownTabs.value.map((t) => t.sid))
}
function selectNone() {
  selected.value = new Set()
}
function onGroupChange() {
  selectNone()
}
const selTabs = computed(() => connectedTabs.value.filter((t) => selected.value.has(t.sid)))

// --- 采样数据（3s 一档，窗口 3分/15分/本次） ----------------------------------
const windowMode = ref<'3m' | '15m' | 'all'>('3m')
const WINDOW_CAP: Record<string, number> = { '3m': 60, '15m': 300, all: 2400 }
const series = ref<Record<string, Series>>({})
let pollTimer: ReturnType<typeof setInterval> | null = null

function pushSample(sid: string, p: { lat: number | null; loss: number | null }, m?: Metrics) {
  const s = (series.value[sid] ??= { t: [], cpu: [], mem: [], lat: [], loss: [], inject: [] })
  s.t.push(new Date().toTimeString().slice(0, 8))
  s.cpu.push(+(m?.cpu_pct ?? 0).toFixed(1))
  s.mem.push(+(m?.mem_pct ?? 0).toFixed(1))
  s.lat.push(p.lat != null ? +(p.lat.toFixed(2)) : 0)
  s.loss.push(p.loss != null ? +(p.loss.toFixed(1)) : 0)
  const cap = WINDOW_CAP[windowMode.value]
  if (s.t.length > cap) {
    s.t.shift(); s.cpu.shift(); s.mem.shift(); s.lat.shift(); s.loss.shift()
  }
  s.inject.forEach((mk) => (mk.atIdx--))
  s.inject = s.inject.filter((mk) => mk.atIdx >= 0)
}

async function tickPing() {
  const targets = selTabs.value
  if (!targets.length) return
  await Promise.allSettled(
    targets.map(async (t) => {
      try {
        const p = await api.pingNow(t.sid)
        pushSample(t.sid, { lat: p.rtt_avg, loss: p.loss_pct }, sampleFor(t.sid)?.metrics)
      } catch {
        /* 单台失败不影响其它 */
      }
    })
  )
  await nextTick()
  updateCharts()
}

// --- ECharts（同监控平台配置） ------------------------------------------------
const AXIS = { fontSize: 9 }
const LEGEND = { top: 0, itemHeight: 8, itemWidth: 12, icon: 'roundRect' }
function colorOption(p: ReturnType<typeof chartPalette>, colors: string[]) {
  return {
    color: colors,
    textStyle: { color: p.text, fontSize: 10 },
    axisPointer: { link: [{ xAxisIndex: 'all' }], lineStyle: { color: p.axis } },
  }
}
function axisStyle(p: ReturnType<typeof chartPalette>) {
  return {
    axisLabel: { color: p.axis, fontSize: 9 },
    axisLine: { lineStyle: { color: p.split } },
    splitLine: { lineStyle: { color: p.split } },
    nameTextStyle: { color: p.axis, fontSize: 9 },
  }
}
const chartEls = ref<Record<string, { cpu?: HTMLDivElement; ping?: HTMLDivElement }>>({})
const charts = ref<Record<string, { cpu?: echarts.ECharts; ping?: echarts.ECharts }>>({})

function initCpu(el: HTMLDivElement): echarts.ECharts {
  const p = chartPalette()
  const c = echarts.init(el)
  c.setOption({
    backgroundColor: 'transparent',
    ...colorOption(p, [p.blue, p.green]),
    grid: { left: 34, right: 10, top: 24, bottom: 18 },
    legend: { ...LEGEND, data: ['CPU %', '内存 %'] },
    tooltip: { trigger: 'axis', valueFormatter: (v: number) => v.toFixed(1) + '%' },
    xAxis: { type: 'category', data: [], axisLabel: AXIS, axisLine: { lineStyle: { color: p.split } } },
    yAxis: { type: 'value', min: 0, max: 100, ...axisStyle(p) },
    series: [['CPU %', p.blue], ['内存 %', p.green]].map(([name, color]) => ({
      name, type: 'line', data: [], smooth: true, showSymbol: false,
      lineStyle: { width: 1.5, color }, itemStyle: { color }, areaStyle: { opacity: 0.12, color },
    })),
  })
  return c
}

function initPing(el: HTMLDivElement): echarts.ECharts {
  const p = chartPalette()
  const c = echarts.init(el)
  c.setOption({
    backgroundColor: 'transparent',
    ...colorOption(p, [p.blue, p.red]),
    grid: { left: 42, right: 42, top: 24, bottom: 18 },
    legend: { ...LEGEND, data: ['时延 ms', '丢包 %'] },
    tooltip: {
      trigger: 'axis',
      formatter: (ps: unknown) => {
        const a = ps as { marker: string; seriesName: string; value: number; axisValue: string }[]
        const rows = a.map((x) =>
          x.seriesName === '时延 ms' ? `${x.marker}时延：${(+x.value).toFixed(2)} ms` : `${x.marker}丢包：${(+x.value).toFixed(1)} %`
        )
        const inj = (a[0] as unknown as { dataIndex?: number })?.dataIndex ?? -1
        return (a[0]?.axisValue ?? '') + (inj >= 0 ? '<br/>▲ 注入时刻' : '') + '<br/>' + rows.join('<br/>')
      },
    },
    xAxis: { type: 'category', data: [], axisLabel: AXIS, axisLine: { lineStyle: { color: p.split } } },
    yAxis: [
      { type: 'value', name: 'ms', min: 0, ...axisStyle(p) },
      { type: 'value', name: '%', min: 0, max: 100, ...axisStyle(p), splitLine: { show: false } },
    ],
    series: [
      { name: '时延 ms', type: 'line', data: [], yAxisIndex: 0, smooth: true, showSymbol: false, lineStyle: { width: 1.5, color: p.blue }, itemStyle: { color: p.blue }, areaStyle: { opacity: 0.12, color: p.blue }, markLine: { symbol: 'none', lineStyle: { color: p.yellow, width: 1.3, type: 'dashed' }, label: { show: false }, data: [] } },
      { name: '丢包 %', type: 'line', data: [], yAxisIndex: 1, smooth: true, showSymbol: false, lineStyle: { width: 1.5, color: p.red, type: 'dashed' }, itemStyle: { color: p.red }, areaStyle: { opacity: 0.1, color: p.red } },
    ],
  })
  return c
}

function ensureCharts(tabs: LabTab[]) {
  for (const t of tabs) {
    if (!charts.value[t.sid]) charts.value[t.sid] = {}
    const el = chartEls.value[t.sid]
    if (el?.cpu && !charts.value[t.sid].cpu) charts.value[t.sid].cpu = initCpu(el.cpu)
    if (el?.ping && !charts.value[t.sid].ping) charts.value[t.sid].ping = initPing(el.ping)
  }
}

function updateCharts() {
  for (const t of selTabs.value) {
    const s = series.value[t.sid]
    if (!s) continue
    const c = charts.value[t.sid]
    c?.cpu?.setOption({ xAxis: { data: s.t }, series: [{ data: s.cpu }, { data: s.mem }] })
    const mark = s.inject.map((mk) => ({ xAxis: mk.atIdx }))
    c?.ping?.setOption({ xAxis: { data: s.t }, series: [{ data: s.lat }, { data: s.loss }, { markLine: { data: mark } }] })
  }
}

function setChartRef(sid: string, kind: 'cpu' | 'ping', el: HTMLDivElement | null) {
  if (!el) return
  if (!chartEls.value[sid]) chartEls.value[sid] = {}
  chartEls.value[sid][kind] = el
  ensureCharts([{ sid } as LabTab])
}

// --- 注入时间线 --------------------------------------------------------------
function markInject(sid: string, kind: string, recoverSecs: number) {
  const s = (series.value[sid] ??= { t: [], cpu: [], mem: [], lat: [], loss: [], inject: [] })
  s.inject.push({ atIdx: Math.max(0, s.lat.length - 1), recAt: recoverSecs > 0 ? Date.now() + recoverSecs * 1000 : 0, kind })
}

// --- 故障注入（目标=勾选，逻辑同 v1） -----------------------------------------
const injectType = ref<'delay' | 'loss' | 'blip' | 'clear'>('delay')
const injectDelayMs = ref(200)
const injectLossPct = ref(50)
const injectBlipOn = ref(5)
const injectBlipOff = ref(10)
const injectBlipN = ref(3)
const injectNicMode = ref<'auto' | 'manual'>('auto')
const injectNic = ref('eth0')
const injectRecover = ref(true)
const injectRecoverSecs = ref(60)
const injectMsg = ref('')
const injectConfirm = ref<{ n: number; cmd: string; preview: string; targets: { sid: string; label: string }[] } | null>(null)

function ifaceSetup(): string[] {
  const manual = injectNicMode.value === 'manual' ? injectNic.value.trim() : ''
  if (manual) {
    return [
      `IFACE=${manual}`,
      `ip link show "$IFACE" >/dev/null 2>&1 || { echo "网卡不存在（${manual}），中止"; exit 1; }`,
    ]
  }
  return [
    `IFACE=$(ip route 2>/dev/null | awk '/^default/ {print $5; exit}')`,
    `[ -n "$IFACE" ] || { echo "未找到默认路由出接口，中止"; exit 1; }`,
    `echo "注入接口: $IFACE"`,
  ]
}

function netemLine(rule: string): string {
  if (injectRecover.value && injectRecoverSecs.value > 0) {
    return `tc qdisc replace dev "$IFACE" root netem ${rule} && sleep ${injectRecoverSecs.value} && tc qdisc del dev "$IFACE" root && echo '已自动恢复'`
  }
  return `tc qdisc replace dev "$IFACE" root netem ${rule}`
}

function injectCommand(): string {
  const setup = ifaceSetup()
  const head =
    '# 故障注入（SSHBox 演练台）· 回车由你执行；需要 root' +
    (injectNicMode.value === 'auto' ? '；网卡自动探测（各台默认路由出口）' : '；网卡手动指定')
  const body: string[] = [...setup]
  if (injectType.value === 'clear') {
    body.push(`tc qdisc del dev "$IFACE" root 2>/dev/null; tc qdisc show dev "$IFACE" || true`)
  } else if (injectType.value === 'delay') {
    body.push(netemLine(`delay ${injectDelayMs.value}ms`))
  } else if (injectType.value === 'loss') {
    body.push(netemLine(`loss ${injectLossPct.value}%`))
  } else {
    const on = Math.max(1, injectBlipOn.value)
    const off = Math.max(1, injectBlipOff.value)
    const n = Math.max(1, injectBlipN.value)
    for (let i = 0; i < n; i++) {
      body.push(`tc qdisc replace dev "$IFACE" root netem loss 100% && sleep ${on} && tc qdisc del dev "$IFACE" root && sleep ${off}`)
    }
    body.push('echo 闪断结束')
  }
  return [head, ...body].join('\n')
}

function runInject() {
  const targets = selTabs.value
  if (!targets.length) {
    injectMsg.value = '失败：还没有勾选已连接的主机'
    return
  }
  const cmd = injectCommand()
  for (const t of targets) emit('command', { sid: t.sid, text: cmd })
  for (const t of targets) markInject(t.sid, injectType.value, injectRecover.value ? injectRecoverSecs.value : 0)
  const preview = (cmd.split('\n').find((l) => l.startsWith('tc qdisc')) || cmd.split('\n').pop() || '').slice(0, 60)
  injectConfirm.value = { n: targets.length, cmd, preview, targets: targets.map((t) => ({ sid: t.sid, label: t.label })) }
  injectMsg.value = `已把命令填入 ${targets.length} 台主机的终端，选择是否立即执行`
  void nextTick(() => updateCharts())
}

function confirmInjectExecute() {
  const c = injectConfirm.value
  if (!c) return
  for (const t of c.targets) emit('command', { sid: t.sid, text: c.cmd, execute: true })
  injectConfirm.value = null
  injectMsg.value = `已向 ${c.n} 台主机下发执行 —— 观察底部趋势图`
}

// --- 命令/脚本就地执行 --------------------------------------------------------
const execCmd = ref('')
const execTimeout = ref(60)
const execBusy = ref(false)
const execResults = ref<Record<string, LabExec>>({})
const execConfirm = ref<{ n: number; preview: string } | null>(null)

function runExecSubmit() {
  const cmd = (execCmd.value ?? '').trim()
  if (!cmd) return
  const n = selTabs.value.length
  if (!n) return
  execConfirm.value = { n, preview: cmd.split('\n')[0].slice(0, 80) }
}

async function runExec() {
  const c = execConfirm.value
  execConfirm.value = null
  if (!c) return
  const targets = selTabs.value
  if (!targets.length) return
  execBusy.value = true
  const results: Record<string, LabExec> = {}
  await Promise.allSettled(
    targets.map(async (t) => {
      try {
        results[t.sid] = await api.execBatch(t.sid, execCmd.value.trim(), execTimeout.value)
      } catch (e) {
        results[t.sid] = { sid: t.sid, ok: false, stdout: '', exit: -1, elapsed_ms: 0, error: String(e) }
      }
    })
  )
  execResults.value = results
  execBusy.value = false
}

// --- 导出（采样 CSV + 注入事件 CSV，复用后端落盘） ------------------------------
const exportMsg = ref('')
async function exportCsvs() {
  const targets = selTabs.value
  if (!targets.length) {
    exportMsg.value = '失败：还没有勾选主机'
    return
  }
  try {
    const ts = new Date().toISOString().slice(0, 19).replace(/[T:]/g, '-')
    // 采样 CSV
    const esc = (v: unknown) => `"${String(v ?? '').replace(/"/g, '""')}"`
    const head = ['host', 'time', 'cpu_pct', 'mem_pct', 'latency_ms', 'loss_pct', 'inject_kind'].join(',')
    const rows: string[] = [head]
    const evRows: string[] = ['host,time,event,kind,recover_at']
    for (const t of targets) {
      const s = series.value[t.sid]
      if (!s) continue
      const label = t.label
      // 事件 CSV（闪断/注入开始/恢复）
      s.inject.forEach((mk, idx) => {
        const tStr = s.t[mk.atIdx] ?? ''
        evRows.push([esc(label), esc(tStr), 'inject', esc(mk.kind), mk.recAt ? esc(new Date(mk.recAt).toTimeString().slice(0, 8)) : ''].join(','))
        void idx
      })
      for (let j = 0; j < s.t.length; j++) {
        const kindAt = s.inject.find((mk) => mk.atIdx === j)?.kind ?? ''
        rows.push([esc(label), esc(s.t[j]), s.cpu[j], s.mem[j], s.lat[j], s.loss[j], esc(kindAt)].join(','))
      }
    }
    const bom = String.fromCharCode(0xfeff)
    const sampleR = await api.exportCsvText(`lab-${ts}-samples.csv`, bom + rows.join(String.fromCharCode(13, 10)), 'lab')
    const eventR = await api.exportCsvText(`lab-${ts}-events.csv`, bom + evRows.join(String.fromCharCode(13, 10)), 'lab')
    exportMsg.value = `已导出 ${targets.length} 台：${sampleR.path} / ${eventR.path}`
  } catch (e) {
    exportMsg.value = `导出失败：${String(e)}`
  }
}

// --- 终端展开（双击主机行） ----------------------------------------------------
const openTerm = ref<Set<string>>(new Set())
function toggleTerm(sid: string) {
  const s = new Set(openTerm.value)
  if (s.has(sid)) s.delete(sid)
  else s.add(sid)
  openTerm.value = s
}

watch(themeVersion, () => {
  Object.values(charts.value).forEach((c) => {
    c.cpu?.dispose?.()
    c.ping?.dispose?.()
  })
  charts.value = {}
  void nextTick(() => ensureCharts(selTabs.value))
})

watch(windowMode, () => {
  for (const sid of Object.keys(series.value)) {
    const s = series.value[sid]
    const cap = WINDOW_CAP[windowMode.value]
    while (s.t.length > cap) {
      s.t.shift(); s.cpu.shift(); s.mem.shift(); s.lat.shift(); s.loss.shift()
    }
  }
  void nextTick(() => updateCharts())
})

onMounted(() => {
  emit('enter')
  pollTimer = setInterval(() => void tickPing(), 3000)
  void tickPing()
})
onBeforeUnmount(() => {
  if (pollTimer) clearInterval(pollTimer)
})
</script>

<template>
  <div class="mask" @click.self="emit('close')">
    <div class="lab panel">
      <div class="head">
        <span class="title">故障演练台</span>
        <select v-model="groupSel" class="sel" @change="onGroupChange" title="分组">
          <option v-for="g in groupOptions" :key="g" :value="g">{{ groupLabel(g) }}</option>
        </select>
        <span class="dim">已选 {{ selTabs.length }}/{{ shownTabs.length }} 台</span>
        <span class="spacer"></span>
        <button class="btn ghost" @click="selectAll">全选</button>
        <button class="btn ghost" @click="selectNone">清空</button>
        <button class="btn ghost" @click="exportCsvs" :disabled="!selTabs.length" title="导出勾选主机的采样数据 + 注入事件，两份 CSV">导出 CSV</button>
        <button class="btn ghost" @click="emit('close')">关闭</button>
      </div>

      <!-- 主机列表：名称 + IP，双击展开/收起该主机终端 -->
      <div class="host-list">
        <div v-for="t in shownTabs" :key="t.sid" class="h-row" :class="{ on: selected.has(t.sid) }">
          <label class="check"><input type="checkbox" :checked="selected.has(t.sid)" @change="toggle(t.sid)" /></label>
          <span class="h-name" :title="t.sid" @dblclick="toggleTerm(t.sid)">{{ t.label }}</span>
          <span class="h-ip dim">{{ hostIp(t) || '临时连接' }}</span>
          <span class="h-stat" :class="{ conn: t.status === 'connected' }">{{ t.status === 'connected' ? '● 在线' : '—' }}</span>
          <button class="btn ghost mini" @click="toggleTerm(t.sid)" :title="openTerm.has(t.sid) ? '收起终端' : '展开该主机终端（双击名称也行）'">
            {{ openTerm.has(t.sid) ? '收起终端' : '终端 ▸' }}
          </button>
        </div>
        <div v-if="!shownTabs.length" class="dim empty">该分组下没有已连接的主机 —— 先双击左侧主机建立会话</div>
        <div v-if="openTerm.size" class="term-zone">
          <TerminalPane v-for="sid in [...openTerm]" :key="sid" :sid="sid" :active="true" />
        </div>
      </div>

      <!-- 底部趋势图：每台两块 ECharts -->
      <template v-if="selTabs.length">
        <div class="sec-title">
          实时趋势 <span class="dim">· 3s 采样 | 注入在图中画黄虚线标记</span>
          <span class="seg">
            <button :class="{ on: windowMode === '3m' }" @click="windowMode = '3m'">近3分</button>
            <button :class="{ on: windowMode === '15m' }" @click="windowMode = '15m'">近15分</button>
            <button :class="{ on: windowMode === 'all' }" @click="windowMode = 'all'">本次</button>
          </span>
        </div>
        <div class="charts">
          <div v-for="t in selTabs" :key="t.sid" class="chart-card">
            <div class="chart-title">{{ t.label }} <span class="dim">{{ hostIp(t) }}</span></div>
            <div class="chart-box" :ref="(el) => setChartRef(t.sid, 'cpu', el as HTMLDivElement | null)"></div>
            <div class="chart-box" :ref="(el) => setChartRef(t.sid, 'ping', el as HTMLDivElement | null)"></div>
          </div>
        </div>
      </template>
      <div v-else class="dim empty">勾选主机后，这里显示每台的 CPU/内存 与 时延/丢包 趋势图</div>

      <!-- 故障注入 -->
      <div class="section">
        <div class="sec-title">故障注入 <span class="dim">（目标=上方勾选 · tc netem · 需要 root）</span></div>
        <div class="inj-row">
          <label class="check"><input type="radio" value="delay" v-model="injectType" /> 时延</label>
          <input v-model.number="injectDelayMs" class="num" style="width: 70px" title="时延 ms" />
          <span class="dim">ms</span>
          <label class="check"><input type="radio" value="loss" v-model="injectType" /> 丢包</label>
          <input v-model.number="injectLossPct" class="num" style="width: 60px" title="丢包 %" />
          <span class="dim">%</span>
          <label class="check"><input type="radio" value="blip" v-model="injectType" /> 闪断</label>
          <input v-model.number="injectBlipOn" class="num" style="width: 54px" />s 断
          <input v-model.number="injectBlipOff" class="num" style="width: 54px" />s 恢复 ×
          <input v-model.number="injectBlipN" class="num" style="width: 42px" />
          <label class="check"><input type="radio" value="clear" v-model="injectType" /> 清除</label>
        </div>
        <div class="inj-row">
          <span class="dim">网卡</span>
          <label class="check"><input type="radio" value="auto" v-model="injectNicMode" /> 自动</label>
          <label class="check"><input type="radio" value="manual" v-model="injectNicMode" /> 手动</label>
          <input v-if="injectNicMode === 'manual'" v-model="injectNic" class="num" style="width: 96px" />
          <label class="check"><input type="checkbox" v-model="injectRecover" /> 到期自动恢复</label>
          <input v-model.number="injectRecoverSecs" class="num" style="width: 60px" :disabled="!injectRecover" />
          <span class="dim">秒</span>
        </div>
        <div class="inj-row">
          <button class="btn" :disabled="!selTabs.length" @click="runInject">生成并填入终端（{{ selTabs.length }} 台）</button>
          <template v-if="injectConfirm">
            <div class="inj-confirm">
              <span class="dim">将执行：<code>{{ injectConfirm.preview }}</code></span>
              <button class="btn danger" @click="confirmInjectExecute">⚠ 立即执行（{{ injectConfirm.n }} 台）</button>
              <button class="btn ghost" style="margin-left: 6px" @click="injectConfirm = null">仅等待</button>
            </div>
          </template>
          <span v-if="injectMsg" class="dim" :class="{ err: injectMsg.startsWith('失败') }">{{ injectMsg }}</span>
        </div>
      </div>

      <!-- 命令/脚本就地执行 -->
      <div class="section">
        <div class="sec-title">
          命令 / 脚本就地执行
          <span class="dim">（目标=勾选 · only-read 通道 · 交互命令走列表里展开的终端）</span>
        </div>
        <textarea v-model="execCmd" class="exec-input" rows="3" spellcheck="false"
          placeholder="例如：uptime&#10;多行脚本也行：&#10;for i in 1 2 3; do echo tick $i; sleep 1; done"></textarea>
        <div class="inj-row">
          <span class="dim">超时</span>
          <input v-model.number="execTimeout" class="num" style="width: 60px" />
          <span class="dim">秒</span>
          <button class="btn" :disabled="!execCmd.trim() || !selTabs.length || execBusy" @click="runExecSubmit">
            {{ execBusy ? '执行中…' : `并行执行到 ${selTabs.length} 台` }}
          </button>
        </div>
        <div v-if="execConfirm" class="exec-confirm">
          <span class="dim">将执行到 <b>{{ execConfirm.n }} 台</b>：<code>{{ execConfirm.preview }}</code></span>
          <button class="btn danger" @click="runExec">确认执行</button>
          <button class="btn ghost" style="margin-left: 6px" @click="execConfirm = null">取消</button>
        </div>
        <div v-if="Object.keys(execResults).length" class="exec-outs">
          <div v-for="t in selTabs" :key="t.sid" class="exec-out">
            <div class="eo-head">
              <span class="eo-host">{{ t.label }}</span>
              <span v-if="execResults[t.sid]" class="eo-meta" :class="{ ok: execResults[t.sid]?.ok, bad: !execResults[t.sid]?.ok }">
                {{ execResults[t.sid]?.ok ? 'exit ' + execResults[t.sid]?.exit : '失败' }} · {{ execResults[t.sid]?.elapsed_ms }}ms
              </span>
            </div>
            <pre v-if="execResults[t.sid]?.stdout" class="eo-out">{{ execResults[t.sid]?.stdout }}</pre>
            <div v-if="execResults[t.sid]?.error" class="eo-err">{{ execResults[t.sid]?.error }}</div>
          </div>
        </div>
        <div v-if="exportMsg" class="dim" :class="{ err: exportMsg.startsWith('失败') }">{{ exportMsg }}</div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.lab { width: min(1240px, 95vw); max-height: 94vh; overflow-y: auto; display: flex; flex-direction: column; gap: 10px; }
.head { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.head .spacer { flex: 1; }
.title { font-weight: 700; font-size: 15px; }
.sel { background: var(--ctp-mantle); border: 1px solid var(--ctp-surface0); border-radius: 6px; color: var(--ctp-text); padding: 3px 6px; font-size: 12px; }
.host-list { border: 1px solid var(--ctp-surface0); border-radius: 8px; padding: 6px 8px; }
.h-row { display: flex; align-items: center; gap: 10px; padding: 3px 6px; border-radius: 6px; }
.h-row.on { background: var(--ctp-surface0); }
.h-name { font-weight: 600; font-size: 12.5px; cursor: pointer; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.h-ip { font-size: 11px; }
.h-stat { font-size: 11px; color: var(--ctp-overlay0); }
.h-stat.conn { color: var(--ctp-green); }
.btn.mini { padding: 1px 8px; font-size: 11px; }
.term-zone { margin-top: 6px; display: grid; grid-template-columns: repeat(auto-fill, minmax(420px, 1fr)); gap: 8px; }
.term-zone :deep(.term-wrap) { border: 1px solid var(--ctp-surface0); border-radius: 6px; height: 260px; }
.empty { padding: 8px 4px; }
.sec-title { font-weight: 600; display: flex; align-items: center; gap: 10px; margin: 4px 0 6px; }
.seg { display: inline-flex; border: 1px solid var(--ctp-surface0); border-radius: 6px; overflow: hidden; margin-left: auto; }
.seg button { background: transparent; border: none; color: var(--ctp-subtext0); font-size: 11px; padding: 3px 10px; cursor: pointer; }
.seg button.on { background: var(--ctp-blue); color: var(--on-accent); }
.charts { display: grid; grid-template-columns: repeat(auto-fill, minmax(360px, 1fr)); gap: 10px; }
.chart-card { border: 1px solid var(--ctp-surface0); border-radius: 8px; padding: 8px; }
.chart-title { font-weight: 600; font-size: 12px; margin-bottom: 4px; }
.chart-box { height: 130px; width: 100%; }
.section { border-top: 1px solid var(--ctp-surface0); padding-top: 8px; }
.inj-row { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; margin-top: 6px; }
.inj-row .num { width: 64px; }
.inj-confirm { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.inj-confirm code, .exec-confirm code { background: var(--ctp-mantle); padding: 1px 6px; border-radius: 4px; font-size: 11px; }
.inj-confirm .danger, .exec-confirm .danger { background: var(--ctp-red); color: var(--on-accent); border: none; font-weight: 600; }
.err { color: var(--ctp-red); }
.exec-input { width: 100%; background: var(--ctp-mantle); border: 1px solid var(--ctp-surface0); border-radius: 6px; color: var(--ctp-text); font-family: Consolas, monospace; font-size: 12px; padding: 8px; resize: vertical; }
.exec-confirm { margin-top: 6px; display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.exec-outs { margin-top: 8px; display: grid; grid-template-columns: repeat(auto-fill, minmax(340px, 1fr)); gap: 8px; }
.exec-out { border: 1px solid var(--ctp-surface0); border-radius: 6px; padding: 6px 8px; }
.eo-head { display: flex; align-items: center; gap: 8px; }
.eo-host { font-weight: 600; font-size: 12px; }
.eo-meta { font-size: 11px; }
.eo-meta.ok { color: var(--ctp-green); }
.eo-meta.bad { color: var(--ctp-red); }
.eo-out { margin: 4px 0 0; padding: 6px; background: var(--ctp-mantle); border-radius: 4px; font-size: 11px; max-height: 140px; overflow-y: auto; white-space: pre-wrap; word-break: break-all; }
.eo-err { color: var(--ctp-red); font-size: 11px; margin-top: 3px; }
</style>