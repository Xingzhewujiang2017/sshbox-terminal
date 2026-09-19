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
const emit = defineEmits<{ (e: 'data', data: string): void }>()

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
  <div class="term-wrap" v-show="active">
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
