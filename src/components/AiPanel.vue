<script setup lang="ts">
/**
 * AI 侧栏：三条入口（解释这段 / 生成命令 / 对话）共用同一个面板。
 *
 * 生成命令**不自动执行** —— 面板里给一个「插入终端」按钮，
 * 命令落进终端后仍由用户按回车。这是安全底线：模型可能给出 rm 之类的命令。
 */
import { computed, nextTick, ref, watch } from 'vue'
import {
  ai,
  activeProfile,
  aiDisabledReason,
  aiReady,
  ask,
  cancelAi,
  clearTurns,
} from '../ai'

const props = defineProps<{ sid?: string | null }>()
const emit = defineEmits<{
  (e: 'insert', text: string): void
  (e: 'close'): void
}>()

const input = ref('')
/** 面板里的两种模式：聊天，或把需求变成一条命令 */
const mode = ref<'chat' | 'command'>('chat')
const scroller = ref<HTMLElement | null>(null)

const ready = computed(() => aiReady())
const reason = computed(() => aiDisabledReason())
const profile = computed(() => activeProfile())

function send() {
  const text = input.value.trim()
  if (!text || ai.streaming) return
  input.value = ''
  void ask(mode.value, { prompt: text, sid: props.sid ?? undefined })
}

function insertPending() {
  if (!ai.pendingCommand) return
  emit('insert', ai.pendingCommand)
  ai.pendingCommand = ''
}

async function copyPending() {
  try {
    await navigator.clipboard.writeText(ai.pendingCommand)
  } catch {
    /* 剪贴板不可用时用户还能手动选中 */
  }
}

async function scrollToEnd() {
  await nextTick()
  const el = scroller.value
  if (el) el.scrollTop = el.scrollHeight
}

watch(() => ai.turns.length, scrollToEnd)
watch(() => ai.turns[ai.turns.length - 1]?.content, scrollToEnd)

/** 缩进/反引号那类行单独显示成代码样式。 */
function looksLikeCommand(line: string): boolean {
  const t = line.trim()
  if (!t) return false
  return /^[$#>]\s/.test(t) || /^(sudo|systemctl|journalctl|ss|df|du|ps|top|grep|cat|tail|curl|docker|apt|yum|kill|netstat|lsof|find|awk|sed)\b/.test(t)
}
</script>

<template>
  <aside class="ai-drawer">
    <div class="head">
      <span class="title">AI 助手</span>
      <span v-if="profile" class="badge" :title="`${profile.protocol} · ${profile.base_url}`">
        {{ profile.name }} · {{ profile.model }}
      </span>
      <span v-else class="badge off">未配置</span>
      <span class="modes">
        <button class="mode" :class="{ on: mode === 'chat' }" @click="mode = 'chat'">对话</button>
        <button class="mode" :class="{ on: mode === 'command' }" @click="mode = 'command'">生成命令</button>
      </span>
      <span class="spacer"></span>
      <button class="icon" title="清空对话" @click="clearTurns">🗑</button>
      <button class="icon" title="关闭" @click="emit('close')">×</button>
    </div>

    <div v-if="!ready" class="notice">
      {{ reason }}
    </div>
    <div v-else-if="ai.error" class="notice err">
      {{ ai.error }}
    </div>

    <div ref="scroller" class="turns">
      <div v-if="!ai.turns.length" class="empty">
        <p>三种用法：</p>
        <ul>
          <li>在终端里 <b>选中一段报错</b> → 右键「解释这段」</li>
          <li>按 <b>Ctrl+Shift+I</b> 让 AI 看着最近的输出解释</li>
          <li>直接在这里提问，或说「找出占用 80 端口的进程」让它生成命令</li>
        </ul>
        <p class="hint">上下文会带上当前会话最近的终端输出，所以问题可以很短。</p>
      </div>

      <div v-for="(t, i) in ai.turns" :key="i" class="turn" :class="t.role">
        <div class="who">{{ t.role === 'user' ? '你' : (profile?.name ?? 'AI') }}</div>
        <div v-if="t.role === 'assistant' && t.kind === 'command'" class="cmd-box">
          <code>{{ t.content || '…' }}</code>
          <div v-if="ai.pendingCommand && i === ai.turns.length - 1" class="cmd-actions">
            <button class="mini primary" @click="insertPending">插入终端</button>
            <button class="mini" @click="copyPending">复制</button>
            <span class="mini-hint">不会自动执行，回车由你按</span>
          </div>
        </div>
        <div v-else class="body">
          <template v-for="(line, li) in (t.content || '').split('\n')" :key="li">
            <div v-if="looksLikeCommand(line)" class="code-line">{{ line }}</div>
            <div v-else class="text-line">{{ line || '\u00a0' }}</div>
          </template>
          <span v-if="t.streaming" class="caret">▋</span>
        </div>
        <div v-if="t.error" class="turn-err">{{ t.error }}</div>
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
      <button v-if="ai.streaming" class="send stop" @click="cancelAi">停止</button>
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
.title { font-size: 12.5px; font-weight: 600; color: var(--ctp-text); }
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
  border-radius: 4px; font-size: 10px; padding: 1px 6px; cursor: pointer;
}
.mode.on { color: var(--ctp-blue); border-color: var(--ctp-blue); }
.icon {
  background: none;
  border: none;
  color: var(--ctp-overlay0);
  font-size: 14px;
  cursor: pointer;
  padding: 0 3px;
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
.turns { flex: 1; overflow-y: auto; padding: 10px; min-height: 0; }
.empty { color: var(--ctp-overlay0); font-size: 11.5px; line-height: 1.7; }
.empty ul { margin: 4px 0 8px; padding-left: 18px; }
.empty b { color: var(--ctp-subtext0); }
.empty .hint { color: var(--ctp-surface2); }
.turn { margin-bottom: 12px; }
.who { font-size: 10px; color: var(--ctp-overlay0); margin-bottom: 3px; text-transform: uppercase; letter-spacing: 0.4px; }
.turn.user .who { color: var(--ctp-blue); }
.turn.user .body, .turn.user .text-line { color: var(--ctp-text); }
.body { font-size: 12px; color: var(--ctp-subtext1); line-height: 1.6; white-space: pre-wrap; word-break: break-word; }
.turn.user .body { color: var(--ctp-text); }
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
}
.cmd-box {
  background: var(--ctp-crust);
  border: 1px solid var(--ctp-surface1);
  border-radius: 6px;
  padding: 8px;
}
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
.caret { color: var(--ctp-blue); animation: blink 1s steps(2) infinite; }
@keyframes blink { to { opacity: 0; } }
.turn-err { font-size: 11px; color: var(--ctp-red); margin-top: 4px; line-height: 1.5; }
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
  padding: 7px 12px;
  cursor: pointer;
}
.send:disabled { opacity: 0.45; cursor: not-allowed; }
.send.stop { background: var(--ctp-red); }
</style>
