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

export interface QuickCommand {
  id: string
  label: string
  command: string
  /** 发送后是否自动回车（关掉 = 只填进终端，自己改完再执行） */
  enter: boolean
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
  quick_commands: QuickCommand[]
  history_enabled: boolean
  history_interval_secs: number
  history_retention_days: number
}

// --- SFTP / 本地文件（v0.4）------------------------------------------------

export interface LocalEntry {
  name: string
  path: string
  is_dir: boolean
  is_symlink: boolean
  size: number
  modified: number
  /** POSIX 权限位；Windows 上为空串 */
  permissions: string
  hidden: boolean
}

export interface LocalListing {
  path: string
  parent?: string | null
  entries: LocalEntry[]
  /** Windows 盘符（"C:\\"） */
  drives: string[]
  home: string
}

export interface RemoteEntry {
  name: string
  path: string
  is_dir: boolean
  is_symlink: boolean
  size: number
  modified: number
  /** "rwxr-xr-x"（不含类型位） */
  permissions: string
}

export interface RemoteListing {
  path: string
  parent?: string | null
  entries: RemoteEntry[]
  home: string
}

export interface RemoteStat {
  size: number
  modified: number
  is_dir: boolean
}

// --- 端口转发（v0.4）------------------------------------------------------

export type ForwardKind = 'local' | 'remote'

export interface ForwardRule {
  id: string
  /** local = 本机监听 → 远端目标；remote = 远端监听 → 本机目标 */
  kind: ForwardKind
  listen_host: string
  listen_port: number
  target_host: string
  target_port: number
  auto_start: boolean
  label?: string | null
}

export interface ForwardStatus extends ForwardRule {
  running: boolean
  /** 实际监听端口（远程转发可能被服务端改写） */
  actual_port: number
  error?: string | null
}

export type TransferState = 'running' | 'done' | 'cancelled' | 'error'

export interface TransferProgress {
  task_id: string
  sid: string
  direction: 'up' | 'down'
  name: string
  local: string
  remote: string
  done: number
  total: number
  /** bytes/s，按上次上报间隔算 */
  rate: number
  state: TransferState
  message?: string | null
}

// --- live metrics (shared by the monitor panel and the fleet overview) ---

export interface DiskUsage {
  mount: string
  total_kb: number
  used_kb: number
  use_pct: number
  /** 容器叠加层 / 只读镜像（overlay、squashfs）：不是独立的盘，不参与「最满的盘」。 */
  virtual_fs?: boolean
}
/// 到默认网关的时延/丢包。拿不到就是 null（没网关 / 没装 ping）。
export interface PingInfo {
  target: string
  sent: number
  recv: number
  loss_pct: number
  rtt_min: number
  rtt_avg: number
  rtt_max: number
  jitter: number
}

/** `monitor_recent` 里的一条 ping：面板画迷你图要时间标签，`PingInfo` 自身没有。 */
export interface RecentPing {
  ts: number
  rtt_avg: number
  loss_pct: number
}

export interface NetIf {
  name: string
  rx_bps: number
  tx_bps: number
}
export interface DiskIo {
  name: string
  read_bps: number
  write_bps: number
}
export interface ProcInfo {
  pid: number
  name: string
  state: string
  cpu_pct: number
  rss_kb: number
}
export interface Metrics {
  ts: number
  platform?: string
  cpu_pct: number
  cpu_per_core: number[]
  mem_total_kb: number
  mem_used_kb: number
  mem_pct: number
  swap_total_kb: number
  swap_used_kb: number
  net: NetIf[]
  disk_io: DiskIo[]
  disks: DiskUsage[]
  load: number[]
  processes: ProcInfo[]
  proc_total: number
}

// --- history ---

export interface HistoryBucket {
  ts: number
  cpu_pct: number
  mem_pct: number
  net_rx: number
  net_tx: number
  disk_r: number
  disk_w: number
  load1: number
}

/** ping 表的时延/丢包桶：慢采集 15s 一档才有值，无网关时整桶 None。 */
export interface PingBucket {
  ts: number
  latency_ms: number | null
  jitter_ms: number | null
  loss_pct: number | null
}

export interface PingRange {
  bucket_secs: number
  buckets: PingBucket[]
}

export interface HistoryRange {
  buckets: HistoryBucket[]
  raw_points: number
  bucket_secs: number
  oldest: number
  newest: number
}

export interface HistoryHost {
  host_id: string
  rows: number
  oldest: number
  newest: number
  last_cpu: number
  last_mem: number
}

export interface HistoryStats {
  path: string
  bytes: number
  rows: number
  hosts: number
  oldest: number
  newest: number
  retention_days: number
}

