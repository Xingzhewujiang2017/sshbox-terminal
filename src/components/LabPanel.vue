<script setup lang="ts">
/**
 * 故障演练台（Fault Lab）—— v0.6.1
 *
 * 场景：单机/集群故障测试，全程不跳页：
 *  1. 目标多选（已连接会话）；
 *  2. 每台一张实时卡：CPU/内存（fleet 事件流 ~1s）+ 时延/丢包（3s 高采快 ping），
 *     丢包 >0 / 时延突变红闪告警，注入时刻在曲线上画标记线；
 *  3. 故障注入（复用现有 tc netem 逻辑，目标=勾选，命令可「立即执行」）；
 *  4. 命令/脚本就地组播执行（后端 exec_batch：只读通道、带超时、返 exit code），
 *     执行前弹一次确认；不占用终端 pty，交互式命令仍去终端页。
 *
 * 安全边界：本面板的所有执行动作都需用户手动点确认；AI/agent 流程不自动触发。
 */
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { api, type LabExec, type Metrics } from '../api'
import { sampleFor } from '../fleet'

interface LabTab {
  sid: string
  label: string
  hostId?: string | null
  status: string
}

interface PingTick {
  lat: number[]
  loss: number[]
  t: string[]
}

const emit = defineEmits<{
  (e: 'close'): void
  (e: 'command', p: { sid: string; text: string; execute?: boolean }): void
}>()

const props = defineProps<{ tabs: LabTab[] }>()

const connectedTabs = computed(() => props.tabs.filter((t) => t.status === 'connected'))

// --- 目标多选 ---------------------------------------------------------------
const selected = ref<Set<string>>(new Set(connectedTabs.value.map((t) => t.sid)))
function toggle(sid: string) {
  const s = new Set(selected.value)
  if (s.has(sid)) s.delete(sid)
  else s.add(sid)
  selected.value = s
}
function selectAll() {
  selected.value = new Set(connectedTabs.value.map((t) => t.sid))
}
function selectNone() {
  selected.value = new Set()
}
const selTabs = computed(() => connectedTabs.value.filter((t) => selected.value.has(t.sid)))

// --- 实时观测（CPU/内存来自 fleet；时延/丢包 3s 高采） -----------------------
const pingHist = ref<Record<string, PingTick>>({})
const pingLast = ref<Record<string, { lat: number | null; loss: number | null; at: number }>>({})
const lostAlarm = ref<Record<string, boolean>>({})
let pollTimer: ReturnType<typeof setInterval> | null = null
let unsetAlarm: ReturnType<typeof setTimeout> | null = null

function sparkPath(vals: number[], w: number, h: number): string {
  if (!vals.length) return ''
  const max = Math.max(...vals, 1)
  const step = w / Math.max(1, vals.length - 1)
  return vals
    .map((v, i) => `${i === 0 ? 'M' : 'L'}${(i * step).toFixed(1)},${(h - 2 - (h - 6) * (v / max)).toFixed(1)}`)
    .join(' ')
}

async function tickPing() {
  const targets = selTabs.value
  if (!targets.length) return
  const jobs = targets.map(async (t) => {
    try {
      const p = await api.pingNow(t.sid)
      if (!p.rtt_avg && !p.loss_pct) return // 网关拿不到：跳过，不画 0 骗人
      const h = (pingHist.value[t.sid] ??= { lat: [], loss: [], t: [] })
      h.lat.push(+(p.rtt_avg ?? 0).toFixed(2))
      h.loss.push(+(p.loss_pct ?? 0).toFixed(1))
      h.t.push(new Date().toTimeString().slice(0, 8))
      if (h.lat.length > 60) {
        h.lat.shift(); h.loss.shift(); h.t.shift()
      }
      pingLast.value[t.sid] = { lat: p.rtt_avg, loss: p.loss_pct, at: Date.now() }
      // 丢包 >0 或时延 >50ms（局域网基准）红闪 3 秒
      const alertNow = (p.loss_pct ?? 0) > 0 || (p.rtt_avg ?? 0) >= 50
      if (alertNow) {
        lostAlarm.value = { ...lostAlarm.value, [t.sid]: true }
        if (unsetAlarm) clearTimeout(unsetAlarm)
        unsetAlarm = setTimeout(() => {
          lostAlarm.value = {}
        }, 4000)
      }
    } catch {
      /* 单台失败不影响其它台 */
    }
  })
  await Promise.allSettled(jobs)
}

