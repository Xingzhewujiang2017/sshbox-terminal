<template>
  <div class="mask">
    <div class="panel">
      <header class="head">
        <span class="title">文件传输</span>
        <span class="host" :title="label">{{ label }}</span>
        <span class="spacer" />
        <button class="icon" title="刷新（F5）" @click="refreshAll">⟳</button>
        <button class="icon" title="关闭（Esc）" @click="emit('close')">×</button>
      </header>

      <div v-if="err" class="err">
        <span>{{ err }}</span>
        <button @click="err = ''">×</button>
      </div>

      <div class="panes">
        <!-- 本地 -->
        <section class="pane">
          <div class="pane-head">
            <span class="tag">本地</span>
            <button class="icon" title="上一级" :disabled="!local?.parent" @click="goUp('local')">↑</button>
            <input
              v-model="localInput"
              class="path"
              spellcheck="false"
              @keyup.enter="loadLocal(localInput)"
            />
            <select
              v-if="local?.drives.length"
              class="drives"
              title="盘符"
              @change="onDrive($event)"
            >
              <option value="">盘符…</option>
              <option v-for="d in local.drives" :key="d" :value="d">{{ d }}</option>
            </select>
            <button class="icon" title="回到主目录" @click="loadLocal(local?.home)">⌂</button>
          </div>
          <ul class="list" @click="onListClick('local', $event)" @dblclick="onListDbl('local', $event)">
            <li
              v-for="e in local?.entries ?? []"
              :key="e.path"
              :data-path="e.path"
              :data-dir="e.is_dir ? '1' : '0'"
              :class="{ sel: isSel('local', e.path), dim: e.hidden }"
            >
              <span class="ico">{{ e.is_dir ? '📁' : e.is_symlink ? '🔗' : '📄' }}</span>
              <span class="nm" :title="e.name">{{ e.name }}</span>
              <span class="sz">{{ e.is_dir ? '—' : fmtSize(e.size) }}</span>
              <span class="tm">{{ fmtTime(e.modified) }}</span>
            </li>
            <li v-if="loadingLocal" class="empty">读取中…</li>
            <li v-else-if="!local?.entries.length" class="empty">（空目录）</li>
          </ul>
          <div class="pane-foot">
            <button :disabled="busy" @click="mk('local')">新建目录</button>
            <button :disabled="busy || localSel.length !== 1" @click="ren('local')">改名</button>
            <button :disabled="busy || !localSel.length" @click="askDelete('local')">删除</button>
            <span class="spacer" />
            <button
              class="primary"
              :disabled="busy || !localSel.length"
              title="上传选中项到远端当前目录"
              @click="uploadSelected"
            >
              上传 →
            </button>
          </div>
        </section>

        <!-- 远端 -->
        <section class="pane">
          <div class="pane-head">
            <span class="tag">远端</span>
            <button class="icon" title="上一级" :disabled="!remote?.parent" @click="goUp('remote')">↑</button>
            <input
              v-model="remoteInput"
              class="path"
              spellcheck="false"
              @keyup.enter="loadRemote(remoteInput)"
            />
            <button class="icon" title="回到家目录" @click="loadRemote(remote?.home)">⌂</button>
          </div>
          <ul class="list" @click="onListClick('remote', $event)" @dblclick="onListDbl('remote', $event)">
            <li
              v-for="e in remote?.entries ?? []"
              :key="e.path"
              :data-path="e.path"
              :data-dir="e.is_dir ? '1' : '0'"
              :class="{ sel: isSel('remote', e.path) }"
            >
              <span class="ico">{{ e.is_dir ? '📁' : e.is_symlink ? '🔗' : '📄' }}</span>
              <span class="nm" :title="e.name">{{ e.name }}</span>
              <span class="sz">{{ e.is_dir ? '—' : fmtSize(e.size) }}</span>
              <span class="tm">{{ e.permissions || fmtTime(e.modified) }}</span>
            </li>
            <li v-if="loadingRemote" class="empty">读取中…</li>
            <li v-else-if="!remote?.entries.length" class="empty">（空目录）</li>
          </ul>
          <div class="pane-foot">
            <button :disabled="busy" @click="mk('remote')">新建目录</button>
            <button :disabled="busy || remoteSel.length !== 1" @click="ren('remote')">改名</button>
            <button :disabled="busy || !remoteSel.length" @click="askDelete('remote')">删除</button>
            <span class="spacer" />
            <button
              class="primary"
              :disabled="busy || !remoteSel.length"
              title="下载选中项到本地当前目录"
              @click="downloadSelected"
            >
              ← 下载
            </button>
          </div>
        </section>
      </div>

      <!-- 传输队列 -->
      <div class="queue">
        <div class="q-head">
          <span>传输队列</span>
          <span v-if="items.length" class="muted">{{ running.length }} 进行中 / {{ items.length }} 条</span>
          <span v-else class="muted">（空）</span>
          <span class="spacer" />
          <span v-if="queued" class="muted">排队 {{ queued }} 个</span>
          <button v-if="items.some((t) => t.state !== 'running')" class="icon" @click="clearFinished">
            清除已完成
          </button>
        </div>
        <div v-for="t in items" :key="t.task_id" class="q-row">
          <span class="dir">{{ t.direction === 'up' ? '↑' : '↓' }}</span>
          <span class="nm" :title="t.remote">{{ t.name }}</span>
          <div class="bar">
            <div class="fill" :class="t.state" :style="{ width: pct(t) + '%' }" />
          </div>
          <span class="meta">
            {{ fmtSize(t.done) }} / {{ fmtSize(t.total) }} · {{ fmtRate(t.rate) }} ·
            {{ stateLabel(t) }}
          </span>
          <button v-if="t.state === 'running'" class="icon" @click="cancel(t)">取消</button>
          <button
            v-else-if="t.state !== 'done' && t.direction === 'up'"
            class="icon"
            title="续传"
            @click="retry(t)"
          >
            重试
          </button>
        </div>
      </div>
    </div>

    <!-- 删除确认 -->
    <div v-if="pendingDelete" class="dialog">
      <div class="dlg-box">
        <div class="dlg-title">删除 {{ pendingDelete.paths.length }} 项？</div>
        <div class="dlg-body">
          <div v-for="p in pendingDelete.paths.slice(0, 6)" :key="p" class="mono">{{ baseName(p) }}</div>
          <div v-if="pendingDelete.paths.length > 6" class="muted">…还有 {{ pendingDelete.paths.length - 6 }} 项</div>
        </div>
        <label v-if="pendingDelete.hasDir" class="chk">
          <input v-model="pendingDelete.recursive" type="checkbox" />
          递归删除目录内容
        </label>
        <div class="dlg-foot">
          <button @click="pendingDelete = null">取消</button>
          <button class="danger" @click="doDelete">删除</button>
        </div>
      </div>
    </div>

    <!-- 同名文件冲突 -->
    <div v-if="conflict" class="dialog">
      <div class="dlg-box">
        <div class="dlg-title">目标已存在</div>
        <div class="dlg-body">
          <div class="mono">{{ conflict.name }}</div>
          <div class="muted">
            本地 {{ fmtSize(conflict.localSize) }} · 远端 {{ fmtSize(conflict.remoteSize) }}
          </div>
        </div>
        <div class="dlg-foot">
          <button @click="answerConflict('skip')">跳过</button>
          <button v-if="conflict.canResume" @click="answerConflict('resume')">
            续传（从 {{ fmtSize(conflict.remoteSize) }} 继续）
          </button>
          <button class="primary" @click="answerConflict('overwrite')">覆盖</button>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
