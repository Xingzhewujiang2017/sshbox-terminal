/**
 * 剥离模型写进正文的思考（Qwen3 / DeepSeek-R1 / qwen3.6-27b 经中转 provider）。
 *
 * 与 src-tauri/src/ai.rs 的 strip_thinking 是**同一规则的两份实现**：
 * 后端管报告 AI 结论（非流式），这里管对话/生成命令/解释（流式在收尾时剥）。
 * 改规则必须两边同步 + 两边都跑样例单测 —— 共享样例在
 * src-tauri/tests/fixtures/think_cases.json，前端用 `npm run test:think` 跑。
 *
 * 安全属性：剥离是**搬移不是删除** —— 剥出的内容进「思考过程」折叠栏，
 * 用户展开永远能看到原文。找不到结构就原样返回。
 */

/** 标签字面量用拼接写死：直接写完整尖括号标签，某些工具链会把它当 HTML 吞掉（实测踩过）。 */
const LT = '<'
const GT = '>'
const OPEN_TAGS = [LT + 'think' + GT, LT + 'thinking' + GT]
/** 没有开始标签时，前缀短于这个长度就不当思考剥（见 ai.rs 同名常量）。 */
const MIN_TAGGED_THINK_CHARS = 80

const CLOSE_TAGS = [LT + '/think' + GT, LT + '/thinking' + GT, LT + '｜end▁of▁thinking｜' + GT]

/** 只转 ASCII 大写：JS 的 toLowerCase() 可能改变字符串长度（如 İ），索引就不安全了。 */
function asciiLower(s: string): string {
  return s.replace(/[A-Z]/g, (c) => String.fromCharCode(c.charCodeAt(0) + 32))
}

function pick(tags: string[], lc: string): { at: number; len: number } | null {
  let best: { at: number; len: number } | null = null
  for (const t of tags) {
    const at = lc.indexOf(t)
    if (at < 0) continue
    if (!best || at < best.at || (at === best.at && t.length > best.len)) best = { at, len: t.length }
  }
  return best
}

/**
 * 标签式思考。qwen3.6-27b 经中转时**只有闭合标签**、没有开始标签，
 * 思考段能长达数百字（真实样本如此）→ 这种情形按"正文开头到闭合标签"整段搬走。
 */
function stripTagged(text: string): { answer: string; reasoning?: string } | null {
  const lc = asciiLower(text)
  const close = pick(CLOSE_TAGS, lc)
  if (!close) return null
  const openAt = pick(OPEN_TAGS, lc)
  const open = openAt && openAt.at < close.at ? openAt : null

  // 没有开始标签时，前缀短于阈值就多半是**正文在讨论这个标签本身**，不是思考
  if (!open && text.slice(0, close.at).trim().length < MIN_TAGGED_THINK_CHARS) return null

  const think = open ? text.slice(open.at + open.len, close.at) : text.slice(0, close.at)
  const answer = open
    ? text.slice(0, open.at) + text.slice(close.at + close.len)
    : text.slice(close.at + close.len)

  const trimmed = think.trim()
  if (!trimmed) return null
  return { answer, reasoning: trimmed }
}

const MARKERS = ["here's a thinking process:", '以下是思考过程：', '思考过程：']

function isNumbered(l: string): boolean {
  const t = l.trimStart()
  let i = 0
  while (i < t.length && t[i] >= '0' && t[i] <= '9') i++
  return i > 0 && i < t.length && t[i] === '.'
}

/** 编号式思考。marker 只在前 10 行内认：思考总在开头，避免解释这个功能时误剥正文。 */
function stripNumbered(text: string): { answer: string; reasoning?: string } {
  const lines = text.split('\n')
  const mline = lines.slice(0, 10).findIndex((l) => {
    const t = l.toLowerCase()
    return MARKERS.some((m) => t.includes(m))
  })
  if (mline < 0) return { answer: text }

  let end = -1
  for (let i = mline; i < lines.length; i++) {
    if (isNumbered(lines[i])) end = i
  }
  if (end < 0) return { answer: text }

  let cut = end + 1
  while (cut < lines.length) {
    const t = lines[cut].trimStart()
    if (!t || t.startsWith('-') || t.startsWith('*') || t.startsWith('+')) cut++
    else break
  }
  const reasoning = lines.slice(mline, cut).join('\n')
  const answer = [...lines.slice(0, mline), ...lines.slice(cut)].join('\n')
  return reasoning.trim() ? { answer, reasoning } : { answer: text }
}

export function stripThinking(text: string): { answer: string; reasoning?: string } {
  return stripTagged(text) ?? stripNumbered(text)
}

/** 把剥离出的思考合并进现有的 reasoning 字段（追加在末尾，别覆盖标准 reasoning）。 */
export function mergeReasoning(existing: string | undefined, stripped: string | undefined): string | undefined {
  if (!stripped) return existing
  const label = '（模型把思考写进了正文，已自动剥离）'
  return existing ? `${existing}\n\n${label}\n${stripped}` : `${label}\n${stripped}`
}