function live(t: LabTab) {
  return sampleFor(t.sid)?.metrics
}
function cpuPct(m?: Metrics): number | null {
  return m ? m.cpu_pct : null
}
function memPct(m?: Metrics): number | null {
  return m ? m.mem_pct : null
}

// --- 注入时间线（标记线 + 恢复倒计时） --------------------------------------
interface InjectMark {
  atIdx: number
  recAt: number // epoch ms，0 = 不自动恢复
  kind: string
}
const injectMarks = ref<Record<string, InjectMark>>({})
const nowTick = ref(Date.now())
setInterval(() => (nowTick.value = Date.now()), 1000)
function markInject(sid: string, kind: string, recoverSecs: number) {
  const h = pingHist.value[sid]
  injectMarks.value = {
    ...injectMarks.value,
    [sid]: { atIdx: h ? h.lat.length : 0, recAt: recoverSecs > 0 ? Date.now() + recoverSecs * 1000 : 0, kind },
  }
}
function remainingSec(sid: string): number {
  const m = injectMarks.value[sid]
  if (!m || !m.recAt) return 0
  return Math.max(0, Math.ceil((m.recAt - nowTick.value) / 1000))
}

// --- 故障注入（复用总览逻辑，目标=勾选） ------------------------------------
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
  for (const t of targets) {
    markInject(t.sid, injectType.value, injectRecover.value ? injectRecoverSecs.value : 0)
  }
  const preview = (cmd.split('\n').find((l) => l.startsWith('tc qdisc')) || cmd.split('\n').pop() || '').slice(0, 60)
  injectConfirm.value = { n: targets.length, cmd, preview, targets: targets.map((t) => ({ sid: t.sid, label: t.label })) }
  injectMsg.value = `已把命令填入 ${targets.length} 台主机的终端，选择是否立即执行`
}

function confirmInjectExecute() {
  const c = injectConfirm.value
  if (!c) return
  for (const t of c.targets) emit('command', { sid: t.sid, text: c.cmd, execute: true })
  injectConfirm.value = null
  injectMsg.value = `已向 ${c.n} 台主机下发执行 —— 观察上方实时卡`
}

// --- 命令/脚本就地执行 ------------------------------------------------------
const execCmd = ref('')
const execTimeout = ref(60)
const execBusy = ref(false)
const execResults = ref<Record<string, LabExec>>({})
const execConfirm = ref<{ n: number; preview: string } | null>(null)

function runExecSubmit() {
  const cmd = (execCmd.value ?? '').trim()
  if (!cmd) return
  const n = selTabs.value.length
  if (!n) {
    execResults.value = { err: { sid: '', ok: false, stdout: '', exit: -1, elapsed_ms: 0, error: '还没有勾选已连接的主机' } as LabExec }
    return
  }
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
  const started: Record<string, { label: string }> = {}
  for (const t of targets) started[t.sid] = { label: t.label }
  await Promise.allSettled(
    targets.map(async (t) => {
      try {
        const r = await api.execBatch(t.sid, execCmd.value.trim(), execTimeout.value)
        results[t.sid] = r
      } catch (e) {
        results[t.sid] = { sid: t.sid, ok: false, stdout: '', exit: -1, elapsed_ms: 0, error: String(e) }
      }
    })
  )
  execResults.value = { ...started, ...results } as unknown as Record<string, LabExec>
  execBusy.value = false
}

function execOut(t: LabTab): LabExec | undefined {
  const r = execResults.value[t.sid]
  if (!r || r.sid === '') return undefined
  if (r.ok === undefined) return r // 占位（运行中）
  return r
}

onMounted(() => {
  pollTimer = setInterval(() => void tickPing(), 3000)
  void tickPing()
})
onBeforeUnmount(() => {
  if (pollTimer) clearInterval(pollTimer)
  if (unsetAlarm) clearTimeout(unsetAlarm)
})
</script>

