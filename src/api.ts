/**
 * Typed bridge to the Rust side.
 *
 * Rust reports failures as `SSHBOX_ERR:<json>` so the UI can distinguish
 * recoverable conditions (unknown host key, missing password) from plain
 * errors instead of pattern-matching on message text.
 */
import { invoke } from '@tauri-apps/api/core'

export const ERR_PREFIX = 'SSHBOX_ERR:'

export interface ErrPayload {
  kind: string
  message: string
  host?: string
  port?: number
  key_type?: string
  fingerprint?: string
  expected_fingerprint?: string
  host_id?: string
}

export class SshboxError extends Error {
  payload: ErrPayload
  constructor(payload: ErrPayload) {
    super(payload.message)
    this.name = 'SshboxError'
    this.payload = payload
  }
  get kind() {
    return this.payload.kind
  }
}

export function toSshboxError(e: unknown): SshboxError {
  const s = typeof e === 'string' ? e : e instanceof Error ? e.message : String(e)
  if (s.startsWith(ERR_PREFIX)) {
    try {
      return new SshboxError(JSON.parse(s.slice(ERR_PREFIX.length)) as ErrPayload)
    } catch {
      /* fall through to a generic error */
    }
  }
  return new SshboxError({ kind: 'unknown', message: s })
}

export async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args)
  } catch (e) {
    const err = toSshboxError(e)
    void uiLog(`${cmd} 失败 kind=${err.kind}: ${err.message}`, err.kind === 'unknown' ? 'error' : 'warn')
    throw err
  }
}

/**
 * Push a UI event into the backend log file.
 *
 * WebView2 is not scriptable from outside the app, so this is how frontend
 * state transitions become observable when debugging "nothing happened".
 */
export function uiLog(msg: string, level: 'info' | 'warn' | 'error' | 'debug' = 'info'): Promise<void> {
  return invoke<void>('ui_log', { msg, level }).catch(() => {})
}

// --- types mirroring the Rust structs -------------------------------------

export type AuthMode = 'password' | 'key'

export interface Host {
  id: string
  name: string
  group: string
  host: string
  port: number
  username: string
  auth: AuthMode
  key_path?: string | null
  save_password: boolean
  auto_reconnect: boolean
  host_key_policy?: string | null
  color?: string | null
  note?: string | null
  last_used?: number | null
  created_at: number
}

export interface HostsFile {
  version: number
  groups: string[]
  hosts: Host[]
}

export interface Settings {
  sample_interval_secs: number
  monitor_enabled: boolean
  auto_reconnect: boolean
  reconnect_max_attempts: number
  process_top_n: number
  alerts_enabled: boolean
  cpu_alert_pct: number
  mem_alert_pct: number
  disk_alert_pct: number
  confirm_on_close_tab: boolean
  theme: string
}

export interface Alert {
  kind: 'cpu' | 'mem' | 'disk'
  title: string
  body: string
  value: number
  threshold: number
}

export interface SessionInfo {
  sid: string
  host_id?: string | null
  label: string
  host: string
  port: number
  username: string
  connected_at: number
  closed: boolean
}

export interface KnownHostEntry {
  host: string
  key_type: string
  fingerprint: string
  line: number
}

export interface AppPaths {
  data_dir: string
  hosts_json: string
  settings_json: string
  known_hosts: string
  credential_store_ok: boolean
}

export function emptyHost(): Host {
  return {
    id: '',
    name: '',
    group: '默认',
    host: '',
    port: 22,
    username: 'root',
    auth: 'password',
    key_path: '',
    save_password: true,
    auto_reconnect: true,
    host_key_policy: 'strict',
    color: null,
    note: '',
    last_used: null,
    created_at: 0,
  }
}

// --- command wrappers ------------------------------------------------------

export const api = {
  // hosts
  hostsList: () => call<HostsFile>('hosts_list'),
  hostSave: (host: Host, password?: string) =>
    call<Host>('host_save', { host, password: password ?? null }),
  hostDelete: (id: string) => call<void>('host_delete', { id }),
  hostReorder: (ids: string[]) => call<void>('host_reorder', { ids }),
  hostTouch: (id: string) => call<void>('host_touch', { id }),

  // credentials
  credentialHas: (id: string) => call<boolean>('credential_has', { id }),
  credentialSet: (id: string, password: string) =>
    call<void>('credential_set', { id, password }),
  credentialDelete: (id: string) => call<void>('credential_delete', { id }),
  credentialStoreStatus: () => call<boolean>('credential_store_status'),

  // sessions
  connectHost: (opts: {
    hostId: string
    password?: string
    keyPassphrase?: string
    acceptHostKey?: boolean
    savePassword?: boolean
  }) =>
    call<string>('connect_host', {
      hostId: opts.hostId,
      password: opts.password ?? null,
      keyPassphrase: opts.keyPassphrase ?? null,
      acceptHostKey: opts.acceptHostKey ?? false,
      savePassword: opts.savePassword ?? null,
    }),
  connect: (params: Record<string, unknown>) => call<string>('connect', { params }),
  disconnect: (sid: string) => call<void>('disconnect', { sid }),
  termWrite: (sid: string, data: string) => call<void>('term_write', { sid, data }),
  termResize: (sid: string, cols: number, rows: number) =>
    call<void>('term_resize', { sid, cols, rows }),
  listSessions: () => call<SessionInfo[]>('list_sessions'),
  sessionAlive: (sid: string) => call<boolean>('session_alive', { sid }),
  monitorSetVisible: (sid: string, visible: boolean) =>
    call<void>('monitor_set_visible', { sid, visible }),
  monitorRestart: (sid: string) => call<void>('monitor_restart', { sid }),
  monitorSetPaused: (sid: string, paused: boolean) => call<void>('monitor_set_paused', { sid, paused }),
  monitorPaused: (sid: string) => call<boolean>('monitor_paused', { sid }),
  monitorSampleNow: (sid: string) => call<void>('monitor_sample_now', { sid }),

  // settings
  settingsGet: () => call<Settings>('settings_get'),
  settingsSet: (settings: Settings) => call<void>('settings_set', { settings }),

  // known_hosts
  knownHostsList: () => call<KnownHostEntry[]>('known_hosts_list'),
  knownHostsRemove: (host: string, port: number) =>
    call<number>('known_hosts_remove', { host, port }),

  // misc
  appPaths: () => call<AppPaths>('app_paths'),
  hostsExport: (path: string, includeSecrets: boolean) =>
    call<number>('hosts_export', { path, includeSecrets }),
  hostsImport: (path: string) => call<number>('hosts_import', { path }),
}
