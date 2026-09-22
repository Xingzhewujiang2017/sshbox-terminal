<script setup lang="ts">
/**
 * AI 侧栏：三条入口（解释这段 / 生成命令 / 对话）共用同一个面板。
 *
 * 生成命令**不自动执行** —— 面板里给一个「插入终端」按钮，
 * 命令落进终端后仍由用户按回车。这是安全底线：模型可能给出 rm 之类的命令。
 */
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { listen } from '@tauri-apps/api/event'
import { api } from '../api'
import {
  ai,
  activeProfile,
  aiDisabledReason,
  aiReady,
  ask,
  cancelAi,
  cleanCommand,
  clearTurns,
  loadAiSettings,
} from '../ai'

const props = defineProps<{ sid?: string | null; activeLabel?: string }>()
const emit = defineEmits<{
  /** 把命令填进终端；sid = 这条命令是为哪个会话生成的（填回同一台主机） */
  (e: 'insert', payload: { text: string; sid: string }): void
  (e: 'close'): void
}>()

/** 当前槽（跟随活动标签）的对话与流式状态 */
const turns = computed(() => ai.turnsByTab[ai.activeTabId] ?? [])
const streamingNow = computed(() => !!ai.streamingByTab[ai.activeTabId])

const input = ref('')
/** 面板里的两种模式：聊天，或把需求变成一条命令 */
const mode = ref<'chat' | 'command'>('chat')
const scroller = ref<HTMLElement | null>(null)

const ready = computed(() => aiReady())
const reason = computed(() => aiDisabledReason())
const profile = computed(() => activeProfile())
const profiles = computed(() => ai.settings?.profiles ?? [])

/** 本地端点不需要密钥（判断口径与设置页一致）。 */
function needsKey(p: { base_url: string }) {
  return !/127\.0\.0\.1|localhost|\[::1\]/i.test(p.base_url)
}
function keyMissing(p: { base_url: string; has_key: boolean }) {
  return needsKey(p) && !p.has_key
}

/** 面板里直接换模型：写回后端 → 重读设置，AI 入口的可用状态跟着变。 */
async function switchModel(id: string) {
  if (!id) return
  try {
    await api.aiSetActive(id)
    await loadAiSettings()
  } catch (e) {
    ai.error = `切换模型失败：${(e as Error).message}`
  }
}

function send() {
  const text = input.value.trim()
  if (!text || streamingNow.value) return
  input.value = ''
  void ask(mode.value, { prompt: text, sid: props.sid ?? undefined })
}

/** 双击命令框里的命令 → 直接填进它对应的主机（不回车）。
 *
 * **按钮和双击走同一条路**：都以"这一轮自己的内容"为准。
 * 之前按钮用的是全局 pendingCommand 并在点击后清空，一旦 App 那边因为
 * "没有会话"把这次插入丢掉（用户看到的是毫无反应），状态就已经被吃掉了，
 * 按钮此后再也点不动 —— 实测复现过。 */
/** 危险命令模式：AI 生成的命令只“填不执行”（回车由用户按），但高危模式
 *  仍要红字提醒 —— 最后一道防线是用户的眼睛，别让它们松懈。 */
