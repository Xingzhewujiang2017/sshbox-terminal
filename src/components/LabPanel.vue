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
  key?: string
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
  (e: 'connect', hostId: string): void
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

// --- 离线主机：勾选/双击即建立会话，连接成功后自动进入已选 --------------------
interface LabRow {
  key: string
  sid?: string
  hostId?: string | null
  label: string
  ip: string
  status: string
}
const offlineRows = computed<LabRow[]>(() => {
  const connIds = new Set<string>()
  connectedTabs.value.forEach((t) => {
    if (t.hostId) connIds.add(t.hostId)
  })
  // 同名同 IP 的重复条目只显示一条（hosts.json 可能有多份），避免
  // "点连接只成功一台 / 演练台出现两个同名主机"的错觉。
  const seen = new Set<string>()
  return props.hosts.hosts
    .filter((h) => !connIds.has(h.id))
    .filter((h) => {
      const key = `${h.name ?? ''}|${h.host ?? ''}`
      if (seen.has(key)) return false
      seen.add(key)
      return true
    })
    .map((h) => ({
      key: `h-${h.id}`,
      hostId: h.id,
      label: h.name || h.host || h.id,
      ip: h.host ?? '',
      status: 'offline',
    }))
    .filter((r) => {
      if (groupSel.value === '') return true
      const g = props.hosts.hosts.find((x) => x.id === r.hostId)?.group || ''
      return groupSel.value === '__none' ? g === '' : g === groupSel.value
    })
})
const allRows = computed<LabRow[]>(() => {
  const conn = shownTabs.value.map((t) => ({
    key: `s-${t.sid}`,
    sid: t.sid,
    hostId: t.hostId,
    label: t.label,
    ip: hostIp(t),
    status: 'connected',
  }))
  return [...conn, ...offlineRows.value]
})
/** 等待连接的离线主机（hostId）。 */
const pendingConnect = ref<Set<string>>(new Set())
function connectOffline(r: LabRow) {
  if (!r.hostId) return
  emit('connect', r.hostId)
  pendingConnect.value = new Set([...pendingConnect.value, r.hostId])
}
function toggleRow(r: LabRow) {
  if (r.status === 'connected') toggle(r.sid!)
  else connectOffline(r)
}
// 连接成功 → 自动勾选该主机（趋势图随之出现）
watch(
  () => connectedTabs.value.map((t) => t.sid).join(','),
  () => {
    if (!pendingConnect.value.size) return
    const s = new Set(selected.value)
    const pend = new Set(pendingConnect.value)
    let changed = false
    for (const t of connectedTabs.value) {
      if (t.hostId && pend.has(t.hostId)) {
        s.add(t.sid)
        pend.delete(t.hostId)
        changed = true
      }
    }
    if (changed) {
      selected.value = s
      pendingConnect.value = pend
    }
  }
)

function rowLabel(sid: string): string {
  return allRows.value.find((r) => r.sid === sid)?.label ?? sid
}
function rowIp(sid: string): string {
  return allRows.value.find((r) => r.sid === sid)?.ip ?? ''
}

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
  // 丢包显示平滑：最近 3 次采样均值 —— 单次统计 ±10% 波动正常，直接显示会在
  // 注入 10% 时乱跳 0/10/20…，均值后曲线稳定在真实值附近
  let loss = p.loss != null ? +(p.loss.toFixed(1)) : 0
  if (p.loss != null) {
    const wins = [loss, ...s.loss.slice(-2).filter((v): v is number => v > 0)]
    if (wins.length > 1) loss = wins.reduce((a, b) => a + b, 0) / wins.length
  }
  s.loss.push(loss)
  // 数据永远保留「本次」全量（上限 2h ≈ 2400 点）；窗口切换只裁剪显示，
  // 不丢历史 —— 从 15分 切回 3分 再切回 15分，数据还在。
  const cap = 2400
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
  checkInjectEffect()
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
/** 跟踪 chart 容器尺寸：透明布局后才 init 的 0×0 画布靠它救回来。 */
const resizeObs: ResizeObserver[] = []
function attachResize(el: HTMLDivElement, c: echarts.ECharts) {
  const ro = new ResizeObserver(() => c.resize())
  ro.observe(el)
  resizeObs.push(ro)
}

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
    // v-for ref 回调在元素刚挂载时触发，此时很可能还没布局（容器 0×0），
    // echarts.init 会得到 0×0 画布 —— 所以 init 必须推到 nextTick 之后，
    // 并再用 ResizeObserver 保尺寸（容器真正落地后自动 resize 重绘）。
    if (el?.cpu && !charts.value[t.sid].cpu) {
      const c = initCpu(el.cpu)
      charts.value[t.sid].cpu = c
      attachResize(el.cpu, c)
      void nextTick(() => c.resize())
    }
    if (el?.ping && !charts.value[t.sid].ping) {
      const c = initPing(el.ping)
      charts.value[t.sid].ping = c
      attachResize(el.ping, c)
      void nextTick(() => c.resize())
    }
  }
}