/**
 * SFTP 双栏文件浏览器（全窗口浮层，形态同总览/历史）。
 *
 * 设计要点：
 * - 传输任务在后端跑，进度走 `sftp://progress` 事件进 transfers 模块级队列，
 *   所以关掉面板再打开，进度条还在（后端不会因为关面板就中断传输）。
 * - 前端只负责"发命令 + 排队"：并发上限 2，其余在队列里等。
 * - 同名冲突一律先问（覆盖/续传/跳过），不做静默覆盖。
 */
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import {
  api,
  toSshboxError,
  uiLog,
  type LocalEntry,
  type LocalListing,
  type RemoteEntry,
  type RemoteListing,
  type TransferProgress,
} from '../api'
import { clearFinished, pendingDrop, transfers } from '../transfers'

const props = defineProps<{ sid: string; label: string }>()
const emit = defineEmits<{ (e: 'close'): void }>()

type Side = 'local' | 'remote'
const MAX_CONCURRENT = 2

const local = ref<LocalListing | null>(null)
const remote = ref<RemoteListing | null>(null)
const localInput = ref('')
const remoteInput = ref('')
const localSel = ref<string[]>([])
const remoteSel = ref<string[]>([])
const loadingLocal = ref(false)
const loadingRemote = ref(false)
const err = ref('')

