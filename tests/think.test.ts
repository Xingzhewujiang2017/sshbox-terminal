/**
 * 与 Rust 侧共用同一份样例（src-tauri/tests/fixtures/think_cases.json）。
 * 跑法：npm run test:think（node --experimental-strip-types）。
 */
import { readFileSync } from 'node:fs'
import { stripThinking } from '../src/think.ts'

type Case = {
  name: string
  input: string
  think_none?: boolean
  think_has?: string[]
  answer_has?: string[]
  answer_has_not?: string[]
}

const fix = new URL('../src-tauri/tests/fixtures/think_cases.json', import.meta.url)
const cases: Case[] = JSON.parse(readFileSync(fix, 'utf8'))

let failed = 0
const check = (ok: boolean, msg: string) => {
  if (!ok) {
    failed++
    console.error('  ✗ ' + msg)
  }
}

if (cases.length < 7) {
  console.error(`样例被删了？只有 ${cases.length} 个`)
  process.exit(1)
}

for (const c of cases) {
  const { answer, reasoning } = stripThinking(c.input)
  if (c.think_none) {
    check(!reasoning, `[${c.name}] 不该剥出思考，却剥了`)
    check(answer === c.input, `[${c.name}] 不剥时应原样返回`)
  }
  if (c.think_has) {
    check(!!reasoning, `[${c.name}] 应该剥出思考，但没有`)
    for (const s of c.think_has) check(!!reasoning?.includes(s), `[${c.name}] 思考里缺「${s}」`)
  }
  for (const s of c.answer_has ?? []) check(answer.includes(s), `[${c.name}] 正文里缺「${s}」`)
  for (const s of c.answer_has_not ?? []) check(!answer.includes(s), `[${c.name}] 正文里不该有「${s}」`)
}

if (failed) {
  console.error(`\nFAIL：${failed} 处断言失败`)
  process.exit(1)
}
console.log(`OK：${cases.length} 个样例全部通过（与 Rust 侧同一份 fixture）`)