function updateCharts() {
  for (const t of selTabs.value) {
    const s = series.value[t.sid]
    if (!s) continue
    const c = charts.value[t.sid]
    // 窗口=显示裁剪（slice），原始数据不动
    const cap = WINDOW_CAP[windowMode.value]
    const tt = s.t.slice(-cap)
    const cpu = s.cpu.slice(-cap)
    const mem = s.mem.slice(-cap)
    const lat = s.lat.slice(-cap)
    const loss = s.loss.slice(-cap)
    const mark = s.inject.filter((mk) => mk.atIdx < tt.length).map((mk) => ({ xAxis: mk.atIdx }))
    c?.cpu?.setOption({ xAxis: { data: tt }, series: [{ data: cpu }, { data: mem }] })
    c?.ping?.setOption({ xAxis: { data: tt }, series: [{ data: lat }, { data: loss }, { markLine: { data: mark } }] })
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

/** 注入效果检测：注入后 25s 内时延/丢包毫无变化 → 判定注入疑似未生效
 *  （目标机缺 sch_netem 等），如实提示而不是让用户对着平线猜。 */
const injWatch = ref<Record<string, { baseLat: number; baseLoss: number; at: number }>>({})
const injInert = ref<string[]>([])
function armInjectWatch(targets: { sid: string; label: string }[]) {
  const w: Record<string, { baseLat: number; baseLoss: number; at: number }> = {}
  for (const t of targets) {
    const s = series.value[t.sid]
    w[t.sid] = {
      baseLat: s && s.lat.length ? s.lat[s.lat.length - 1] : NaN,
      baseLoss: s && s.loss.length ? s.loss[s.loss.length - 1] : NaN,
      at: Date.now(),
    }
  }
  injWatch.value = w
  injInert.value = []
}
function checkInjectEffect() {
  const w = injWatch.value
  if (!Object.keys(w).length) return
  const inert: string[] = []
  const now = Date.now()
  for (const sid of Object.keys(w)) {
    const meta = w[sid]
    if (now - meta.at < 25000) continue // 还在观察期，留着下次判
    const s = series.value[sid]
    if (!s || !s.lat.length) {
      delete injWatch.value[sid]
      continue
    }
    const latMax = Math.max(...s.lat)
    const latChanged = Math.abs(latMax - meta.baseLat) >= 8
    const lossChanged = s.loss.some((l) => l > 0)
    if (!latChanged && !lossChanged) inert.push(sid)
    delete injWatch.value[sid]
  }
  if (inert.length) {
    injInert.value = inert.map((sid) => selTabs.value.find((t) => t.sid === sid)?.label ?? sid)
  }
}

// --- 故障注入（目标=勾选，逻辑同 v1） -----------------------------------------
const injectType = ref<'delay' | 'loss' | 'blip' | 'clear'>('delay')
const injectDelayMs = ref(200)
const injectLossPct = ref(50)
const injectBlipOn = ref(5)
const injectBlipOff = ref(10)
const injectBlipN = ref(3)
const injectNicMode = ref<'auto' | 'manual'>('auto')
const injectRecover = ref(true)
const injectRecoverSecs = ref(60)
const injectMsg = ref('')
/** 手动模式下每台主机各自的网卡名（key = 主机行 key） */
const manualNics = ref<Record<string, string>>({})
/** 会话 sid → 主机行 key（手动网卡按主机记录/读取用） */
function keyOf(sid: string): string {
  return allRows.value.find((r) => r.sid === sid)?.key ?? sid
}
/** 注入事件流水（每台一次，导出事件 CSV 据此逐台记录，4 台必 4 条） */
const labEvents = ref<{ label: string; time: string; kind: string; recAt: string; note: string }[]>([])
const injectConfirm = ref<{ n: number; cmd: string; preview: string; targets: { sid: string; label: string; cmd: string }[] } | null>(null)

/** auto 模式：注入前逐台探测默认路由出接口，确认弹窗显示每台接口、命令各自生成 */
async function probeNic(sid: string): Promise<string> {
  try {
    const r = await api.execBatch(sid, `ip route 2>/dev/null | awk '/^default/ {print $5; exit}'`, 15)
    const v = (r.stdout ?? '').trim()
    if (r.ok && v) return v
  } catch { /* 探测失败走回退 */ }
  return ''
}

/** 权限自适应：非 root 时尝试 sudo（免密才成）；都没有就明确报错而不是静默失败 */
const TC_BOOT = [
  `if [ "$(id -u)" != 0 ]; then command -v sudo >/dev/null 2>&1 && TC="sudo tc" || { echo "需要 root 或免密 sudo（tc 需要 CAP_NET_ADMIN）"; exit 1; }; else TC="tc"; fi`,
]

function ifaceSetup(manualNic: string, autoNic: string): string[] {
  if (injectNicMode.value === 'manual') {
    const nic = manualNic.trim()
    if (!nic) return [...TC_BOOT, `echo "该主机未填写网卡，中止"; exit 1`]
    return [...TC_BOOT, `IFACE=${nic}`]
  }
  if (autoNic) {
    // 探测成功：网卡已确定，命令直接写死 —— 每台各不相同
    return [...TC_BOOT, `IFACE=${autoNic}`, `echo "注入接口: $IFACE"`]
  }
  return [...TC_BOOT, `IFACE=$(ip route 2>/dev/null | awk '/^default/ {print $5; exit}')`]
}

function netemLine(rule: string): string {
  if (injectRecover.value && injectRecoverSecs.value > 0) {
    return `"$TC" qdisc replace dev "$IFACE" root netem ${rule} && sleep ${injectRecoverSecs.value} && "$TC" qdisc del dev "$IFACE" root && echo '已自动恢复'`
  }
  return `"$TC" qdisc replace dev "$IFACE" root netem ${rule}`
}

/** 为单个目标生成注入命令（每台网卡/权限各自独立） */
function buildInjectCmd(t: { sid: string; key?: string }, autoNic: string): string {
  const head =
    '# 故障注入（SSHBox 演练台）· 回车由你执行；需要 root 或免密 sudo' +
    (injectNicMode.value === 'auto' ? '；网卡自动探测' : '；网卡手动指定')
  const manualNic = injectNicMode.value === 'manual' ? (manualNics.value[t.key ?? ''] ?? '') : ''
  const setup = ifaceSetup(manualNic, autoNic)
  const body: string[] = [...setup]
  if (injectType.value === 'clear') {
    body.push(`"$TC" qdisc del dev "$IFACE" root && echo 已清除该网卡上的规则; "$TC" qdisc show dev "$IFACE" || true`)
  } else if (injectType.value === 'delay') {
    body.push(netemLine(`delay ${injectDelayMs.value}ms`))
  } else if (injectType.value === 'loss') {
    body.push(netemLine(`loss ${injectLossPct.value}%`))
  } else {
    const on = Math.max(1, injectBlipOn.value)
    const off = Math.max(1, injectBlipOff.value)
    const n = Math.max(1, injectBlipN.value)
    for (let i = 0; i < n; i++) {
      body.push(`"$TC" qdisc replace dev "$IFACE" root netem loss 100% && sleep ${on} && "$TC" qdisc del dev "$IFACE" root && sleep ${off}`)
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
  void (async () => {
    if (injectNicMode.value === 'auto') injectMsg.value = '正在探测各台默认路由出接口…'
    // auto：逐台探测接口；manual：取每台自己的输入
    const nicOf: Record<string, string> = {}
    if (injectNicMode.value === 'auto') {
      for (const t of targets) nicOf[t.sid] = await probeNic(t.sid)
    }
    const per = targets.map((t) => ({ t, cmd: buildInjectCmd({ sid: t.sid, key: keyOf(t.sid) }, nicOf[t.sid] ?? '') }))
    for (const p of per) emit('command', { sid: p.t.sid, text: p.cmd })
    if (injectType.value === 'clear') {
      // 清除：移除该主机的注入标记（黄线）与效果检测，不画新标记。
      for (const t of targets) {
        const s = series.value[t.sid]
        if (s) s.inject = []
      }
    } else {
      for (const p of per) {
        markInject(p.t.sid, injectType.value, injectRecover.value ? injectRecoverSecs.value : 0)
        labEvents.value.push({
          label: p.t.label,
          time: new Date().toTimeString().slice(0, 8),
          kind: `inject:${injectType.value}`,
          recAt: '',
          note: nicOf[p.t.sid] ? `接口 ${nicOf[p.t.sid]}` : (injectNicMode.value === 'manual' ? '手动网卡' : '远端探测'),
        })
      }
      armInjectWatch(targets.map((t) => ({ sid: t.sid, label: t.label })))
    }
    const preview = per.map((p) => {
      const nicShown = injectNicMode.value === 'auto' ? nicOf[p.t.sid] : (manualNics.value[keyOf(p.t.sid)] ?? '')
      return `${p.t.label}${nicShown ? ' [' + nicShown + ']' : ''}: ${(p.cmd.split('\n').find((l) => l.includes('qdisc')) || p.cmd.split('\n').pop() || '').slice(0, 56)}`
    }).join('  |  ')
    injectConfirm.value = { n: targets.length, cmd: per.map((p) => p.cmd).join('\n'), preview, targets: per.map((p) => ({ sid: p.t.sid, label: p.t.label, cmd: p.cmd })) }
    injectMsg.value = `已按主机分别生成命令并填入 ${targets.length} 台终端的终端（回车执行；也可点立即执行）`
    void nextTick(() => updateCharts())
  })()
}

function confirmInjectExecute() {
  const c = injectConfirm.value
  if (!c) return
  for (const t of c.targets) emit('command', { sid: t.sid, text: t.cmd, execute: true })
  injectConfirm.value = null
  armInjectWatch(c.targets)
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
    const evRows: string[] = ['host,time,event,kind,recover_at,note']
    // 注入事件流水：每台一条（4 台注入必 4 条，含失败/权限提示上下文）
    for (const ev of labEvents.value) {
      evRows.push([esc(ev.label), esc(ev.time), ev.kind.split(':')[0], esc(ev.kind), ev.recAt ? esc(new Date(ev.recAt).toTimeString().slice(0, 8)) : '', esc(ev.note)].join(','))
    }
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
    showToast(`✅ 导出成功（${targets.length} 台，事件 ${labEvents.value.length} 条）\n${sampleR.path}\n${eventR.path}`)
  } catch (e) {
    exportMsg.value = `导出失败：${String(e)}`
    showToast('❌ 导出失败：' + String(e))
  }
}

// --- 导出成功提示（toast 浮层，3 秒自动消失） -----------------------------------
const toast = ref('')
let toastTimer = 0
function showToast(msg: string) {
  toast.value = msg
  clearTimeout(toastTimer)
  toastTimer = window.setTimeout(() => (toast.value = ''), 4500)
}

// --- 终端展开（双击主机行） ----------------------------------------------------
const openTerm = ref<Set<string>>(new Set())
function toggleTerm(sid: string) {
  const s = new Set(openTerm.value)
  if (s.has(sid)) s.delete(sid)
  else {
    s.add(sid)
    // 会话事件流不回溯：最初 welcome+提示符早已流过，新终端挂载时看不到。
    // 发一个空回车让 shell 重新打印提示符（无害，只是换一行）。
    api.termWrite(sid, '\r').catch(() => {})
  }
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
  // 窗口切换只是显示裁剪（updateCharts 内 slice），数据不动。
  void nextTick(() => updateCharts())
})

// 取消勾选（或清空）→ 释放该主机的图表实例与数据，
// 重新勾选时重新 init —— 否则图表 DOM 重建而 echarts 实例残留，
// 新容器永远不会被初始化，趋势图表现为"没有数据"。
watch(
  () => selTabs.value.map((t) => t.sid).join(','),
  (cur, old) => {
    if (!old) return
    const curSet = new Set(cur ? cur.split(',') : [])
    for (const sid of old.split(',')) {
      if (!sid || curSet.has(sid)) continue
      charts.value[sid]?.cpu?.dispose()
      charts.value[sid]?.ping?.dispose()
      delete charts.value[sid]
      delete chartEls.value[sid]
      delete series.value[sid]
    }
  }
)

onMounted(() => {
  emit('enter')
  pollTimer = setInterval(() => void tickPing(), 3000)
  void tickPing()
})
onBeforeUnmount(() => {
  if (pollTimer) clearInterval(pollTimer)
  resizeObs.forEach((ro) => ro.disconnect())
  resizeObs.length = 0
  Object.values(charts.value).forEach((c) => {
    c.cpu?.dispose()
    c.ping?.dispose()
  })
})
</script>

<template>
  <div class="lab-page">
    <div class="lab">
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

      <!-- 主机列表：在线会话 + 离线主机（勾选/双击即连接），双击名称展开该主机终端 -->
      <div class="host-list">
        <div v-for="r in allRows" :key="r.key" class="h-row" :class="{ on: r.status === 'connected' ? selected.has(r.sid!) : pendingConnect.has(r.hostId!) }">
          <label class="check" :title="r.status === 'offline' ? '勾选=连接该主机' : ''">
            <input
              type="checkbox"
              :checked="r.status === 'connected' ? selected.has(r.sid!) : pendingConnect.has(r.hostId!)"
              @change="toggleRow(r)"
            />
          </label>
          <span class="h-name" :title="r.sid ?? r.hostId ?? ''" @dblclick="r.status === 'connected' ? toggleTerm(r.sid!) : connectOffline(r)">{{ r.label }}</span>
          <span class="h-ip dim">{{ r.ip || '临时连接' }}</span>
          <span class="h-stat" :class="{ conn: r.status === 'connected' }">{{ r.status === 'connected' ? '● 在线' : '离线' }}</span>
          <input v-if="injectNicMode === 'manual'" v-model="manualNics[r.key]" class="num nic-in" placeholder="网卡" style="width: 64px" :title="`${r.label} 的网卡名（如 eth0/ens33），注入/清除都用它`" />
          <button v-if="r.status === 'connected'" class="btn ghost mini" @click="toggleTerm(r.sid!)" :title="openTerm.has(r.sid!) ? '收起终端' : '展开该主机终端（双击名称也行）'">
            {{ openTerm.has(r.sid!) ? '收起终端' : '终端 ▸' }}
          </button>
          <button v-else class="btn ghost mini" :disabled="pendingConnect.has(r.hostId!)" @click="connectOffline(r)">
            {{ pendingConnect.has(r.hostId!) ? '连接中…' : '连接' }}
          </button>
        </div>
        <div v-if="!allRows.length" class="dim empty">该分组下没有主机 —— 先在左侧添加主机</div>
        <div v-if="openTerm.size" class="term-zone">
          <div v-for="sid in [...openTerm]" :key="sid" class="lab-term-block">
            <div class="lt-head">
              <span class="lt-name">{{ rowLabel(sid) }}</span>
              <span class="dim lt-ip">{{ rowIp(sid) }}</span>
              <span class="spacer"></span>
              <button class="btn ghost mini" @click="toggleTerm(sid)" title="收起该终端">收起 ✕</button>
            </div>
            <TerminalPane
              :sid="sid"
              :active="true"
              @data="(d) => api.termWrite(sid, d)"
            />
          </div>
        </div>
      </div>

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
          <span class="dim" v-if="injectNicMode === 'manual'">（选中主机的行内填各自网卡）</span>
          <span class="dim" v-else>（逐台探测默认路由出接口）</span>
          <label class="check"><input type="checkbox" v-model="injectRecover" /> 到期自动恢复</label>
          <input v-model.number="injectRecoverSecs" class="num" style="width: 60px" :disabled="!injectRecover" />
          <span class="dim">秒</span>
        </div>
        <div class="inj-row">
          <button class="btn" :disabled="!selTabs.length" @click="runInject">生成并填入终端（{{ selTabs.length }} 台）</button>
          <template v-if="injectConfirm">
            <div class="inj-confirm">
              <div class="dim" style="margin-bottom: 4px">将执行（每台命令已按各自网卡生成）：<code>{{ injectConfirm.preview }}</code></div>
              <button class="btn danger" @click="confirmInjectExecute">⚠ 立即执行（{{ injectConfirm.n }} 台）</button>
              <button class="btn ghost" style="margin-left: 6px" @click="injectConfirm = null">仅等待</button>
            </div>
          </template>
          <span v-if="injectMsg" class="dim" :class="{ err: injectMsg.startsWith('失败') }">{{ injectMsg }}</span>
          <div v-if="injInert.length" class="err inj-inert">
            ⚠ 注入疑似未生效（{{ injInert.join('、') }}）：25 秒内时延/丢包曲线无变化 —— 目标机可能缺少 sch_netem 内核模块（受限内核/容器环境常见），请检查终端里的命令输出；可用「清除」恢复。
          </div>
        </div>
      </div>

      <!-- 底部趋势图：每台两块 ECharts -->
      <template v-if="selTabs.length">
        <div class="sec-title">
          主机趋势图 <span class="dim">· 3s 采样 | 注入在图中画黄虚线标记</span>
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
      <!-- 导出/操作结果 toast 浮层（右上，3 秒自动消失） -->
      <transition name="fade">
        <div v-if="toast" class="lab-toast">{{ toast }}</div>
      </transition>
    </div>
  </div>
</template>

<style scoped>
.lab-page { position: fixed; inset: 0; z-index: 40; background: var(--ctp-base); display: flex; }
.lab { width: 100%; height: 100%; overflow-y: auto; display: flex; flex-direction: column; gap: 10px; padding: 12px 16px; box-sizing: border-box; }
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
.term-zone { margin-top: 6px; display: grid; grid-template-columns: repeat(auto-fill, minmax(440px, 1fr)); gap: 8px; }
.lab-term-block { border: 1px solid var(--ctp-surface0); border-radius: 6px; padding: 6px; }
.lt-head { display: flex; align-items: center; gap: 8px; margin-bottom: 4px; }
.lt-name { font-weight: 600; font-size: 12px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.lt-ip { font-size: 11px; }
.term-zone :deep(.term-wrap) { border: 1px solid var(--ctp-surface0); border-radius: 6px; height: 260px; }
.empty { padding: 8px 4px; }
.nic-in { flex: 0 0 auto; }
.lab-toast {
  position: fixed;
  top: 12px;
  right: 12px;
  z-index: 60;
  max-width: 460px;
  background: var(--ctp-mantle);
  border: 1px solid var(--ctp-green);
  color: var(--ctp-text);
  border-radius: 8px;
  padding: 10px 14px;
  font-size: 12px;
  line-height: 1.5;
  white-space: pre-line;
  box-shadow: 0 6px 18px rgba(0, 0, 0, 0.35);
}
.fade-enter-active, .fade-leave-active { transition: opacity 0.25s; }
.fade-enter-from, .fade-leave-to { opacity: 0; }
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
.inj-inert { margin-top: 4px; font-size: 11.5px; background: var(--ctp-mantle); padding: 4px 8px; border-radius: 6px; }
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