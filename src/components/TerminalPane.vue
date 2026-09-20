<script setup lang="ts">
import { onMounted, onBeforeUnmount, ref, watch } from 'vue'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { api } from '../api'
import { listen } from '@tauri-apps/api/event'
import { terminalTheme, themeVersion } from '../theme'

const props = defineProps<{ sid: string; active: boolean }>()

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

defineExpose({ getSelection, getTail })

function onContext(e: MouseEvent) {
  // 选中了就提供「解释这段」；没选中也能用（AI 看最近的输出）
  emit('context', { x: e.clientX, y: e.clientY, selection: getSelection() })
}

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
  </div>
</template>

<style scoped>
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