const DANGEROUS: { re: RegExp; hint: string }[] = [
  { re: /\brm\s+-[a-z]*r[a-z]*f\s+\/\s*($|;|&&|\|)/, hint: 'rm -rf / 根目录' },
  { re: /\bmkfs(\s|\.)/, hint: 'mkfs 格式化' },
  { re: /\bdd\b[^\n]*\bof=\/dev\/sd/, hint: 'dd 写块设备' },
  { re: /\b(>|>>|1>)\s*\/dev\/sd/, hint: '重定向写块设备' },
  { re: /:\(\)\s*\{\s*:\|\s*:\&\s*\}\s*;/, hint: 'fork 炸弹' },
  { re: /\bshutdown\b|\breboot\b|\binit\s+0\b|\bpoweroff\b/, hint: '关机/重启' },
  { re: /\bcurl\b[^\n]*\|\s*(ba)?sh\b/, hint: 'curl | sh 管道执行' },
  { re: /\bsudo\s+(rm|mkfs|dd)\b/, hint: 'sudo 高危命令' },
  { re: /\bchmod\s+-R\s+777\s+\//, hint: 'chmod -R 777 /' },
]
const insertWarn = ref<{ text: string; hint: string } | null>(null)

// --- 一次性告知（c）：上下文会发往第三方模型；localStorage 记住，不每轮弹窗 ---
const showNotice = ref(!localStorage.getItem('sshbox_ai_notice'))
function dismissNotice() {
  localStorage.setItem('sshbox_ai_notice', '1')
  showNotice.value = false
}

// --- 成本透明：服务端 usage 事件 → 每轮回复显示 token 数 ---
const lastUsage = ref<{ p: number; c: number; total: number } | null>(null)
let unlistenUsage: (() => void) | null = null
onMounted(() => {
  void listen<{ req_id?: string; prompt_tokens?: number; completion_tokens?: number; total?: number }>('ssh://ai/usage', (e) => {
    if (e.payload && e.payload.total != null) {
      lastUsage.value = { p: e.payload.prompt_tokens ?? 0, c: e.payload.completion_tokens ?? 0, total: e.payload.total }
    }
  }).then((u) => (unlistenUsage = u))
})
onBeforeUnmount(() => unlistenUsage?.())

function insertTurn(t: { content: string; sid?: string; streaming?: boolean }) {
  if (t.streaming) return
  const text = cleanCommand(t.content)
  if (!text.trim()) return
  const hit = DANGEROUS.find((d) => d.re.test(text))
  if (hit) {
    insertWarn.value = { text, hint: hit.hint }
    return
  }
  emit('insert', { text, sid: t.sid ?? '' })
}

function confirmWarnedInsert() {
  const w = insertWarn.value
  insertWarn.value = null
  if (w) emit('insert', { text: w.text, sid: '' })
}

/** 复制这一轮的命令到剪贴板。 */
async function copyTurn(t: { content: string }) {
  try {
    await navigator.clipboard.writeText(cleanCommand(t.content))
  } catch {
    /* 剪贴板不可用时用户还能手动选中 */
  }
}

async function scrollToEnd() {
  await nextTick()
  const el = scroller.value
  if (el) el.scrollTop = el.scrollHeight
}

watch(() => turns.value.length, scrollToEnd)
watch(() => turns.value[turns.value.length - 1]?.content, scrollToEnd)

/** 缩进/反引号那类行单独显示成代码样式。 */
function looksLikeCommand(line: string): boolean {
  const t = line.trim()
  if (!t) return false
  return /^[$#>]\s/.test(t) || /^(sudo|systemctl|journalctl|ss|df|du|ps|top|grep|cat|tail|curl|docker|apt|yum|kill|netstat|lsof|find|awk|sed)\b/.test(t)
}
/** 思考文本的展示清洗：只去掉 markdown 加粗星号，不渲染、不砍内容。 */
function plainThink(s: string): string {
  return s.split('**').join('')
}
</script>

<template>
  <aside class="ai-drawer">
    <div class="head">
      <span class="title">AI 助手{{ props.activeLabel ? ' · ' + props.activeLabel : '' }}</span>
      <span class="spacer"></span>
      <div v-if="showNotice" class="ai-notice">
        提示：提问/生成命令时会自动带上当前终端最近的输出（敏感内容已自动打码），发送到第三方模型 {{ profile?.protocol ?? 'AI 服务商' }}。点「知道了」后不再提示。
        <button class="mini" @click="dismissNotice">知道了</button>
      </div>
      <!-- 直接在下拉里换模型：以前每次换都要开设置 → AI 模型 → 使用，问一句换一次很烦 -->
      <select
        v-if="profiles.length"
        class="picker"
        :value="ai.settings?.active_profile_id ?? ''"
        :title="profile ? `${profile.protocol} · ${profile.base_url}` : '选一个模型'"
        @change="switchModel(($event.target as HTMLSelectElement).value)"
      >
        <option value="" disabled>未选择模型</option>
        <option v-for="p in profiles" :key="p.id" :value="p.id">
          {{ p.name }} · {{ p.model }}{{ keyMissing(p) ? '（缺密钥）' : '' }}
        </option>
      </select>
      <span v-else class="badge off">未配置</span>
      <span class="spacer"></span>
      <span class="modes">
        <button class="mode" :class="{ on: mode === 'chat' }" @click="mode = 'chat'">对话</button>
        <button class="mode" :class="{ on: mode === 'command' }" @click="mode = 'command'">生成命令</button>
      </span>
      <button class="mode" title="清空对话" @click="clearTurns">清空</button>
      <button class="icon" title="关闭" @click="emit('close')">×</button>
    </div>

    <div v-if="!ready" class="notice">
      {{ reason }}
    </div>
    <div v-else-if="ai.error" class="notice err">
      {{ ai.error }}
    </div>

    <div ref="scroller" class="turns">
      <div v-if="!turns.length" class="empty">
        <p>三条入口，随取随用：</p>
        <ul>
          <li><b>右键解释</b>：终端里选中报错/输出 → 右键「解释这段」，AI 只看这一小段</li>
          <li><b>生成命令</b>：右上切「生成命令」→ 一句话描述要做的事 → 双击命令或点「插入终端」填入（<b>不自动执行</b>，回车由你按）</li>
          <li><b>直接对话</b>：就在这里提问</li>
        </ul>
        <p class="hint">
          提问自动带上当前会话最近的终端输出作上下文，所以「刚才的服务为什么没起来」不用补背景。
        </p>
        <p class="hint"><b>Ctrl+Shift+I</b> 或工具栏 AI 按钮 = 打开/收起本面板。</p>
      </div>

      <div v-for="(t, i) in turns" :key="i" class="turn" :class="t.role">
        <div class="who">{{ t.role === 'user' ? '你' : (profile?.name ?? 'AI') }}</div>
        <div
          v-if="t.role === 'assistant' && t.kind === 'command'"
          class="cmd-box"
          :title="t.streaming ? '正在生成…' : '双击命令直接填入终端（不会自动执行）'"
          @dblclick="insertTurn(t)"
        >
          <code>{{ t.content || '…' }}</code>
          <div class="cmd-actions">
            <button class="mini primary" @click="insertTurn(t)">插入终端</button>
            <button class="mini" @click="copyTurn(t)">复制</button>
            <span class="mini-hint">双击命令也能直接填入；不会自动执行，回车由你按</span>
          </div>
        </div>
        <div v-else class="body">
          <!-- 思考过程：默认折叠（模型可能吐几千字），标题给字数，展开才看 -->
          <details v-if="t.reasoning" class="think">
            <summary>
              <span class="tk">思考过程</span> {{ t.reasoning.length }} 字<span v-if="t.streaming" class="thinking">正在思考…</span>
            </summary>
            <pre>{{ plainThink(t.reasoning) }}</pre>
          </details>
          <template v-for="(line, li) in (t.content || '').split('\n')" :key="li">
            <div v-if="looksLikeCommand(line)" class="code-line">
              <span class="cl-text">{{ line }}</span>
              <button class="cl-insert" title="把这行填入终端（回车由你按）" @click="insertTurn({ content: line, sid: t.sid })">插入</button>
            </div>
            <div v-else class="text-line">{{ line || '\u00a0' }}</div>
          </template>
          <span v-if="t.streaming" class="caret">▋</span>
        </div>
        <div v-if="t.error" class="turn-err">{{ t.error }}</div>
      </div>
    </div>

    <div v-if="lastUsage" class="tok">↑{{ lastUsage.p }} ↓{{ lastUsage.c }} · {{ lastUsage.total }} tok（本模型 {{ profile?.model ?? '' }}）</div>
    <div v-if="insertWarn" class="warn-float">
      <div class="err">⚠ 危险命令（{{ insertWarn.hint }}）：不会自动执行，确认要插入终端？</div>
      <code>{{ insertWarn.text }}</code>
      <div class="warn-actions">
        <button class="mini primary" @click="confirmWarnedInsert">确认插入</button>
        <button class="mini" @click="insertWarn = null">取消</button>
      </div>
    </div>

    <div class="foot">
      <textarea
        v-model="input"
        rows="2"
        :placeholder="
          !ready
            ? '先在设置里配置模型'
            : mode === 'command'
              ? '描述你要做的事，例如：找出占用 80 端口的进程'
              : '问点什么（Enter 发送，Shift+Enter 换行）'
        "
        :disabled="!ready"
        @keydown.enter.exact.prevent="send"
      ></textarea>
      <button v-if="streamingNow" class="send stop" @click="cancelAi">停止</button>
      <button v-else class="send" :disabled="!ready || !input.trim()" @click="send">发送</button>
    </div>
  </aside>
</template>

<style scoped>
.ai-drawer {
  width: 380px;
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
  background: var(--ctp-mantle);
  border-left: 1px solid var(--ctp-surface0);
  height: 100%;
  min-height: 0;
}
.head {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 8px 10px;
  border-bottom: 1px solid var(--ctp-surface0);
}
.picker {
  background: var(--ctp-crust); color: var(--ctp-text); border: 1px solid var(--ctp-surface1);
  border-radius: 4px; font-size: 11px; padding: 2px 4px; max-width: 150px; min-width: 0;
  flex-shrink: 1; cursor: pointer;
}
.picker:hover { border-color: var(--ctp-blue); }
.title { font-size: 12.5px; font-weight: 600; color: var(--ctp-text); white-space: nowrap; flex-shrink: 0; }
.badge {
  font-size: 10px;
  color: var(--ctp-blue);
  border: 1px solid var(--ctp-surface1);
  border-radius: 4px;
  padding: 1px 5px;
  max-width: 170px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.badge.off { color: var(--ctp-overlay0); }
.spacer { flex: 1; }
.modes { display: flex; gap: 2px; margin-left: 4px; }
.mode {
  background: none; border: 1px solid var(--ctp-surface0); color: var(--ctp-overlay0);
  border-radius: 4px; font-size: 10px; padding: 2px 7px; cursor: pointer;
  white-space: nowrap; flex-shrink: 0;
}
.mode.on { color: var(--ctp-blue); border-color: var(--ctp-blue); background: var(--ctp-surface0); }
.icon {
  background: none;
  border: none;
  color: var(--ctp-overlay0);
  font-size: 14px;
  cursor: pointer;
  padding: 0 3px;
  flex-shrink: 0;
}
.icon:hover { color: var(--ctp-text); }
.notice {
  font-size: 11px;
  color: var(--ctp-yellow);
  background: var(--banner-warn-bg);
  padding: 6px 10px;
  line-height: 1.5;
}
.notice.err { color: var(--ctp-red); }
.turns { flex: 1; overflow-y: auto; padding: 12px; min-height: 0; scrollbar-width: thin; scrollbar-color: var(--ctp-surface1) transparent; }
.turns::-webkit-scrollbar { width: 8px; }
.turns::-webkit-scrollbar-thumb { background: var(--ctp-surface1); border-radius: 4px; }
.turns::-webkit-scrollbar-thumb:hover { background: var(--ctp-surface2); }
.empty { color: var(--ctp-overlay0); font-size: 11.5px; line-height: 1.7; }
.empty ul { margin: 4px 0 8px; padding-left: 18px; }
.empty b { color: var(--ctp-subtext0); }
.empty .hint { color: var(--ctp-surface2); }
.turn { margin-bottom: 14px; animation: turn-in 0.15s ease; }
@keyframes turn-in { from { opacity: 0; transform: translateY(3px); } }
.who { font-size: 10px; color: var(--ctp-overlay0); margin-bottom: 4px; letter-spacing: 0.4px; }
.turn.user .who { text-align: right; color: var(--ctp-blue); }
/* 用户回合：右侧气泡，一眼能区分"我发的"和"AI 回的" */
.turn.user .body {
  background: var(--ctp-surface0);
  border-radius: 9px;
  border-top-right-radius: 3px;
  padding: 7px 11px;
  max-width: 92%;
  margin-left: auto;
  color: var(--ctp-text);
}
/* 助手回合：名称绿色 + 正文卡片托底（与用户气泡对称，长回复不裸奔） */
.turn.assistant .who { color: var(--ctp-green); }
.turn.assistant .body {
  background: var(--ctp-base);
  border: 1px solid var(--ctp-surface1);
  border-radius: 9px;
  border-top-left-radius: 3px;
  padding: 8px 11px;
}
.body { font-size: 12px; color: var(--ctp-subtext1); line-height: 1.65; white-space: pre-wrap; word-break: break-word; }
.text-line { white-space: pre-wrap; }
.code-line {
  font-family: ui-monospace, monospace;
  font-size: 11.5px;
  background: var(--ctp-crust);
  color: var(--ctp-green);
  border-radius: 4px;
  padding: 2px 6px;
  margin: 2px 0;
  white-space: pre-wrap;
  word-break: break-all;
  display: flex;
  align-items: center;
  gap: 6px;
}
.code-line .cl-text { flex: 1; min-width: 0; }
.code-line .cl-insert {
  flex-shrink: 0;
  background: var(--ctp-surface1);
  color: var(--ctp-text);
  border: none;
  border-radius: 4px;
  font-size: 10px;
  padding: 2px 7px;
  cursor: pointer;
  white-space: nowrap;
}
.code-line .cl-insert:hover { background: var(--ctp-blue); color: var(--on-accent); }
.cmd-box {
  background: var(--ctp-crust);
  border: 1px solid var(--ctp-surface1);
  border-radius: 8px;
  padding: 10px 12px;
  cursor: pointer;
}
.cmd-box:hover { border-color: var(--ctp-green); box-shadow: 0 0 0 1px var(--ctp-green); }
.cmd-box code {
  display: block;
  font-family: ui-monospace, monospace;
  font-size: 12px;
  color: var(--ctp-green);
  white-space: pre-wrap;
  word-break: break-all;
}
.cmd-actions { display: flex; align-items: center; gap: 6px; margin-top: 7px; }
.mini {
  background: var(--ctp-base);
  border: 1px solid var(--ctp-surface1);
  color: var(--ctp-subtext0);
  border-radius: 4px;
  font-size: 11px;
  padding: 2px 8px;
  cursor: pointer;
}
.mini.primary { background: var(--ctp-blue); color: var(--on-accent); border: none; font-weight: 600; }
.mini-hint { font-size: 10px; color: var(--ctp-overlay0); }
.think {
  margin: 0 0 8px;
  background: var(--ctp-crust);
  border-left: 3px solid var(--ctp-surface2);
  border-radius: 0 6px 6px 0;
  padding: 6px 10px;
}
.think summary {
  font-size: 10.5px; color: var(--ctp-overlay0); cursor: pointer; user-select: none; list-style: none;
}
.think summary:hover { color: var(--ctp-subtext0); }
.think summary::-webkit-details-marker { display: none; }
.think summary::before { content: '▸ '; }
.think[open] summary::before { content: '▾ '; }
.tk { color: var(--ctp-subtext0); font-weight: 600; }
.think pre {
  margin: 8px 0 0; padding-top: 8px; border-top: 1px dashed var(--ctp-surface0);
  white-space: pre-wrap; word-break: break-word;
  font-family: ui-monospace, monospace; font-size: 10.5px;
  color: var(--ctp-overlay0); line-height: 1.6;
}
.thinking { color: var(--ctp-blue); margin-left: 6px; }
.caret { color: var(--ctp-blue); animation: blink 1s steps(2) infinite; }
@keyframes blink { to { opacity: 0; } }
.turn-err { font-size: 11px; color: var(--ctp-red); margin-top: 4px; line-height: 1.5; }
.warn-float {
  margin: 0 10px 8px;
  padding: 8px 10px;
  border: 1px solid var(--ctp-red);
  border-radius: 6px;
  background: var(--ctp-mantle);
}
.ai-notice {
  font-size: 11px;
  color: var(--ctp-yellow);
  display: flex;
  align-items: center;
  gap: 8px;
  margin-left: 8px;
}
.tok { font-size: 10px; color: var(--ctp-overlay0); text-align: right; padding: 0 10px 4px; }
.warn-float code {
  display: block;
  white-space: pre-wrap;
  word-break: break-all;
  font-size: 11px;
  margin: 6px 0;
  color: var(--ctp-text);
}
.warn-actions { display: flex; gap: 6px; }
.foot { display: flex; gap: 6px; padding: 8px 10px; border-top: 1px solid var(--ctp-surface0); }
.foot textarea {
  flex: 1;
  background: var(--ctp-crust);
  border: 1px solid var(--ctp-surface0);
  border-radius: 6px;
  color: var(--ctp-text);
  font-family: inherit;
  font-size: 12px;
  padding: 6px 8px;
  resize: none;
}
.foot textarea:focus { outline: 1px solid var(--ctp-blue); }
.send {
  align-self: flex-end;
  background: var(--ctp-blue);
  color: var(--on-accent);
  border: none;
  border-radius: 6px;
  font-size: 12px;
  font-weight: 600;
  padding: 7px 14px;
  min-width: 52px;
  cursor: pointer;
}
.send:disabled { opacity: 0.45; cursor: not-allowed; }
.send.stop { background: var(--ctp-red); }
</style>
