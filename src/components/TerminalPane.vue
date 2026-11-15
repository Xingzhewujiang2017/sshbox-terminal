<script setup lang="ts">
import { nextTick, onMounted, onBeforeUnmount, ref, watch } from 'vue'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { SearchAddon } from '@xterm/addon-search'
import { api } from '../api'
import { listen } from '@tauri-apps/api/event'
// 剪贴板走原生插件：不走 WebView 的 navigator.clipboard（新配置目录下会弹系统权限框，
// 且异步期间会夺走键盘焦点，导致粘贴后按回车无反应）。
import { readText, writeText } from '@tauri-apps/plugin-clipboard-manager'
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
    await writeText(text)
    // 原生插件调用也可能让 WebView 把键盘焦点让给 body；稳妥起见抢回来。
    term?.focus()
    return true
  } catch {
    return false
  }
}

/** 把剪贴板内容粘进终端。**不自动回车** —— 由用户确认后再按。 */
async function pasteClipboard(): Promise<boolean> {
  try {
    const text = await readText()
    if (!text) return false
    // 广播模式：粘贴只作用于当前终端（键盘输入才广播）。提示而不是静默 ——
    // 用户十有八九想贴到多台，这里把事实讲清楚，避免"以为全贴了、回车只动一台"。
    if ((props.broadcastCount ?? 0) > 0) {
      pasteConfirm.value = { text, n: props.broadcastCount ?? 0 }
      return false
    }
    term?.paste(text)
    // 原生调用期间焦点可能被让给 body；贴完按回车没反应，所以抢回来。
    term?.focus()
    return true
  } catch {
    return false
  }
}