<template>
  <div class="mask" @click.self="emit('close')">
    <div class="lab panel">
      <div class="head">
        <span class="title">故障演练台</span>
        <span class="sub">目标多选 · CPU/内存实时 · 时延/丢包 3s 高采 · 注入/执行全程不跳页</span>
        <span class="spacer"></span>
        <span class="dim">已选 {{ selTabs.length }}/{{ connectedTabs.length }} 台</span>
        <button class="btn ghost" @click="selectAll">全选</button>
        <button class="btn ghost" @click="selectNone">清空</button>
        <button class="btn ghost" @click="emit('close')">关闭</button>
      </div>

      <div class="pick-row">
        <label v-for="t in connectedTabs" :key="t.sid" class="pick" :class="{ on: selected.has(t.sid) }">
          <input type="checkbox" :checked="selected.has(t.sid)" @change="toggle(t.sid)" />
          {{ t.label }}
        </label>
        <span v-if="!connectedTabs.length" class="dim">还没有连接的主机 —— 先双击左侧主机建立会话</span>
      </div>

      <!-- 实时卡 -->
      <div v-if="selTabs.length" class="grid">
        <div
          v-for="t in selTabs"
          :key="t.sid"
          class="lab-card"
          :class="{ alarm: lostAlarm[t.sid] }"
        >
          <div class="c-head">
            <span class="c-name" :title="t.sid">{{ t.label }}</span>
            <span v-if="remainingSec(t.sid)" class="badge hot" :title="injectMarks[t.sid].kind + ' 注入中'">
              注入中 · {{ remainingSec(t.sid) }}s
            </span>
          </div>
          <div class="c-row">
            <span class="c-k">CPU</span>
            <span class="c-v" :class="{ hot: (cpuPct(live(t)) ?? 0) > 85 }">{{ (cpuPct(live(t)) ?? 0).toFixed(1) }}%</span>
          </div>
          <div class="c-row">
            <span class="c-k">内存</span>
            <span class="c-v" :class="{ hot: (memPct(live(t)) ?? 0) > 90 }">{{ (memPct(live(t)) ?? 0).toFixed(1) }}%</span>
          </div>
          <div class="c-row">
            <span class="c-k">时延</span>
            <span class="c-v" :class="{ hot: (pingLast[t.sid]?.lat ?? 0) >= 50 }">
              {{ pingLast[t.sid]?.lat != null ? pingLast[t.sid]!.lat!.toFixed(2) + ' ms' : '—' }}
            </span>
            <span class="c-k">丢包</span>
            <span class="c-v" :class="{ hot: (pingLast[t.sid]?.loss ?? 0) > 0 }">
              {{ pingLast[t.sid]?.loss != null ? pingLast[t.sid]!.loss!.toFixed(1) + '%' : '—' }}
            </span>
          </div>
          <svg
            v-if="pingHist[t.sid]?.lat.length"
            viewBox="0 0 240 34"
            preserveAspectRatio="none"
            width="100%"
            height="34"
            class="lab-spark"
          >
            <line
              v-if="injectMarks[t.sid]"
              :x1="injectMarks[t.sid].atIdx * (240 / Math.max(1, pingHist[t.sid].lat.length - 1))"
              :x2="injectMarks[t.sid].atIdx * (240 / Math.max(1, pingHist[t.sid].lat.length - 1))"
              y1="0"
              y2="34"
              stroke="var(--ctp-yellow)"
              stroke-width="1.4"
              stroke-dasharray="2 2"
            />
            <path :d="sparkPath(pingHist[t.sid]?.lat ?? [], 240, 34)" fill="none" stroke="var(--ctp-sky)" stroke-width="1.4" />
            <path
              :d="sparkPath(pingHist[t.sid]?.loss ?? [], 240, 34)"
              fill="none"
              stroke="var(--ctp-red)"
              stroke-width="1.4"
              stroke-dasharray="1 3"
            />
          </svg>
          <div class="c-foot dim">蓝实=时延 · 红点=丢包 · 黄虚线=注入时刻</div>
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
        </div>
        <div class="inj-row">
          <span class="dim">网卡</span>
          <label class="check"><input type="radio" value="auto" v-model="injectNicMode" /> 自动（各台探测默认路由出口）</label>
          <label class="check"><input type="radio" value="manual" v-model="injectNicMode" /> 手动</label>
          <input v-if="injectNicMode === 'manual'" v-model="injectNic" class="num" style="width: 96px" />
          <label class="check"><input type="checkbox" v-model="injectRecover" /> 到期自动恢复</label>
          <input v-model.number="injectRecoverSecs" class="num" style="width: 60px" :disabled="!injectRecover" />
          <span class="dim">秒（期间该终端被占住，可 Ctrl+C 后用「清除」恢复）</span>
        </div>
        <div class="inj-row">
          <button class="btn" :disabled="!selTabs.length" @click="runInject">生成并填入终端（{{ selTabs.length }} 台）</button>
          <template v-if="injectConfirm">
            <div class="inj-confirm">
              <span class="dim">将执行：<code>{{ injectConfirm.preview }}</code></span>
              <button class="btn danger" @click="confirmInjectExecute">⚠ 立即执行（{{ injectConfirm.n }} 台）</button>
              <button class="btn ghost" style="margin-left: 6px" @click="injectConfirm = null">仅等待，我自己回车</button>
            </div>
          </template>
          <span v-if="injectMsg" class="dim" :class="{ err: injectMsg.startsWith('失败') }">{{ injectMsg }}</span>
        </div>
      </div>

      <!-- 命令 / 脚本就地执行 -->
      <div class="section">
        <div class="sec-title">
          命令 / 脚本就地执行
          <span class="dim">（目标=上方勾选 · 并行 only-read 通道 · 交互式命令请去终端页 · 执行前确认一次）</span>
        </div>
        <textarea
          v-model="execCmd"
          class="exec-input"
          rows="3"
          spellcheck="false"
          placeholder="例如：uptime&#10;也支持多行脚本：&#10;for i in $(seq 1 3); do echo tick $i; sleep 1; done"
        ></textarea>
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
          <div v-for="t in selTabs" :key="t.sid" class="exec-out" v-show="execOut(t)">
            <div class="eo-head">
              <span class="eo-host">{{ t.label }}</span>
              <span v-if="execOut(t)?.ok !== undefined" class="eo-meta" :class="{ ok: execOut(t)?.ok, bad: !execOut(t)?.ok }">
                {{ execOut(t)?.ok ? 'exit ' + execOut(t)?.exit : '失败' }}
                · {{ execOut(t)?.elapsed_ms }}ms
              </span>
            </div>
            <pre v-if="execOut(t)?.stdout" class="eo-out">{{ execOut(t)?.stdout }}</pre>
            <div v-if="execOut(t)?.error" class="eo-err">{{ execOut(t)?.error }}</div>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.lab { width: min(1080px, 94vw); max-height: 92vh; overflow-y: auto; display: flex; flex-direction: column; gap: 10px; }
