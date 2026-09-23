/**
 * 告警显示口径的单测（src/alertsView.ts 的 isActiveAlert）。
 * 跑法：npm run test:alerts（node --experimental-strip-types）。
 *
 * 为什么单独测：后端会发「解除」事件，徽标和「告警 N」只算未解除的。
 * 判断写反、或漏掉 resolvedAt=0，都不会报错 —— 只会让一次冲高的告警
 * 永久亮着（假警报），而这正是「看起来正常」的那类坏法。
 */
import { isActiveAlert } from '../src/alertsView.ts'

let failed = 0
const eq = (got: boolean, want: boolean, name: string) => {
  if (got !== want) {
    failed++
    console.error(`  ✗ ${name}\n    得到: ${got}\n    期望: ${want}`)
  }
}

// 1. 从没告过警
eq(isActiveAlert(undefined), false, '没有条目 → 不算数')
eq(isActiveAlert(null), false, 'null → 不算数')

// 2. 触发过、还没解除 → 算数
eq(isActiveAlert({}), true, '触发过且未解除 → 算数')

// 3. 解除过 → 不再算数（这就是本次修的那条）
eq(isActiveAlert({ resolvedAt: 1_700_000_000_000 }), false, '解除过 → 不算数')

// 4. resolvedAt=0 也是「解除过」，不能当 falsy 漏掉
eq(isActiveAlert({ resolvedAt: 0 }), false, 'resolvedAt=0 也算解除过')

console.log(failed === 0 ? 'alerts.test.ts: 全部通过' : `alerts.test.ts: ${failed} 条失败`)
if (failed) process.exit(1)
