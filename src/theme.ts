/* 主题切换：CSS 变量是唯一真源，xterm 和 ECharts 也从变量里读色，
   所以浅色/深色只有 assets/theme.css 一处定义。

   三态：dark / light / system（跟随系统）。 */
import { ref } from 'vue'

export type ThemeMode = 'dark' | 'light' | 'system'
export type ResolvedTheme = 'dark' | 'light'

const KEY = 'sshbox-theme'

/** 主题版本号：非 CSS 的消费者（ECharts 画布、xterm）用它做依赖，
 *  主题一变就重新上色。CSS 侧靠 data-theme 自动生效，不需要这个。 */
export const themeVersion = ref(0)

function mq(): MediaQueryList | null {
  return window.matchMedia?.('(prefers-color-scheme: light)') ?? null
}

export function systemTheme(): ResolvedTheme {
  return mq()?.matches ? 'light' : 'dark'
}

export function resolveTheme(mode: ThemeMode): ResolvedTheme {
  return mode === 'system' ? systemTheme() : mode
}

/** 应用主题。写 data-theme 让 CSS 生效，同时缓存到 localStorage ——
 *  settings.json 是异步 IPC 读的，首帧前拿不到，缓存副本用来避免
 *  "先闪一下深色再变白"。 */
export function applyTheme(mode: ThemeMode): ResolvedTheme {
  const resolved = resolveTheme(mode)
  document.documentElement.dataset.theme = resolved
  try {
    localStorage.setItem(KEY, resolved)
  } catch {
    /* 隐私模式等场景下写不了，无所谓 */
  }
  themeVersion.value++
  return resolved
}

/** 首帧前调用（main.ts）：先按上次的解析结果上色。 */
export function primeTheme(): ResolvedTheme {
  let cached: ResolvedTheme = 'dark'
  try {
    cached = (localStorage.getItem(KEY) as ResolvedTheme) || 'dark'
  } catch {
    /* 读不到就用默认深色 */
  }
  document.documentElement.dataset.theme = cached
  return cached
}

/** 跟随系统时，系统配色变了要跟着变。返回取消订阅函数。 */
export function watchSystem(onChange: (t: ResolvedTheme) => void): () => void {
  const m = mq()
  if (!m) return () => {}
  const handler = () => onChange(m.matches ? 'light' : 'dark')
  m.addEventListener('change', handler)
  return () => m.removeEventListener('change', handler)
}

export function cssVar(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim()
}

/** xterm 的主题对象。xterm 支持运行时改 term.options.theme，不必重建终端。 */
export function terminalTheme() {
  return {
    background: cssVar('--ctp-base'),
    foreground: cssVar('--ctp-text'),
    cursor: cssVar('--ctp-rosewater'),
    selectionBackground: cssVar('--ctp-surface1'),
  }
}

/** ECharts 画布用的颜色。画布是像素，拿不到 CSS 变量，只能运行时读进来传给它。 */
export function chartPalette() {
  return {
    axis: cssVar('--ctp-overlay0'),
    split: cssVar('--ctp-surface0'),
    text: cssVar('--ctp-text'),
    blue: cssVar('--ctp-blue'),
    green: cssVar('--ctp-green'),
    yellow: cssVar('--ctp-yellow'),
    red: cssVar('--ctp-red'),
    peach: cssVar('--ctp-peach'),
    mauve: cssVar('--ctp-mauve'),
    surface1: cssVar('--ctp-surface1'),
  }
}
