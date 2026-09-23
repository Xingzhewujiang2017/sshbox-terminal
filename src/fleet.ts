/**
 * Fleet-wide live state.
 *
 * The monitor panel only subscribes for the tab it belongs to, so switching
 * tabs throws away every other machine's samples. This module subscribes once,
 * for the whole app, and keeps the latest sample (and latest alert) per
 * session — which is what makes an overview of every host possible without a
 * single extra SSH connection.
 *
 * Nothing here talks to the backend: the monitor loop already emits for every
 * connected session, this just stops ignoring most of it.
 */
import { listen } from '@tauri-apps/api/event'
import { ref } from 'vue'
import { isActiveAlert } from './alertsView'
import type { Alert, Metrics } from './api'

export interface FleetSample {
  metrics: Metrics
  /** Local receive time — the metric's own `ts` is the sampler's clock. */
  at: number
}

export interface FleetAlert {
  alert: Alert
  at: number
  /** 后端发来解除事件的时间（ms）。徽标 / 「告警 N」只算未解除的。 */
  resolvedAt?: number
}

const samples = ref<Record<string, FleetSample>>({})
const alerts = ref<Record<string, FleetAlert>>({})
/** Ticks once a second so "8 秒前" stays honest without every row owning a timer. */
const now = ref(Date.now())

let unlistenMetrics: (() => void) | null = null
let unlistenAlerts: (() => void) | null = null
let ticker: ReturnType<typeof setInterval> | null = null

export async function initFleet(): Promise<void> {
  if (unlistenMetrics) return
  unlistenMetrics = await listen<{ sid: string; metrics: Metrics }>('ssh://metrics', (e) => {
    samples.value[e.payload.sid] = { metrics: e.payload.metrics, at: Date.now() }
  })
  unlistenAlerts = await listen<{ sid: string; alert: Alert; resolved?: boolean }>(
    'ssh://alert',
    (e) => {
      const cur = alerts.value[e.payload.sid]
      if (e.payload.resolved) {
        // 解除：保留条目、打上时间戳，但不再计入徽标和「告警 N」。
        // 后端迟滞保证解除只发一次，这里不用防抖。
        if (cur) cur.resolvedAt = Date.now()
        return
      }
      alerts.value[e.payload.sid] = { alert: e.payload.alert, at: Date.now() }
    },
  )
  ticker = setInterval(() => (now.value = Date.now()), 1000)
}

export function disposeFleet(): void {
  unlistenMetrics?.()
  unlistenAlerts?.()
  if (ticker) clearInterval(ticker)
  unlistenMetrics = unlistenAlerts = null
  ticker = null
}

export function sampleFor(sid: string): FleetSample | undefined {
  return samples.value[sid]
}

export function alertFor(sid: string): FleetAlert | undefined {
  return alerts.value[sid]
}

/** 未解除的告警（口径见 alertsView.isActiveAlert）。 */
export function activeAlertFor(sid: string): FleetAlert | undefined {
  const a = alerts.value[sid]
  return isActiveAlert(a) ? a : undefined
}

/** Called when a session goes away, so a stale row cannot linger forever. */
export function forgetSession(sid: string): void {
  delete samples.value[sid]
  delete alerts.value[sid]
}

export const fleetNow = now