const pendingDelete = ref<{
  side: Side
  paths: string[]
  hasDir: boolean
  recursive: boolean
} | null>(null)

interface Conflict {
  name: string
  localSize: number
  remoteSize: number
  canResume: boolean
  resolve: (c: 'overwrite' | 'resume' | 'skip') => void
}
const conflict = ref<Conflict | null>(null)

/** 待发起的传输（并发 2，其余排队） */
interface Job {
  side: Side
  localPath: string
  remotePath: string
  isDir: boolean
}
const queue = ref<Job[]>([])
const busy = ref(false)

const items = computed(() => transfers.value.filter((t) => t.sid === props.sid))
const running = computed(() => items.value.filter((t) => t.state === 'running'))
const queued = computed(() => queue.value.length)

// --- 格式化 ---------------------------------------------------------------

function fmtSize(n: number): string {
  if (!n) return '0'
  const u = ['B', 'KB', 'MB', 'GB', 'TB']
  let i = 0
  let v = n
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024
    i++
  }
  return `${v < 10 && i > 0 ? v.toFixed(1) : Math.round(v)}${u[i]}`
}

function fmtRate(r: number): string {
  return r > 0 ? `${fmtSize(r)}/s` : '—'
}

function fmtTime(secs: number): string {
  if (!secs) return ''
  const d = new Date(secs * 1000)
  const p = (x: number) => String(x).padStart(2, '0')
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`
}

function baseName(p: string): string {
  const parts = p.split(/[\\/]/).filter(Boolean)
  return parts[parts.length - 1] ?? p
}

function pct(t: TransferProgress): number {
  if (!t.total) return t.state === 'done' ? 100 : 0
  return Math.min(100, Math.round((t.done / t.total) * 100))
}

function stateLabel(t: TransferProgress): string {
  switch (t.state) {
    case 'running':
      return `${pct(t)}%`
    case 'done':
      return '完成'
    case 'cancelled':
      return '已取消（可续传）'
    default:
      return t.message || '失败'
  }
}

// --- 加载 -----------------------------------------------------------------

async function loadLocal(path?: string | null) {
  loadingLocal.value = true
  try {
    const l = await api.localList(path ?? null)
    local.value = l
    localInput.value = l.path
    localSel.value = []
  } catch (e) {
    err.value = toSshboxError(e).message
  } finally {
    loadingLocal.value = false
  }
}

async function loadRemote(path?: string | null) {
  loadingRemote.value = true
  try {
    const r = await api.sftpList(props.sid, path ?? null)
    remote.value = r
    remoteInput.value = r.path
    remoteSel.value = []
  } catch (e) {
    err.value = toSshboxError(e).message
  } finally {
    loadingRemote.value = false
  }
}

function refreshAll() {
  void loadLocal(local.value?.path)
  void loadRemote(remote.value?.path)
}

function onDrive(ev: Event) {
  const v = (ev.target as HTMLSelectElement).value
  if (v) void loadLocal(v)
}

function goUp(side: Side) {
  const parent = side === 'local' ? local.value?.parent : remote.value?.parent
  if (parent) side === 'local' ? void loadLocal(parent) : void loadRemote(parent)
}

// --- 选择 -----------------------------------------------------------------

function selList(side: Side) {
  return side === 'local' ? localSel : remoteSel
}

function entriesOf(side: Side): (LocalEntry | RemoteEntry)[] {
  return (side === 'local' ? local.value?.entries : remote.value?.entries) ?? []
}

function isSel(side: Side, path: string): boolean {
  return selList(side).value.includes(path)
}

function onListClick(side: Side, ev: MouseEvent) {
  const li = (ev.target as HTMLElement).closest('li[data-path]') as HTMLElement | null
  if (!li) return
  const path = li.dataset.path!
  const sel = selList(side)
  const all = entriesOf(side).map((e) => e.path)
  if (ev.ctrlKey || ev.metaKey) {
    sel.value = sel.value.includes(path) ? sel.value.filter((p) => p !== path) : [...sel.value, path]
  } else if (ev.shiftKey && sel.value.length) {
    const a = all.indexOf(sel.value[0])
    const b = all.indexOf(path)
    if (a >= 0 && b >= 0) {
      const [lo, hi] = a < b ? [a, b] : [b, a]
      sel.value = all.slice(lo, hi + 1)
    }
  } else {
    sel.value = [path]
  }
}

function onListDbl(side: Side, ev: MouseEvent) {
  const li = (ev.target as HTMLElement).closest('li[data-path]') as HTMLElement | null
  if (!li) return
  const path = li.dataset.path!
  if (li.dataset.dir === '1') {
    side === 'local' ? void loadLocal(path) : void loadRemote(path)
  } else if (side === 'local') {
    // 双击本地文件 = 上传（MobaXterm 的习惯）
    void startJobs([{ side: 'local', localPath: path, remotePath: joinRemote(path), isDir: false }])
  } else {
    void startJobs([{ side: 'remote', localPath: joinLocal(path), remotePath: path, isDir: false }])
  }
}

function joinRemote(localPath: string): string {
  const dir = remote.value?.path ?? '/'
  return dir.endsWith('/') ? dir + baseName(localPath) : `${dir}/${baseName(localPath)}`
}

function joinLocal(remotePath: string): string {
  const dir = local.value?.path ?? ''
  const sep = dir.includes('\\') ? '\\' : '/'
  return dir.endsWith(sep) ? dir + baseName(remotePath) : dir + sep + baseName(remotePath)
}

function selectedEntries(side: Side) {
  const sel = selList(side).value
  return entriesOf(side).filter((e) => sel.includes(e.path))
}

// --- 传输 -----------------------------------------------------------------

function startJobs(jobs: Job[]) {
  queue.value.push(...jobs)
  void pump()
}

async function pump() {
  if (busy.value) return
  busy.value = true
  try {
    while (queue.value.length && running.value.length < MAX_CONCURRENT) {
      const job = queue.value[0]
      const done = await dispatch(job)
      if (done) queue.value.shift()
      else break // 用户在冲突框上点了取消 → 停下等下一次触发
    }
  } finally {
    busy.value = false
  }
}

/** 返回 true 表示这条已处理完（可以出队）。 */
async function dispatch(job: Job): Promise<boolean> {
  try {
    if (job.isDir) {
      if (job.side === 'local') {
        await api.sftpUploadDir(props.sid, job.localPath, job.remotePath)
      } else {
        await api.sftpDownloadDir(props.sid, job.remotePath, job.localPath)
      }
      return true
    }
    const st = job.side === 'local' ? await api.sftpStat(props.sid, job.remotePath) : null
    if (st && st.size > 0) {
      const localSize =
        job.side === 'local'
          ? (local.value?.entries.find((e) => e.path === job.localPath)?.size ?? 0)
          : (remote.value?.entries.find((e) => e.path === job.remotePath)?.size ?? 0)
      const choice = await askConflict(
        baseName(job.remotePath),
        localSize,
        st.size,
        job.side === 'local' && localSize > st.size,
      )
      if (choice === 'skip') return true
      if (job.side === 'local') {
        await api.sftpUpload(props.sid, job.localPath, job.remotePath, choice === 'resume')
      } else {
        await api.sftpDownload(props.sid, job.remotePath, job.localPath, false)
      }
      return true
    }
    if (job.side === 'local') {
      await api.sftpUpload(props.sid, job.localPath, job.remotePath, false)
    } else {
      await api.sftpDownload(props.sid, job.remotePath, job.localPath, false)
    }
    return true
  } catch (e) {
    err.value = toSshboxError(e).message
    void uiLog(`传输失败 ${job.localPath} → ${job.remotePath}: ${err.value}`, 'error')
    return true
  }
}

function askConflict(
  name: string,
  localSize: number,
  remoteSize: number,
  canResume: boolean,
): Promise<'overwrite' | 'resume' | 'skip'> {
  return new Promise((resolve) => {
    conflict.value = {
      name,
      localSize,
      remoteSize,
      canResume,
      resolve: (c) => {
        conflict.value = null
        resolve(c)
      },
    }
  })
}

function answerConflict(c: 'overwrite' | 'resume' | 'skip') {
  conflict.value?.resolve(c)
}

function uploadSelected() {
  const dir = remote.value?.path ?? '/'
  const jobs: Job[] = selectedEntries('local').map((e) => ({
    side: 'local' as Side,
    localPath: e.path,
    remotePath: dir.endsWith('/') ? dir + e.name : `${dir}/${e.name}`,
    isDir: e.is_dir,
  }))
  if (jobs.length) startJobs(jobs)
}

function downloadSelected() {
  const dir = local.value?.path ?? ''
  const sep = dir.includes('\\') ? '\\' : '/'
  const jobs: Job[] = selectedEntries('remote').map((e) => ({
    side: 'remote' as Side,
    localPath: dir.endsWith(sep) ? dir + e.name : dir + sep + e.name,
    remotePath: e.path,
    isDir: e.is_dir,
  }))
  if (jobs.length) startJobs(jobs)
}

async function cancel(t: TransferProgress) {
  await api.sftpCancel(t.task_id)
}

function retry(t: TransferProgress) {
  startJobs([
    { side: 'up' === t.direction ? 'local' : 'remote', localPath: t.local, remotePath: t.remote, isDir: false },
  ])
}

// --- 文件操作 -------------------------------------------------------------

async function mk(side: Side) {
  const dir = side === 'local' ? local.value?.path : remote.value?.path
  if (!dir) return
  const name = window.prompt('新目录名', '新建文件夹')
  if (!name) return
  const sep = side === 'local' && dir.includes('\\') ? '\\' : '/'
  const target = dir.endsWith(sep) ? dir + name : dir + sep + name
  try {
    if (side === 'local') await api.localMkdir(target)
    else await api.sftpMkdir(props.sid, target)
    side === 'local' ? void loadLocal(dir) : void loadRemote(dir)
  } catch (e) {
    err.value = toSshboxError(e).message
  }
}

async function ren(side: Side) {
  const sel = selList(side).value
  if (sel.length !== 1) return
  const from = sel[0]
  const name = window.prompt('新名字', baseName(from))
  if (!name || name === baseName(from)) return
  const parent = from.slice(0, from.length - baseName(from).length)
  const to = parent + name
  try {
    if (side === 'local') await api.localRename(from, to)
    else await api.sftpRename(props.sid, from, to)
    side === 'local' ? void loadLocal(local.value?.path) : void loadRemote(remote.value?.path)
  } catch (e) {
    err.value = toSshboxError(e).message
  }
}

function askDelete(side: Side) {
  const paths = [...selList(side).value]
  if (!paths.length) return
  const entries = entriesOf(side).filter((e) => paths.includes(e.path))
  pendingDelete.value = {
    side,
    paths,
    hasDir: entries.some((e) => e.is_dir),
    recursive: false,
  }
}

async function doDelete() {
  const job = pendingDelete.value
  if (!job) return
  pendingDelete.value = null
  const dirs: Record<string, boolean> = {}
  for (const e of entriesOf(job.side)) {
    if (job.paths.includes(e.path) && e.is_dir) dirs[e.path] = true
  }
  for (const p of job.paths) {
    try {
      if (job.side === 'local') await api.localRemove(p, !!dirs[p], job.recursive)
      else await api.sftpRemove(props.sid, p, !!dirs[p], job.recursive)
    } catch (e) {
      err.value = toSshboxError(e).message
      break
    }
  }
  job.side === 'local' ? void loadLocal(local.value?.path) : void loadRemote(remote.value?.path)
}

// --- 键盘 -----------------------------------------------------------------

function onKey(ev: KeyboardEvent) {
  const t = ev.target as HTMLElement | null
  const typing = t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA')
  if (ev.key === 'Escape') {
    if (conflict.value) conflict.value.resolve('skip')
    else if (pendingDelete.value) pendingDelete.value = null
    else emit('close')
    return
  }
  if (typing) return
  if (ev.key === 'F5') {
    ev.preventDefault()
    refreshAll()
  } else if (ev.key === 'Backspace') {
    goUp('local')
  } else if (ev.key === 'Delete') {
    if (localSel.value.length) askDelete('local')
    else if (remoteSel.value.length) askDelete('remote')
  }
}

// 传输完成后自动刷新对应一侧的列表——否则文件明明传上去了，列表还是旧的，
// 用户会以为失败又点一次。
const doneSeen = new Set<string>()
watch(
  items,
  (list) => {
    for (const t of list) {
      if (t.state === 'done' && !doneSeen.has(t.task_id)) {
        doneSeen.add(t.task_id)
        if (t.direction === 'up') void loadRemote(remote.value?.path)
        else void loadLocal(local.value?.path)
        break
      }
    }
  },
  { deep: true },
)

/** 从资源管理器拖进来的文件：上传到远端当前目录（App.vue 写 pendingDrop） */
watch(pendingDrop, async (paths) => {
  if (!paths.length || !remote.value) return
  pendingDrop.value = []
  let kinds: string[] = []
  try {
    kinds = await api.localKinds(paths)
  } catch {
    kinds = paths.map(() => 'file')
  }
  const dir = remote.value.path
  const jobs: Job[] = paths
    .map((p, i) => ({
      side: 'local' as Side,
      localPath: p,
      remotePath: dir.endsWith('/') ? dir + baseName(p) : `${dir}/${baseName(p)}`,
      isDir: kinds[i] === 'dir',
    }))
    .filter((_, i) => kinds[i] !== 'missing')
  if (jobs.length) startJobs(jobs)
})

onMounted(() => {
  window.addEventListener('keydown', onKey)
  void loadLocal(local.value?.path)
  void loadRemote(remote.value?.path)
  void pump()
  if (pendingDrop.value.length) {
    const pending = [...pendingDrop.value]
    pendingDrop.value = []
    // 面板是后打开的：把已经在队列里的拖入文件接过来
    void api
      .localKinds(pending)
      .catch(() => pending.map(() => 'file'))
      .then((kinds) => {
        const dir = remote.value?.path
        if (!dir) return
        startJobs(
          pending
            .map((p, i) => ({
              side: 'local' as Side,
              localPath: p,
              remotePath: dir.endsWith('/') ? dir + baseName(p) : `${dir}/${baseName(p)}`,
              isDir: kinds[i] === 'dir',
            }))
            .filter((_, i) => kinds[i] !== 'missing'),
        )
      })
  }
})

onUnmounted(() => {
  window.removeEventListener('keydown', onKey)
})
</script>

<style scoped>
.mask {
  position: fixed;
  inset: 0;
  background: var(--mask);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 50;
}

.panel {
  width: min(1240px, 96vw);
  height: min(780px, 92vh);
  background: var(--ctp-base);
  border: 1px solid var(--ctp-surface1);
  border-radius: 10px;
  display: flex;
  flex-direction: column;
  overflow: hidden;
  box-shadow: 0 18px 48px rgb(0 0 0 / 45%);
}

.head {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 10px 14px;
  background: var(--ctp-mantle);
  border-bottom: 1px solid var(--ctp-surface0);
}

.title {
  font-weight: 600;
  color: var(--ctp-text);
}

.host {
  font-size: 12px;
  color: var(--ctp-subtext0);
  max-width: 40ch;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.spacer {
  flex: 1;
}

.icon {
  background: var(--ctp-surface0);
  color: var(--ctp-text);
  border: 1px solid var(--ctp-surface1);
  border-radius: 6px;
  padding: 3px 8px;
  cursor: pointer;
  font-size: 12px;
}

.icon:disabled {
  opacity: 0.45;
  cursor: default;
}

.err {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 7px 14px;
  background: var(--banner-error-bg);
  color: var(--ctp-text);
  font-size: 12px;
}

.panes {
  flex: 1;
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 1px;
  background: var(--ctp-surface0);
  min-height: 0;
}

.pane {
  display: flex;
  flex-direction: column;
  background: var(--ctp-base);
  min-height: 0;
}

.pane-head {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 8px 10px;
  border-bottom: 1px solid var(--ctp-surface0);
}

.tag {
  font-size: 12px;
  color: var(--ctp-text);
  background: var(--ctp-surface0);
  border-radius: 4px;
  padding: 2px 6px;
}

.path {
  flex: 1;
  min-width: 0;
  background: var(--ctp-crust);
  color: var(--ctp-text);
  border: 1px solid var(--ctp-surface1);
  border-radius: 6px;
  padding: 4px 8px;
  font-size: 12px;
  font-family: ui-monospace, Consolas, monospace;
}

.drives {
  background: var(--ctp-surface0);
  color: var(--ctp-text);
  border: 1px solid var(--ctp-surface1);
  border-radius: 6px;
  font-size: 12px;
  padding: 3px;
}

.list {
  flex: 1;
  overflow: auto;
  margin: 0;
  padding: 4px 0;
  list-style: none;
  min-height: 0;
}

.list li {
  color: var(--ctp-text);
  display: grid;
  grid-template-columns: 20px 1fr auto auto;
  gap: 8px;
  align-items: center;
  padding: 4px 10px;
  cursor: default;
  font-size: 12.5px;
  color: var(--ctp-text);
}

.list li:hover {
  background: var(--ctp-surface0);
}

.list li.sel {
  background: var(--ctp-blue);
  color: var(--on-accent);
}

.list li.dim .nm {
  color: var(--ctp-overlay0);
}

.nm {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.sz,
.tm {
  /* 11px 的次要信息再降一档；文件名是主文字色，层级靠这行拉开 */
  color: var(--ctp-overlay0);
  font-size: 11px;
  font-variant-numeric: tabular-nums;
}

.list li.sel .sz,
.list li.sel .tm {
  color: var(--on-accent);
}

.empty {
  color: var(--ctp-overlay0);
  padding: 10px;
  display: block;
}

.pane-foot {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 8px 10px;
  border-top: 1px solid var(--ctp-surface0);
}

.pane-foot button {
  background: var(--ctp-surface0);
  color: var(--ctp-text);
  border: 1px solid var(--ctp-surface1);
  border-radius: 6px;
  padding: 4px 10px;
  cursor: pointer;
  font-size: 12px;
}

.pane-foot button.primary {
  background: var(--ctp-blue);
  color: var(--on-accent);
  border-color: var(--ctp-blue);
}

.pane-foot button:disabled {
  opacity: 0.45;
  cursor: default;
}

.queue {
  max-height: 30%;
  overflow: auto;
  border-top: 1px solid var(--ctp-surface0);
  background: var(--ctp-mantle);
}

.q-head {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 7px 12px;
  font-size: 12px;
  color: var(--ctp-text);
  border-bottom: 1px solid var(--ctp-surface0);
}

.muted {
  color: var(--ctp-overlay0);
}

.q-row {
  display: grid;
  grid-template-columns: 16px minmax(120px, 1fr) 180px auto auto;
  gap: 10px;
  align-items: center;
  padding: 6px 12px;
  font-size: 12px;
  color: var(--ctp-text);
}

.bar {
  height: 6px;
  background: var(--ctp-surface0);
  border-radius: 3px;
  overflow: hidden;
}

.fill {
  height: 100%;
  background: var(--ctp-blue);
  transition: width 0.2s linear;
}

.fill.done {
  background: var(--ctp-green);
}

.fill.error {
  background: var(--ctp-red);
}

.fill.cancelled {
  background: var(--ctp-yellow);
}

.meta {
  color: var(--ctp-subtext0);
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}

.dialog {
  position: fixed;
  inset: 0;
  background: rgb(0 0 0 / 45%);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 60;
}

.dlg-box {
  width: min(420px, 90vw);
  background: var(--ctp-base);
  border: 1px solid var(--ctp-surface1);
  border-radius: 10px;
  padding: 16px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.dlg-title {
  font-weight: 600;
  color: var(--ctp-text);
}

.dlg-body {
  font-size: 12.5px;
  color: var(--ctp-text);
  max-height: 30vh;
  overflow: auto;
}

.mono {
  font-family: ui-monospace, Consolas, monospace;
  font-size: 12px;
}

.chk {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12.5px;
  color: var(--ctp-text);
}

.dlg-foot {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}

.dlg-foot button {
  background: var(--ctp-surface0);
  color: var(--ctp-text);
  border: 1px solid var(--ctp-surface1);
  border-radius: 6px;
  padding: 5px 12px;
  cursor: pointer;
  font-size: 12.5px;
}

.dlg-foot button.primary {
  background: var(--ctp-blue);
  color: var(--on-accent);
  border-color: var(--ctp-blue);
}

.dlg-foot button.danger {
  background: var(--ctp-red);
  color: var(--on-accent);
  border-color: var(--ctp-red);
}
</style>