.head { display: flex; align-items: center; gap: 10px; }
.head .spacer { flex: 1; }
.title { font-weight: 700; font-size: 15px; }
.sub { font-size: 11px; color: var(--ctp-overlay0); }
.pick-row { display: flex; flex-wrap: wrap; gap: 6px; }
.pick { display: inline-flex; align-items: center; gap: 5px; border: 1px solid var(--ctp-surface0); border-radius: 14px; padding: 3px 10px; font-size: 12px; cursor: pointer; }
.pick.on { border-color: var(--ctp-blue); color: var(--ctp-blue); }
.grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(250px, 1fr)); gap: 10px; }
.lab-card { border: 1px solid var(--ctp-surface0); border-radius: 8px; padding: 8px 10px; }
.lab-card.alarm { border-color: var(--ctp-red); box-shadow: 0 0 0 1px var(--ctp-red); }
.c-head { display: flex; align-items: center; gap: 8px; margin-bottom: 4px; }
.c-name { font-weight: 600; font-size: 12.5px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.badge.hot { background: var(--ctp-red); color: var(--on-accent); border-radius: 10px; padding: 1px 8px; font-size: 10.5px; }
.c-row { display: flex; align-items: baseline; gap: 8px; margin-top: 3px; white-space: nowrap; }
.c-k { color: var(--ctp-overlay0); font-size: 11px; min-width: 30px; }
.c-v { font-weight: 600; font-size: 13px; font-variant-numeric: tabular-nums; }
.c-v.hot { color: var(--ctp-red); }
.lab-spark { display: block; margin-top: 4px; background: var(--ctp-mantle); border-radius: 4px; }
.c-foot { font-size: 10px; margin-top: 2px; }
.section { border-top: 1px solid var(--ctp-surface0); padding-top: 8px; }
.sec-title { font-weight: 600; margin-bottom: 6px; }
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