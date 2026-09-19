/**
 * 传输队列状态（模块级，刻意不放在组件里）。
 *
 * SFTP 面板是个浮层，关掉就卸载了；但传输任务在后端继续跑。把队列放在模块里，
 * 关面板再打开还能看到进度条，也能在别处（比如状态栏）显示"还有 N 个传输中"。
 */
import { computed, ref } from 'vue'
import { listen } from '@tauri-apps/api/event'
import type { TransferProgress } from './api'

export const transfers = ref<TransferProgress[]>([])
/** 外部拖入的文件（App.vue 收到 Tauri 拖放事件后写进来，面板消费） */
export const pendingDrop = ref<string[]>([])

let started = false

function upsert(p: TransferProgress) {
  const i = transfers.value.findIndex((t) => t.task_id === p.task_id)
  if (i >= 0) transfers.value[i] = p
  else transfers.value.unshift(p)

  // 已结束的只留最近 50 条，防止长会话里无限增长
  const finished = transfers.value.filter((t) => t.state !== 'running')
  if (finished.length > 50) {
    const keep = new Set(finished.slice(0, 50).map((t) => t.task_id))
    transfers.value = transfers.value.filter((t) => t.state === 'running' || keep.has(t.task_id))
  }
}

export async function initTransfers() {
  if (started) return
  started = true
  await listen<TransferProgress>('sftp://progress', (e) => upsert(e.payload))
}

export const activeTransfers = computed(() => transfers.value.filter((t) => t.state === 'running'))

export function clearFinished() {
  transfers.value = transfers.value.filter((t) => t.state === 'running')
}

export function transfersFor(sid: string) {
  return transfers.value.filter((t) => t.sid === sid)
}

/** 给状态栏用的一句话摘要 */
export function transferSummary(): string {
  const n = activeTransfers.value.length
  if (!n) return ''
  return `${n} 个传输中`
}
