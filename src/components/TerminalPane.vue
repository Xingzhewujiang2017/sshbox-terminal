<script setup lang="ts">
import { onMounted, onBeforeUnmount, ref, watch } from 'vue'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { api } from '../api'
import { listen } from '@tauri-apps/api/event'
import { terminalTheme, themeVersion } from '../theme'

const props = defineProps<{ sid: string; active: boolean; noAsk?: boolean; broadcastCount?: number }>()

/**
 * 键盘输入一律往上抛：广播模式下 App 要把它同时发给多个会话，
 * 组件自己直接写 term_write 就没法做广播了。
 */
const emit = defineEmits<{
  (e: 'data', data: string): void
  /** 右键：带上当前选中文本，App 据此决定要不要弹「解释这段」 */
  (e: 'context', payload: { x: number; y: number; selection: string }): void
}>()

/** 给 AI 用的两个读取口：选中的文本、最近的输出。 */
function getSelection(): string {
  return term?.getSelection() ?? ''
}

/**
 * 终端尾部文本（默认最后 200 行）。
 *
 * 从 xterm 的回滚缓冲里取而不是自己攒一份 —— 自己攒会在切换主题、resize、
 * 清屏之后和真实显示不一致。
 */
function getTail(lines = 200): string {
  const buf = term?.buffer.active
  if (!buf) return ''
  const start = Math.max(0, buf.length - lines)
  const out: string[] = []
  for (let i = start; i < buf.length; i++) {
    const line = buf.getLine(i)
    if (line) out.push(line.translateToString(true))
  }
  // 尾部空行没有信息量，去掉
  while (out.length && !out[out.length - 1].trim()) out.pop()
  return out.join('\n')
}

/** 复制选中内容到系统剪贴板。返回 false = 没有选中或剪贴板不可用。 */
async function copySelection(): Promise<boolean> {
  const text = term?.getSelection() ?? ''
  if (!text) return false
  try {
    await navigator.clipboard.writeText(text)
    return true
  } catch {
    return false
  }
}

/** 把剪贴板内容粘进终端。**不自动回车** —— 由用户确认后再按。 */
async function pasteClipboard(): Promise<boolean> {
  try {
    const text = await navigator.clipboard.readText()
    if (!text) return false
    // 广播模式：粘贴只作用于当前终端（键盘输入才广播）。提示而不是静默 ——
    // 用户十有八九想贴到多台，这里把事实讲清楚，避免"以为全贴了、回车只动一台"。
    if ((props.broadcastCount ?? 0) > 0) {
      pasteConfirm.value = { text, n: props.broadcastCount ?? 0 }
      return false
    }
    term?.paste(text)
    return true
  } catch {
    return false
  }
}

const pasteConfirm = ref<{ text: string; n: number } | null>(null)
function confirmPaste() {
  const p = pasteConfirm.value
  pasteConfirm.value = null
  if (p && term) term.paste(p.text)
}

function selectAllText() {
  term?.selectAll()
}

/** 清屏只清本地回滚缓冲和视图，不去动远端 shell 的状态。 */
function clearScreen() {
  term?.clear()
}

defineExpose({
  getSelection,
  getTail,
  copySelection,
  pasteClipboard,
  selectAllText,
  clearScreen,
})

const ctxMenu = ref<{ x: number; y: number } | null>(null)

function onContext(e: MouseEvent) {
  ctxMenu.value = { x: e.clientX, y: e.clientY }
}

function ctxCopy() {
  ctxMenu.value = null
  void copySelection()
}

function ctxPaste() {
  ctxMenu.value = null
  // 不自动回车 —— 由用户确认后再按，避免误执行剪贴板里的命令
  void pasteClipboard()
}

function ctxAskAI() {
  const pos = ctxMenu.value
  ctxMenu.value = null
  if (pos) emit('context', { x: pos.x, y: pos.y, selection: getSelection() })
}

onMounted(() => {
  document.addEventListener('click', () => (ctxMenu.value = null))
})

const termEl = ref<HTMLDivElement>()
let term: Terminal | null = null
let fit: FitAddon | null = null
let unlisten: (() => void) | null = null
let ro: ResizeObserver | null = null
let resizeTimer: number | undefined
let currentSid = ''

// 主题切换：xterm 的配色是创建时传的，但支持运行时改，所以不用重建终端
// （重建会丢掉回滚缓冲和当前会话状态）。
watch(themeVersion, () => {
  if (term) term.options.theme = terminalTheme()
})

function b64decode(b64: string): Uint8Array {
  const bin = atob(b64)
  const bytes = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i)
  return bytes
}

function pushSize() {
  if (!term) return
  api.termResize(currentSid, term.cols, term.rows).catch(() => {})
}

function doResize() {
  if (!term || !fit) return
  fit.fit()
  clearTimeout(resizeTimer)
  resizeTimer = window.setTimeout(pushSize, 100)
}

