/**
 * 剥离自建模型写进正文的思考（Qwen3 / DeepSeek-R1 风格）。
 *
 * 与 src-tauri/src/ai.rs 的 strip_thinking 是**同一规则的两份实现**：
 * 后端管报告 AI 结论（非流式），这里管对话/生成命令/解释（流式在收尾时剥）。
 * 改规则必须两边同步 + 两边都跑样例单测。
 *
 * 安全属性：剥离是**搬移不是删除** —— 剥出的内容进「思考过程」折叠栏，
 * 用户展开永远能看到原文。找不到编号结构（不是编号式思考）就原样返回。
 */
const MARKERS = ["here's a thinking process:", "以下是思考过程：", "思考过程："]

function isNumbered(l: string): boolean {
  const t = l.trimStart()
  let i = 0
  while (i < t.length && t[i] >= '0' && t[i] <= '9') i++
  return i > 0 && i < t.length && t[i] === '.'
}

export function stripThinking(text: string): { answer: string; reasoning?: string } {
  const lines = text.split('\n')
  const mline = lines.findIndex((l) => {
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

/** 把剥离出的思考合并进现有的 reasoning 字段（追加在末尾，别覆盖标准 reasoning）。 */
export function mergeReasoning(existing: string | undefined, stripped: string | undefined): string | undefined {
  if (!stripped) return existing
  const label = '（模型把思考写进了正文，已自动剥离）'
  return existing ? `${existing}\n\n${label}\n${stripped}` : `${label}\n${stripped}`
}