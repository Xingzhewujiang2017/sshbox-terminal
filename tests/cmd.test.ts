/**
 * 命令块提示符清理的单测（src/cmdtext.ts 的 stripPromptPrefix）。
 * 跑法：npm run test:cmd（node --experimental-strip-types）。
 *
 * 为什么单独测：这个函数决定"插进终端的到底是什么" —— 多剥会把注释变成命令，
 * 少剥会让第二行带着 `$ ` 进终端报 command not found。两端都得钉死。
 */
import { stripPromptPrefix } from '../src/cmdtext.ts'

let failed = 0
const eq = (got: string, want: string, name: string) => {
  if (got !== want) {
    failed++
    console.error(`  ✗ ${name}\n    得到: ${JSON.stringify(got)}\n    期望: ${JSON.stringify(want)}`)
  }
}

// 1. 用户报的那个 bug：多行都带 $ 提示符，必须逐行剥
eq(stripPromptPrefix('$ df -h\n$ free -h'), 'df -h\nfree -h', '多行 $ 逐行剥')

// 2. 前导空格也算提示符（模型常缩进）
eq(stripPromptPrefix('  $ df -h\n\t$ free -h'), 'df -h\nfree -h', '缩进的 $ 也剥')

// 3. # 是 shell 注释，不能剥（剥了会把注释变成一条报错的命令）
eq(stripPromptPrefix('# systemctl restart nginx'), '# systemctl restart nginx', '# 保留')
eq(stripPromptPrefix('$ echo a\n# 注释行'), 'echo a\n# 注释行', '混合块里 # 保留')

// 4. > 是合法重定向，不能剥
eq(stripPromptPrefix('> /tmp/out.txt'), '> /tmp/out.txt', '> 保留')

// 5. $ 后面没有空白 = 变量，绝不能剥
eq(stripPromptPrefix('$HOME/bin/run'), '$HOME/bin/run', '$HOME 不剥')
eq(stripPromptPrefix('$'), '$', '孤立的 $ 不剥')

// 6. 本来就没有提示符：原样（只 trim）
eq(
  stripPromptPrefix('kubectl get pods -A\nkubectl get svc -A'),
  'kubectl get pods -A\nkubectl get svc -A',
  '无提示符原样'
)

// 7. 空串 / 空白
eq(stripPromptPrefix(''), '', '空串')
eq(stripPromptPrefix('   \n  '), '', '纯空白')

// 8. 首尾空白
eq(stripPromptPrefix('\n$ df -h\n\n'), 'df -h', '首尾空白清掉')

if (failed) {
  console.error(`\n== stripPromptPrefix：${failed} 个用例失败 ==`)
  process.exit(1)
}
console.log('== stripPromptPrefix：全部通过 ==')
