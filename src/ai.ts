/**
 * AI 面板的共享状态与请求逻辑（和 `transfers.ts` 同一套路：模块级状态，
 * 组件只负责画，谁都能调用）。
 *
 * 三条入口共用一条流式通道：
 *   - explain  解释终端里选中的这段 / 最近的报错
 *   - command  自然语言 → 一条命令（**只填入终端，不自动执行**）
 *   - chat     侧栏多轮对话
 */
import { reactive } from 'vue'
import { listen } from '@tauri-apps/api/event'
import { api, type AiProfile, type AiSettings, type ChatMessage } from './api'

export type AiKind = 'explain' | 'command' | 'chat'

export interface AiTurn {
  role: 'user' | 'assistant'
  content: string
  /** 正在流式接收 */
  streaming?: boolean
  /** 这一轮出错（错误也留在对话里，方便回看） */
  error?: string
  kind?: AiKind
  /** 这一轮是为哪个会话生成的 —— 命令要填回同一台主机，别贴错机器 */
  sid?: string
  /** 思考过程（deepseek 系模型会先吐一大段）。面板默认折叠，展开才看。 */
  reasoning?: string
  at: number
}

export const ai = reactive({
  open: false,
  kind: 'chat' as AiKind,
  turns: [] as AiTurn[],
  streaming: false,
  settings: null as AiSettings | null,
  /** 全局错误（比如"还没选模型"），显示在面板顶部 */
  error: '',
  /** 生成好、等用户确认插入终端的命令 */
  pendingCommand: '',
  /** 生成命令时对应的会话 id */
  pendingSid: '',
  /** 当前请求 id（停止时要按它取消） */
  reqId: '',
})

/** 终端尾部文本由 App.vue 注入（面板不直接持有 xterm）。 */
let tailProvider: () => string = () => ''
export function setTailProvider(fn: () => string) {
  tailProvider = fn
}

/** 当前生效的 profile。 */
export function activeProfile(): AiProfile | null {
  const s = ai.settings
  if (!s) return null
  const id = s.active_profile_id?.trim()
  if (!id) return null
  return s.profiles.find((p) => p.id === id) ?? null
}

/**
 * AI 是否可用。**没配好就明确禁用**，而不是让用户点了才报错。
 * 本地 Ollama 这类不需要密钥的端点，has_key 为 false 也算可用。
 */
export function aiReady(): boolean {
  const p = activeProfile()
  if (!p) return false
  if (needsKey(p) && !p.has_key) return false
  return true
}

/** 只有本地/回环地址才允许没有密钥（云端不带 key 一定是配置没填完）。 */
export function needsKey(p: AiProfile): boolean {
  return !/^https?:\/\/(127\.0\.0\.1|localhost|\[::1\])/i.test(p.base_url.trim())
}

export function aiDisabledReason(): string {
  const p = activeProfile()
  if (!p) return '还没选择模型 —— 去 设置 → AI 模型 里挑一个'
  if (needsKey(p) && !p.has_key) return `「${p.name}」还没填 API 密钥 —— 去 设置 → AI 模型 里补上`
  return ''
}

export async function loadAiSettings() {
  try {
    ai.settings = await api.aiSettings()
  } catch (e) {
    ai.error = `读取 AI 配置失败：${(e as Error).message}`
  }
}