export interface HistoryExport {
  path: string
  rows: number
  bytes: number
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

/** 一个模型接入点。密钥不在前端 —— 只有 `has_key` 这个布尔值。 */
export interface AiProfile {
  id: string
  name: string
  /** `openai` | `anthropic` | `gemini` */
  protocol: string
  base_url: string
  model: string
  temperature: number
  max_tokens: number
  has_key: boolean
}

export interface AiSettings {
  /** 留空 = 还没选（AI 入口禁用） */
  active_profile_id: string
  /** 「解释这段」单独用的接入点；留空 = 跟 active_profile_id 一样 */
  explain_profile_id: string
  profiles: AiProfile[]
  /** 巡检报告里附一段 AI 结论 */
  report_ai_summary: boolean
}

export interface ChatMessage {
  role: string
  content: string
}

/** 巡检报告生成结果。 */
export interface ReportResult {
  markdown_path: string
  html_path: string
  dir: string
  findings: number
  critical: number
  ai_used: boolean
  ai_error: string | null
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

/** 演练台（lab）一次性执行结果。exit=-1 且 ok=false 表示通道错误或超时。 */
export interface LabExec {
  sid: string
  ok: boolean
  stdout: string
  exit: number
  elapsed_ms: number
  error: string | null
}

/** 演练台快 ping（3s 高采）：拿不到网关时 target/rtt_avg 为 null，不编 0。 */
export interface PingNow {
  target: string | null
  rtt_avg: number | null
  loss_pct: number | null
}

export const api = {
  // hosts
  hostsList: () => call<HostsFile>('hosts_list'),
  hostSave: (host: Host, password?: string) =>
    call<Host>('host_save', { host, password: password ?? null }),
  hostDelete: (id: string) => call<void>('host_delete', { id }),
  hostReorder: (ids: string[]) => call<void>('host_reorder', { ids }),
  hostTouch: (id: string) => call<void>('host_touch', { id }),
  groupsReorder: (order: string[]) => call<string[]>('groups_reorder', { order }),

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
  /** 静态信息快照：面板挂载时补拉一次，避免错过一次性事件。 */
  monitorStatic: (sid: string) => call<Record<string, unknown> | null>('monitor_static', { sid }),
  monitorSetVisible: (sid: string, visible: boolean) =>
    call<void>('monitor_set_visible', { sid, visible }),
  monitorRestart: (sid: string) => call<void>('monitor_restart', { sid }),
  monitorSetPaused: (sid: string, paused: boolean) => call<void>('monitor_set_paused', { sid, paused }),
  monitorPaused: (sid: string) => call<boolean>('monitor_paused', { sid }),
  monitorSampleNow: (sid: string) => call<void>('monitor_sample_now', { sid }),
  /** 面板挂载时补齐曲线：该会话最近采到的原始点（后端内存环形缓冲，不是历史库）。 */
  monitorRecent: (sid: string) =>
    call<{ metrics: Metrics[]; ping: RecentPing[]; interval_secs: number }>('monitor_recent', {
      sid,
    }),

  // history
  historyRange: (hostId: string, from: number, to: number, maxPoints?: number) =>
    call<HistoryRange>('history_range', {
      hostId,
      from,
      to,
      maxPoints: maxPoints ?? null,
    }),
  /** 一次查多台主机（组内历史对比）。单台无数据就缺那一台，不报错。 */
  execBatch: (sid: string, cmd: string, timeoutSecs: number) =>
    call<LabExec>('exec_batch', { sid, cmd, timeoutSecs }),
  pingNow: (sid: string) => call<PingNow>('ping_now', { sid }),
  historyRangeMulti: (hostIds: string[], from: number, to: number, maxPoints?: number) =>
    call<Record<string, HistoryRange>>('history_range_multi', {
      hostIds, from, to, maxPoints: maxPoints ?? null,
    }),
  /** 组内时延/丢包对比（ping 表，慢采集 15s 一档）。 */
  pingRangeMulti: (hostIds: string[], from: number, to: number, maxPoints?: number) =>
    call<Record<string, PingRange>>('ping_range_multi', {
      hostIds, from, to, maxPoints: maxPoints ?? null,
    }),
  historyHosts: () => call<HistoryHost[]>('history_hosts'),
  historyStats: () => call<HistoryStats>('history_stats'),
  historyExport: (hostId: string, from: number, to: number, destDir?: string | null) =>
    call<HistoryExport>('history_export', {
      hostId,
      from,
      to,
      destDir: destDir ?? null,
    }),
  /** 总览 CSV：后端落盘到导出目录（<a download> 在 WebView2 里会被静默拦截）。 */
  exportCsvText: (filename: string, content: string, destDir?: string | null) =>
    call<HistoryExport>('export_csv_text', {
      filename,
      content,
      destDir: destDir ?? null,
    }),

  // sftp / 本地文件
  localList: (path?: string | null) => call<LocalListing>('local_list', { path: path ?? null }),
  localKinds: (paths: string[]) => call<string[]>('local_kinds', { paths }),
  localMkdir: (path: string) => call<void>('local_mkdir', { path }),
  localRename: (from: string, to: string) => call<void>('local_rename', { from, to }),
  localRemove: (path: string, isDir: boolean, recursive: boolean) =>
    call<void>('local_remove', { path, isDir, recursive }),
  sftpList: (sid: string, path?: string | null) =>
    call<RemoteListing>('sftp_list', { sid, path: path ?? null }),
  sftpStat: (sid: string, path: string) => call<RemoteStat | null>('sftp_stat', { sid, path }),
  sftpMkdir: (sid: string, path: string) => call<void>('sftp_mkdir', { sid, path }),
  sftpRename: (sid: string, from: string, to: string) =>
    call<void>('sftp_rename', { sid, from, to }),
  sftpRemove: (sid: string, path: string, isDir: boolean, recursive: boolean) =>
    call<void>('sftp_remove', { sid, path, isDir, recursive }),
  sftpUpload: (sid: string, local: string, remote: string, resume: boolean) =>
    call<string>('sftp_upload', { sid, local, remote, resume }),
  sftpDownload: (sid: string, remote: string, local: string, resume: boolean) =>
    call<string>('sftp_download', { sid, remote, local, resume }),
  sftpUploadDir: (sid: string, local: string, remote: string) =>
    call<number>('sftp_upload_dir', { sid, local, remote }),
  sftpDownloadDir: (sid: string, remote: string, local: string) =>
    call<number>('sftp_download_dir', { sid, remote, local }),
  sftpCancel: (taskId: string) => call<boolean>('sftp_cancel', { taskId }),
  sftpForget: (sid: string) => call<void>('sftp_forget', { sid }),
  /** 拖文件到终端：上传到 `<家目录>/sshbox-uploads/`，返回远端路径 */
  sftpUploadDrop: (sid: string, paths: string[]) =>
    call<string[]>('sftp_upload_drop', { sid, paths }),

  // 端口转发
  forwardList: (sid: string) => call<ForwardStatus[]>('forward_list', { sid }),
  forwardSave: (sid: string, rule: ForwardRule) => call<void>('forward_save', { sid, rule }),
  forwardDelete: (sid: string, id: string) => call<void>('forward_delete', { sid, id }),
  forwardStart: (sid: string, rule: ForwardRule) => call<number>('forward_start', { sid, rule }),
  forwardStop: (sid: string, id: string) => call<void>('forward_stop', { sid, id }),
  forwardStopAll: (sid: string) => call<number>('forward_stop_all', { sid }),

  // settings
  settingsGet: () => call<Settings>('settings_get'),
  settingsSet: (settings: Settings) => call<void>('settings_set', { settings }),

  // AI（v0.5）—— 密钥只在后端，前端只问「有没有」
  aiSettings: () => call<AiSettings>('ai_settings'),
  aiProfileSave: (profile: AiProfile, key?: string) =>
    call<AiSettings>('ai_profile_save', { profile, key: key ?? null }),
  aiProfileDelete: (id: string) => call<AiSettings>('ai_profile_delete', { id }),
  aiSetActive: (id: string) => call<AiSettings>('ai_set_active', { id }),
  aiSetExplainProfile: (id: string) => call<AiSettings>('ai_set_explain_profile', { id }),
  aiSetReportSummary: (on: boolean) => call<void>('ai_set_report_summary', { on }),
  aiKeyHas: (id: string) => call<boolean>('ai_key_has', { id }),
  aiKeyDelete: (id: string) => call<void>('ai_key_delete', { id }),
  aiPresets: () => call<AiProfile[]>('ai_presets'),
  aiTest: (id?: string) => call<string>('ai_test', { id: id ?? null }),
  /** 流式：增量走 ssh://ai/delta 事件，返回完整文本 */
  aiChat: (reqId: string, messages: ChatMessage[], profileId?: string) =>
    call<string>('ai_chat', { reqId, messages, profileId: profileId ?? null }),
  aiCancel: (reqId: string) => call<void>('ai_cancel', { reqId }),
  /** 统一入口：按 kind 组装上下文（explain / command / chat）并流式回答 */
  aiAsk: (opts: {
    reqId: string
    kind: string
    sid?: string | null
    selection?: string | null
    ask?: string | null
    tail?: string | null
    history?: ChatMessage[] | null
    profileId?: string | null
  }) =>
    call<string>('ai_ask', {
      reqId: opts.reqId,
      kind: opts.kind,
      sid: opts.sid ?? null,
      selection: opts.selection ?? null,
      ask: opts.ask ?? null,
      tail: opts.tail ?? null,
      history: opts.history ?? null,
      profileId: opts.profileId ?? null,
    }),

  // 巡检报告（v0.5）
  reportGenerate: (sid: string, hours?: number, destDir?: string) =>
    call<ReportResult>('report_generate', {
      sid,
      hours: hours ?? null,
      destDir: destDir ?? null,
    }),
  reportReveal: (path: string) => call<void>('report_reveal', { path }),

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