onMounted(async () => {
  term = new Terminal({
    cursorBlink: true,
    fontSize: 14,
    fontFamily: 'Cascadia Mono, Consolas, "Courier New", monospace',
    theme: terminalTheme(),
    scrollback: 10000,
  })
  fit = new FitAddon()
  term.loadAddon(fit)
  term.open(termEl.value!)
  fit.fit()

  // Ctrl+Shift+C/V 是终端里的复制/粘贴别名。**Ctrl+C 必须原样放行** ——
  // 在终端里它是中断信号，抢过来做复制会让正在跑的命令停不下来。
  term.attachCustomKeyEventHandler((e) => {
    if (e.type !== 'keydown') return true
    if (e.ctrlKey && e.shiftKey && (e.key === 'C' || e.key === 'c')) {
      void copySelection()
      return false
    }
    if (e.ctrlKey && e.shiftKey && (e.key === 'V' || e.key === 'v')) {
      void pasteClipboard()
      return false
    }
    return true
  })

  term.onData((data) => {
    if (!currentSid) return
    emit('data', data)
  })

  ro = new ResizeObserver(doResize)
  ro.observe(termEl.value!)

  // The listener reads the *current* sid, so a reconnect does not need to
    // re-subscribe and the scrollback above the reconnect marker is preserved.
    unlisten = await listen<{ sid: string; data: string }>('ssh://data', (e) => {
      if (e.payload.sid === currentSid) term?.write(b64decode(e.payload.data))
    })

    currentSid = props.sid
    if (props.sid) {
      // 会话事件流不回溯：welcome+提示符在订阅前早已流过，新挂载的终端看不到。
      // 等订阅就绪后再补一次空回车让 shell 重新打印提示符 —— 必须放在 await listen
      // 之后，否则回车在订阅完成前到达照样丢（反复收起/展开就会复现）。
      api.termWrite(props.sid, String.fromCharCode(13)).catch(() => {})
    }
    pushSize()
  })

watch(
  () => props.sid,
  (sid, old) => {
    if (!sid || sid === old) return
    currentSid = sid
    term?.writeln('')
    term?.writeln('\x1b[33m─── 已重新连接 ───\x1b[0m')
    if (fit) fit.fit()
    pushSize()
  }
)

watch(
  () => props.active,
  (a) => {
    if (a && term && fit) {
      fit.fit()
      term.focus()
      pushSize()
    }
  }
)

onBeforeUnmount(() => {
  unlisten?.()
  ro?.disconnect()
  clearTimeout(resizeTimer)
  term?.dispose()
})
</script>

<template>
  <div class="term-wrap" v-show="active" @contextmenu.prevent="onContext">
    <div ref="termEl" class="term"></div>
    <!-- 右键菜单：复制 / 粘贴 / 解释这段（终端里 Ctrl+Shift+C/V 同效） -->
    <div v-if="pasteConfirm" class="ctx-menu paste-confirm" :style="{ left: '40%', top: '40%' }" @click.stop>
      <div class="dim">⚠ 广播模式中（{{ pasteConfirm.n }} 台）—— 粘贴<b>只作用于当前终端</b>，键盘输入才会广播。要继续？</div>
      <div class="paste-actions">
        <button @click="confirmPaste">仅粘到当前终端</button>
        <button @click="pasteConfirm = null">取消</button>
      </div>
    </div>
    <div v-if="ctxMenu" class="ctx-menu" :style="{ left: ctxMenu.x + 'px', top: ctxMenu.y + 'px' }" @click.stop>
      <button @click="ctxCopy">📋 复制选中</button>
      <button @click="ctxPaste">📥 粘贴</button>
      <button v-if="!noAsk" @click="ctxAskAI">🤖 解释这段（AI）</button>
          </div>
  </div>
</template>

<style scoped>
.ctx-menu {
  position: fixed;
  z-index: 1000;
  display: flex;
  flex-direction: column;
  background: var(--ctp-mantle);
  border: 1px solid var(--ctp-surface0);
  border-radius: 6px;
  padding: 4px;
  box-shadow: 0 6px 18px rgba(0, 0, 0, 0.35);
}
.ctx-menu button {
  background: transparent;
  border: none;
  color: var(--ctp-text);
  font-size: 12px;
  text-align: left;
  padding: 6px 14px;
  border-radius: 4px;
  cursor: pointer;
  white-space: nowrap;
}
.ctx-menu button:hover { background: var(--ctp-surface0); }
.paste-confirm { width: 280px; padding: 10px; gap: 8px; }
.paste-confirm .dim { line-height: 1.5; }
.paste-actions { display: flex; gap: 6px; }
.paste-actions button { flex: 1; justify-content: center; }
.term-wrap {
  height: 100%;
  width: 100%;
  background: var(--ctp-base);
}
.term {
  height: 100%;
  width: 100%;
  padding: 4px;
  box-sizing: border-box;
}
</style>
