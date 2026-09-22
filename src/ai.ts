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
import { mergeReasoning, stripThinking } from './think'

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
  /** 每个终端标签（tab.id）一份对话；' ' = 无标签/全局兜底槽 */
  turnsByTab: {} as Record<string, AiTurn[]>,
  /** per-slot 流式标志：一槽在生成不挡另一槽提问 */
  streamingByTab: {} as Record<string, boolean>,
  /** 当前 AI 面板服务的标签 id（切换页签时由 App 联动） */
  activeTabId: '',
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

/** 当前槽 key：activeTabId（无标签时回退全局槽） */
export function activeSlot(): string {
  return ai.activeTabId
}

/** 当前槽的对话数组（惰性建槽） */
export function slotTurns(): AiTurn[] {
  const k = activeSlot()
  return (ai.turnsByTab[k] ??= [])
}

/** 切槽：面板显示跟随页签但不自动开/关面板 */
export function setActiveSlot(tabId: string) {
  if (tabId !== ai.activeTabId) ai.activeTabId = tabId
}

/** 清空当前槽对话 */
export function clearTurnSlot() {
  slotTurns().length = 0
}

/** 槽数量上限（LRU）：关标签保留对话，但开过的主机无限积累会常驻内存。
 *  超过上限时回收最久没被激活的槽（每个槽的记录 at 取最新一轮时间）。 */
const SLOT_LIMIT = 30
export function enforceSlotLimit() {
  const keys = Object.keys(ai.turnsByTab)
  if (keys.length <= SLOT_LIMIT) return
  const byLastUse = keys
    .filter((k) => k !== activeSlot() && ai.turnsByTab[k].length > 0)
    .map((k) => ({ k, last: ai.turnsByTab[k][ai.turnsByTab[k].length - 1]?.at ?? 0 }))
    .sort((a, b) => a.last - b.last)
  const overflow = keys.length - SLOT_LIMIT
  for (let i = 0; i < Math.min(overflow, byLastUse.length); i++) {
    delete ai.turnsByTab[byLastUse[i].k]
    delete ai.streamingByTab[byLastUse[i].k]
  }
}

/** 请求 id → 槽 key：流式回复必须写回发起时的槽，防中途切标签写错 */
const reqSlot = new Map<string, string>()
function trackReq(reqId: string) {
  reqSlot.set(reqId, activeSlot())
}
function slotOfReq(reqId: string): string {
  return reqSlot.get(reqId) ?? activeSlot()
}
function finishReq(reqId: string) {
  reqSlot.delete(reqId)
}

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
    const slot = slotOfReq(e.payload.req_id)
    const arr = ai.turnsByTab[slot] ?? []
    const t = arr[arr.length - 1]
    if (!t || t.role !== 'assistant' || !t.streaming) return
    t.content += e.payload.text
  })
  // 思考过程单独一条流：面板默认折叠，展开才看
  await listen<{ req_id: string; text: string }>('ssh://ai/reasoning', (e) => {
    const slot = slotOfReq(e.payload.req_id)
    const arr = ai.turnsByTab[slot] ?? []
    const t = arr[arr.length - 1]
    if (!t || t.role !== 'assistant' || !t.streaming) return
    t.reasoning = (t.reasoning ?? '') + e.payload.text
  })
  await listen<{ req_id: string }>('ssh://ai/done', (e) => {
    const slot = slotOfReq(e.payload.req_id)
    const arr = ai.turnsByTab[slot] ?? []
    const t = arr[arr.length - 1]
    if (t && t.role === 'assistant') {
      t.streaming = false
      finalizeTurn(t)
    }
    ai.streamingByTab[slot] = false
    finishReq(e.payload.req_id)
  })
  await listen<{ req_id: string; message: string }>('ssh://ai/error', (e) => {
    const slot = slotOfReq(e.payload.req_id)
    const arr = ai.turnsByTab[slot] ?? []
    const t = arr[arr.length - 1]
    if (t && t.role === 'assistant') {
      t.streaming = false
      t.error = e.payload.message
      if (!t.content) t.content = ''
    }
    ai.streamingByTab[slot] = false
    ai.error = e.payload.message
    finishReq(e.payload.req_id)
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
  const slot = activeSlot()
  if (ai.streamingByTab[slot]) return
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

  const turns = slotTurns()
  turns.push({ role: 'user', content: label, kind, sid: opts.sid, at: Date.now() })
  turns.push({ role: 'assistant', content: '', streaming: true, kind, sid: opts.sid, at: Date.now() })
  ai.streamingByTab[slot] = true

  const history: ChatMessage[] = turns
    .slice(0, -2)
    .filter((t) => !t.error && t.content.trim())
    .map((t) => ({ role: t.role, content: t.content }))

  const reqId = newReqId()
  ai.reqId = reqId
  trackReq(reqId)
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
    const t = turns[turns.length - 1]
    if (t && t.role === 'assistant') {
      t.streaming = false
      if (!t.content) t.content = full
      finalizeTurn(t)
    }
    if (kind === 'command') {
      ai.pendingCommand = cleanCommand(full)
      ai.pendingSid = opts.sid ?? ''
    }
  } catch (e) {
    const t = turns[turns.length - 1]
    const msg = (e as Error).message
    if (t && t.role === 'assistant') {
      t.streaming = false
      t.error = msg
    }
    ai.error = msg
  } finally {
    ai.streamingByTab[slot] = false
    finishReq(reqId)
    enforceSlotLimit()
  }
}

/** 停止当前槽正在生成的请求（原 streaming 全局语义改为按槽）。 */
export function stopAi() {
  const slot = activeSlot()
  if (!ai.streamingByTab[slot]) return
  const arr = ai.turnsByTab[slot] ?? []
  const t = arr[arr.length - 1]
  if (t && t.role === 'assistant') {
    t.streaming = false
    if (!t.content) t.content = '（已停止）'
  }
  ai.streamingByTab[slot] = false
  if (ai.reqId) void api.aiCancel(ai.reqId).catch(() => {})
  finishReq(ai.reqId)
}

/** 收尾时剥离"写进正文的思考"（Qwen3 风格自建模型把思考混在 content 里）。
 *  剥出的思考合进 reasoning 字段 → 面板已有「思考过程」折叠栏承接；正文只留答案。 */
function finalizeTurn(t: { content: string; reasoning?: string }): void {
  const { answer, reasoning } = stripThinking(t.content)
  if (reasoning) {
    t.content = answer
    t.reasoning = mergeReasoning(t.reasoning, reasoning)
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
  const slot = activeSlot()
  if (!ai.streamingByTab[slot]) return
  // 先告诉后端别再读了，再把前端状态收回来（后端会保留已收到的部分）
  if (ai.reqId) void api.aiCancel(ai.reqId).catch(() => {})
  ai.streamingByTab[slot] = false
  const arr = ai.turnsByTab[slot] ?? []
  const t = arr[arr.length - 1]
  if (t && t.role === 'assistant') {
    t.streaming = false
    if (!t.content) t.content = '（已停止）'
  }
  finishReq(ai.reqId)
}

/** 清空当前标签的 AI 对话（原全局 clearTurns 语义改为按槽） */
export function clearTurns() {
  clearTurnSlot()
  ai.error = ''
  ai.pendingCommand = ''
}