const pasteConfirm = ref<{ text: string; n: number } | null>(null)
function confirmPaste() {
  const p = pasteConfirm.value
  pasteConfirm.value = null
  if (p && term) {
    term.paste(p.text)
    term.focus()
  }
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
const ctxMenuEl = ref<HTMLElement>()

// 演练台快捷菜单同样按视口边界回移（少量项，高度小，但靠底时同样会被切）
watch(ctxMenu, async (v) => {
  if (!v) return
  await nextTick()
  const el = ctxMenuEl.value
  if (!el) return
  const r = el.getBoundingClientRect()
  const pad = 8
  let { x, y } = v
  if (x + r.width > window.innerWidth - pad) x = window.innerWidth - r.width - pad
  if (y + r.height > window.innerHeight - pad) y = window.innerHeight - r.height - pad
  if (x !== v.x || y !== v.y) ctxMenu.value = { x, y }
})

/**
 * 右键：主界面直接把坐标 + 当前选中文本交给 App，由 App 弹那一份完整菜单
 * （复制 / 粘贴 / 解释这段 / 用自然语言生成命令 / 全选 / 清屏）。
 *
 * 早先这里是先弹一个三项小菜单、点「解释这段」再让 App 弹完整菜单 ——
 * 两级菜单里第二级又有一个「解释这段」，用户要的复制/清屏藏在第二级里。
 * 演练台的终端没有 AI 入口（noAsk），就地弹两项快捷菜单。
 */
function onContext(e: MouseEvent) {
  if (props.noAsk) {
    ctxMenu.value = { x: e.clientX, y: e.clientY }
    return
  }
  emit('context', { x: e.clientX, y: e.clientY, selection: getSelection() })
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
let search: SearchAddon | null = null

/** 终端内搜索（Ctrl+F）：浮层状态只属于当前这个终端，切标签各记各的。 */
const searchOpen = ref(false)
const searchInput = ref('')
const searchHits = ref({ index: -1, count: 0 })
const searchEl = ref<HTMLInputElement>()

// 不传 `decorations`：xterm 的装饰器属于 **proposed API**，终端没开
// `allowProposedApi` 时 findNext 会直接抛「You must set the allowProposedApi
// option to true」—— 一抛，定位和计数全废（C24 就卡在这）。不传装饰器则只做
// 选中 + 滚动；命中总数我们自己扫缓冲数，代价是不做全量高亮，但不会再抛异常，
// 也不会像 onDidChangeResults 那样恒报 0/0 骗人。

/**
 * 自己数命中数，不用 `onDidChangeResults`。
 *
 * 实测（xterm 6 + addon-search 0.16）：那个事件里的 resultCount 恒为 0 ——
 * 装饰器明明上色了、匹配也选中了，计数栏却显示 `0/0`。与其显示一个假数字，
 * 不如直接扫回滚缓冲自己数：慢一点（万行级 ~ms），但不会骗人。
 */
function countMatches(q: string): number {
  const buf = term?.buffer.active
  if (!buf || !q) return 0
  const needle = q.toLowerCase()
  let n = 0
  for (let i = 0; i < buf.length; i++) {
    const hay = (buf.getLine(i)?.translateToString(true) ?? '').toLowerCase()
    let at = hay.indexOf(needle)
    while (at >= 0) {
      n++
      at = hay.indexOf(needle, at + needle.length)
    }
  }
  return n
}

function runSearch(dir: 1 | -1) {
  if (!search) return
  const q = searchInput.value
  if (!q) {
    search.clearDecorations()
    searchHits.value = { index: -1, count: 0 }
    return
  }
  const opts = { incremental: true }
  if (dir > 0) search.findNext(q, opts)
  else search.findPrevious(q, opts)
  searchHits.value = { index: -1, count: countMatches(q) }
}

function openSearch() {
  searchOpen.value = true
  void nextTick(() => searchEl.value?.select())
}

function closeSearch() {
  searchOpen.value = false
  searchInput.value = ''
  searchHits.value = { index: -1, count: 0 }
  search?.clearDecorations()
  term?.focus()
}

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
  search = new SearchAddon()
  term.loadAddon(search)
  term.open(termEl.value!)
  fit.fit()

  // Ctrl+Shift+C/V 与 Ctrl+C/V 都是终端里的复制/粘贴（Windows Terminal 同款 UX）：
    // - Ctrl+C：有选中文本 → 复制；无选中 → **原样放行**（终端里是中断信号 SIGINT，
    //   抢过来做复制会让正在跑的命令停不下来）
    // - Ctrl+V / Ctrl+Shift+V：粘贴（不自动回车）
    // - 拦截时除了 return false，还必须 preventDefault —— 否则系统仍会把按键
    //   转成原生 paste 事件喂给 xterm 的隐藏 textarea，同一段文本被粘两次
    //   （曾实测 Ctrl+Shift+V 双粘，根因就在这）。
    term.attachCustomKeyEventHandler((e) => {
      if (e.type !== 'keydown') return true
      const ctrlOnly = e.ctrlKey && !e.shiftKey && !e.altKey
      const ctrlShift = e.ctrlKey && e.shiftKey && !e.altKey
      if (ctrlOnly && (e.key === 'C' || e.key === 'c')) {
        if (term?.getSelection()) {
          e.preventDefault()
          void copySelection()
          return false
        }
        return true // 无选中：SIGINT 放行
      }
      if ((ctrlOnly || ctrlShift) && (e.key === 'V' || e.key === 'v')) {
        e.preventDefault()
        void pasteClipboard()
        return false
      }
      if (ctrlShift && (e.key === 'C' || e.key === 'c')) {
        e.preventDefault()
        void copySelection()
        return false
      }
      // Ctrl+F：终端内搜索。xterm 默认会把 Ctrl+F 当普通按键吃进 PTY（远端 shell
      // 收到 ^F 会前移一个字符），所以必须在这里拦下并 preventDefault。
      if (ctrlOnly && (e.key === 'f' || e.key === 'F')) {
        e.preventDefault()
        openSearch()
        return false
      }
      return true
    })

    // 兜底：拦掉到达隐藏 textarea 的原生 paste 事件（来源可能是键盘、右键、菜单），
    // 粘贴统一走 pasteClipboard 一条路径，从根上杜绝双粘。
    termEl.value?.addEventListener('paste', (ev) => {
      ev.preventDefault()
      void pasteClipboard()
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
    <!-- 终端内搜索（Ctrl+F）。Esc 关闭，Enter 下一个，Shift+Enter 上一个 -->
    <div v-if="searchOpen" class="term-search" @click.stop>
      <input
        ref="searchEl"
        v-model="searchInput"
        class="term-search-input"
        placeholder="在终端里搜索…"
        @input="runSearch(1)"
        @keydown.enter.exact.prevent="runSearch(1)"
        @keydown.shift.enter.prevent="runSearch(-1)"
        @keydown.esc.prevent="closeSearch"
      />
      <span class="term-search-hits">{{ searchInput ? (searchHits.count ? `共 ${searchHits.count} 处` : '无命中') : '' }}</span>
      <button title="上一个（Shift+Enter）" @click="runSearch(-1)">↑</button>
      <button title="下一个（Enter）" @click="runSearch(1)">↓</button>
      <button title="关闭（Esc）" @click="closeSearch">✕</button>
    </div>
    <!-- 右键菜单：复制 / 粘贴 / 解释这段（终端里 Ctrl+Shift+C/V 同效） -->
    <div v-if="pasteConfirm" class="ctx-menu paste-confirm" :style="{ left: '40%', top: '40%' }" @click.stop>
      <div class="dim">⚠ 广播模式中（{{ pasteConfirm.n }} 台）—— 粘贴<b>只作用于当前终端</b>，键盘输入才会广播。要继续？</div>
      <div class="paste-actions">
        <button @click="confirmPaste">仅粘到当前终端</button>
        <button @click="pasteConfirm = null">取消</button>
      </div>
    </div>
    <!-- 演练台终端的快捷菜单（noAsk）—— 主界面的右键菜单由 App 统一弹一份 -->
    <div v-if="ctxMenu" ref="ctxMenuEl" class="ctx-menu" :style="{ left: ctxMenu.x + 'px', top: ctxMenu.y + 'px' }" @click.stop>
      <button @click="ctxCopy">📋 复制选中</button>
      <button @click="ctxPaste">📥 粘贴</button>
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
  position: relative;
}
.term-search {
  position: absolute;
  top: 8px;
  right: 18px;
  z-index: 5;
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 4px 6px;
  background: var(--ctp-surface0);
  border: 1px solid var(--ctp-surface1);
  border-radius: 6px;
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.35);
}
.term-search-input {
  width: 180px;
  padding: 2px 6px;
  font-size: 12px;
  color: var(--ctp-text);
  background: var(--ctp-base);
  border: 1px solid var(--ctp-surface1);
  border-radius: 4px;
  outline: none;
}
.term-search-hits {
  min-width: 34px;
  font-size: 11px;
  color: var(--muted);
  text-align: center;
}
.term-search button {
  width: 22px;
  height: 20px;
  font-size: 12px;
  color: var(--ctp-text);
  background: var(--ctp-surface1);
  border: 0;
  border-radius: 4px;
  cursor: pointer;
}
.term {
  height: 100%;
  width: 100%;
  padding: 4px;
  box-sizing: border-box;
}
</style>