/** 事件通道只订阅一次，App 启动时调用。 */
let inited = false
export async function initAi() {
  if (inited) return
  inited = true
  await listen<{ req_id: string; text: string }>('ssh://ai/delta', (e) => {
    const t = ai.turns[ai.turns.length - 1]
    if (!t || t.role !== 'assistant' || !t.streaming) return
    t.content += e.payload.text
  })
  // 思考过程单独一条流：面板默认折叠，展开才看
  await listen<{ req_id: string; text: string }>('ssh://ai/reasoning', (e) => {
    const t = ai.turns[ai.turns.length - 1]
    if (!t || t.role !== 'assistant' || !t.streaming) return
    t.reasoning = (t.reasoning ?? '') + e.payload.text
  })
  await listen<{ req_id: string }>('ssh://ai/done', () => {
    const t = ai.turns[ai.turns.length - 1]
    if (t && t.role === 'assistant') t.streaming = false
    ai.streaming = false
  })
  await listen<{ req_id: string; message: string }>('ssh://ai/error', (e) => {
    const t = ai.turns[ai.turns.length - 1]
    if (t && t.role === 'assistant') {
      t.streaming = false
      t.error = e.payload.message
      if (!t.content) t.content = ''
    }
    ai.streaming = false
    ai.error = e.payload.message
  })
}

function newReqId(): string {
  return `ai-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`
}

/** 发一轮请求。kind 决定后端怎么组装上下文。 */
export async function ask(
  kind: AiKind,
  opts: { selection?: string; prompt?: string; sid?: string } = {},
): Promise<void> {
  if (ai.streaming) return
  if (!aiReady()) {
    ai.error = aiDisabledReason()
    return
  }
  ai.error = ''
  ai.kind = kind
  if (kind === 'command') ai.pendingCommand = ''

  const label =
    kind === 'explain'
      ? '解释这段' + (opts.selection?.trim() ? `：${firstLine(opts.selection)}` : '（最近的输出）')
      : kind === 'command'
        ? `生成命令：${opts.prompt ?? ''}`
        : (opts.prompt ?? '')

  ai.turns.push({ role: 'user', content: label, kind, sid: opts.sid, at: Date.now() })
  ai.turns.push({ role: 'assistant', content: '', streaming: true, kind, sid: opts.sid, at: Date.now() })
  ai.streaming = true

  const history: ChatMessage[] = ai.turns
    .slice(0, -2)
    .filter((t) => !t.error && t.content.trim())
    .map((t) => ({ role: t.role, content: t.content }))

  const reqId = newReqId()
  ai.reqId = reqId
  try {
    const full = await api.aiAsk({
      reqId,
      kind,
      sid: opts.sid ?? null,
      selection: opts.selection ?? null,
      ask: opts.prompt ?? null,
      tail: tailProvider(),
      history: kind === 'chat' ? history : null,
    })
    const t = ai.turns[ai.turns.length - 1]
    if (t && t.role === 'assistant') {
      t.streaming = false
      if (!t.content) t.content = full
    }
    if (kind === 'command') {
      ai.pendingCommand = cleanCommand(full)
      ai.pendingSid = opts.sid ?? ''
    }
  } catch (e) {
    const t = ai.turns[ai.turns.length - 1]
    const msg = (e as Error).message
    if (t && t.role === 'assistant') {
      t.streaming = false
      t.error = msg
    }
    ai.error = msg
  } finally {
    ai.streaming = false
  }
}

/** 模型偶尔会带 ``` 或 `$ ` 前缀，插入终端前清掉。 */
export function cleanCommand(s: string): string {
  return s
    .trim()
    .replace(/^```[a-z]*\n?/i, '')
    .replace(/```$/, '')
    .replace(/^\$\s+/, '')
    .trim()
}

export function firstLine(s: string, max = 60): string {
  const line = (s || '').trim().split('\n')[0] ?? ''
  return line.length > max ? line.slice(0, max) + '…' : line
}

export async function cancelAi() {
  if (!ai.streaming) return
  // 先告诉后端别再读了，再把前端状态收回来（后端会保留已收到的部分）
  if (ai.reqId) void api.aiCancel(ai.reqId).catch(() => {})
  ai.streaming = false
  const t = ai.turns[ai.turns.length - 1]
  if (t && t.role === 'assistant') {
    t.streaming = false
    if (!t.content) t.content = '（已停止）'
  }
}

export function clearTurns() {
  ai.turns = []
  ai.error = ''
  ai.pendingCommand = ''
}
