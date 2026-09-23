/**
 * 告警的显示口径 —— 只有纯函数，方便单测（tests/alerts.test.ts）。
 *
 * 为什么单独一个模块：后端会发「解除」事件（resolved=true），而徽标和
 * 「告警 N」必须只算**未解除**的那些。这条判断纯前端、写错了不报错，
 * 只会让一次 CPU 冲高的告警亮到会话结束 —— 单测能钉住，手点很难发现。
 */

export interface AlertStateLike {
  /** 后端发来解除事件的时间（ms）。从没解除过就是 undefined。 */
  resolvedAt?: number
}

/** 未解除的告警才算数（总览徽标 / 「告警 N」共用这一条口径）。 */
export function isActiveAlert(a: AlertStateLike | undefined | null): boolean {
  return !!a && a.resolvedAt == null
}
