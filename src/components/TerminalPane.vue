<script setup lang="ts">
import { onMounted, onBeforeUnmount, ref, watch } from 'vue'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

const props = defineProps<{ sid: string; active: boolean }>()

const termEl = ref<HTMLDivElement>()
let term: Terminal | null = null
let fit: FitAddon | null = null
let unlisten: (() => void) | null = null
let ro: ResizeObserver | null = null

function b64decode(b64: string): Uint8Array {
  const bin = atob(b64)
  const bytes = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i)
  return bytes
}

onMounted(async () => {
  term = new Terminal({
    cursorBlink: true,
    fontSize: 14,
    fontFamily: 'Cascadia Mono, Consolas, "Courier New", monospace',
    theme: {
      background: '#1e1e2e',
      foreground: '#cdd6f4',
      cursor: '#f5e0dc',
      selectionBackground: '#45475a',
    },
    scrollback: 5000,
  })
  fit = new FitAddon()
  term.loadAddon(fit)
  term.open(termEl.value!)
  fit.fit()

  term.onData((data) => {
    invoke('term_write', { sid: props.sid, data })
  })

  // Push resize to PTY (debounced)
  let resizeTimer: number | undefined
  const doResize = () => {
    if (!term || !fit) return
    fit.fit()
    clearTimeout(resizeTimer)
    resizeTimer = window.setTimeout(() => {
      invoke('term_resize', { sid: props.sid, cols: term!.cols, rows: term!.rows })
    }, 100)
  }
  ro = new ResizeObserver(doResize)
  ro.observe(termEl.value!)

  unlisten = await listen<{ sid: string; data: string }>('ssh://data', (e) => {
    if (e.payload.sid === props.sid) {
      term?.write(b64decode(e.payload.data))
    }
  })

  // Sync initial size
  invoke('term_resize', { sid: props.sid, cols: term.cols, rows: term.rows })
})

watch(
  () => props.active,
  (a) => {
    if (a && term && fit) {
      fit.fit()
      term.focus()
      invoke('term_resize', { sid: props.sid, cols: term.cols, rows: term.rows })
    }
  }
)

onBeforeUnmount(() => {
  unlisten?.()
  ro?.disconnect()
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
  background: #1e1e2e;
}
.term {
  height: 100%;
  width: 100%;
  padding: 4px;
  box-sizing: border-box;
}
</style>
