<template>
  <div class="mask">
    <div class="panel">
      <header class="head">
        <span class="title">端口转发</span>
        <span class="host" :title="label">{{ label }}</span>
        <span class="spacer" />
        <button class="icon" title="刷新" @click="load">⟳</button>
        <button class="icon" title="关闭（Esc）" @click="emit('close')">×</button>
      </header>

      <div v-if="err" class="err">
        <span>{{ err }}</span>
        <button @click="err = ''">×</button>
      </div>

      <div class="hint">
        <b>本地转发</b>：本机端口 → 远端能访问的地址（连远端内网的 MySQL/Redis）。
        <b>远程转发</b>：远端端口 → 本机地址（把本机服务暴露给远端）。
        规则跟着主机保存，导入导出主机时会一起带走。
      </div>

      <div class="body">
        <table class="rules">
          <thead>
            <tr>
              <th class="st">状态</th>
              <th>类型</th>
              <th>监听</th>
              <th>目标</th>
              <th>备注</th>
              <th class="ops">操作</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="r in rules" :key="r.id" :class="{ bad: !!r.error }">
              <td class="st">
                <span class="dot" :class="r.running ? 'on' : 'off'" />
                <span class="st-text">{{ r.running ? '运行中' : '已停止' }}</span>
              </td>
              <td>
                <span class="kind" :class="r.kind">{{ r.kind === 'local' ? '本地 -L' : '远程 -R' }}</span>
              </td>
              <td class="mono">
                {{ r.listen_host }}:{{ r.running && r.actual_port !== r.listen_port ? r.actual_port : r.listen_port }}
                <span v-if="r.kind === 'local'" class="muted">（本机）</span>
                <span v-else class="muted">（远端）</span>
                <!-- sshd 的 GatewayPorts 默认是 no：请求 0.0.0.0 时它静默只绑回环。
                     不说明的话，面板写着 0.0.0.0 会让人以为外部机器能连。 -->
                <div
                  v-if="r.kind === 'remote' && r.running && isWildcard(r.listen_host)"
                  class="warn-note"
                >
                  远端 sshd 的 GatewayPorts 若不是 yes，实际只绑回环地址 —— 只有远端本机能连
                </div>
              </td>
              <td class="mono">
                {{ r.target_host }}:{{ r.target_port }}
                <span v-if="r.kind === 'local'" class="muted">（远端）</span>
                <span v-else class="muted">（本机）</span>
              </td>
              <td class="note">
                {{ r.label || '—' }}
                <span v-if="r.auto_start" class="tag-auto">自动启动</span>
                <div v-if="r.error" class="row-err">{{ r.error }}</div>
              </td>
              <td class="ops">
                <button v-if="!r.running" class="icon" :disabled="busy" @click="start(r)">启动</button>
                <button v-else class="icon" :disabled="busy" @click="stop(r)">停止</button>
                <button class="icon" :disabled="busy || r.running" @click="edit(r)">编辑</button>
                <button class="icon danger" :disabled="busy" @click="remove(r)">删除</button>
              </td>
            </tr>
            <tr v-if="!rules.length">
              <td colspan="6" class="empty">还没有规则 —— 用下面的表单加一条</td>
            </tr>
          </tbody>
        </table>

        <div class="form">
          <div class="form-title">{{ draft.id ? '编辑规则' : '新增规则' }}</div>
          <div class="form-row">
            <label>
              类型
              <select v-model="draft.kind">
                <option value="local">本地转发（-L：本机 → 远端）</option>
                <option value="remote">远程转发（-R：远端 → 本机）</option>
              </select>
            </label>
            <label>
              {{ draft.kind === 'local' ? '本机监听地址' : '远端监听地址' }}
              <input v-model="draft.listen_host" spellcheck="false" placeholder="127.0.0.1" />
            </label>
            <label>
              端口
              <input v-model.number="draft.listen_port" type="number" min="1" max="65535" />
            </label>
          </div>
          <div class="form-row">
            <label>
              {{ draft.kind === 'local' ? '远端目标地址' : '本机目标地址' }}
              <input v-model="draft.target_host" spellcheck="false" placeholder="127.0.0.1" />
            </label>
            <label>
              端口
              <input v-model.number="draft.target_port" type="number" min="1" max="65535" />
            </label>
            <label>
              备注
              <input v-model="draft.label" spellcheck="false" placeholder="比如 VM 里的 MySQL" />
            </label>
            <label class="chk">
              <input v-model="draft.auto_start" type="checkbox" />
              连上后自动启动
            </label>
          </div>
          <div class="form-foot">
            <span class="muted">监听端口填 0 表示让系统自动分配</span>
            <span class="spacer" />
            <button v-if="draft.id" class="icon" @click="resetDraft">取消编辑</button>
            <button class="primary" :disabled="busy" @click="save">
              {{ draft.id ? '保存修改' : '添加规则' }}
            </button>
            <button class="primary" :disabled="busy" @click="saveAndStart">
              保存并启动
            </button>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
/**
 * 端口转发管理（全窗口浮层）。
 *
 * 规则持久化在 hosts.json 的 host 上，所以这条会话必须是从主机列表连的
 * （临时手输的会话没有 host_id，后端会明确报错，这里直接提示）。
 */
import { onMounted, onUnmounted, ref } from 'vue'
import { api, toSshboxError, type ForwardKind, type ForwardRule, type ForwardStatus } from '../api'

const props = defineProps<{ sid: string; label: string }>()
const emit = defineEmits<{ (e: 'close'): void }>()

/** 监听地址是不是"全网卡"（这类地址对远端 -R 会被 sshd 的 GatewayPorts 影响） */
function isWildcard(host: string): boolean {
  return host === '0.0.0.0' || host === '*' || host === '::' || host === '[::]'
}

const rules = ref<ForwardStatus[]>([])
const err = ref('')
const busy = ref(false)

function blankDraft(): ForwardRule {
  return {
    id: '',
    kind: 'local',
    listen_host: '127.0.0.1',
    listen_port: 13306,
    target_host: '127.0.0.1',
    target_port: 3306,
    auto_start: false,
    label: '',
  }
}
const draft = ref<ForwardRule>(blankDraft())

async function load() {
  try {
    rules.value = await api.forwardList(props.sid)
  } catch (e) {
    err.value = toSshboxError(e).message
  }
}

function resetDraft() {
  draft.value = blankDraft()
}

function edit(r: ForwardStatus) {
  draft.value = {
    id: r.id,
    kind: r.kind,
    listen_host: r.listen_host,
    listen_port: r.listen_port,
    target_host: r.target_host,
    target_port: r.target_port,
    auto_start: r.auto_start,
    label: r.label ?? '',
  }
}

async function save(): Promise<ForwardRule | null> {
  if (!draft.value.listen_port || !draft.value.target_port) {
    err.value = '端口必须填 1-65535（监听端口 0 表示自动分配）'
    return null
  }
  busy.value = true
  try {
    const rule: ForwardRule = {
      ...draft.value,
      id: draft.value.id || crypto.randomUUID(),
      kind: draft.value.kind as ForwardKind,
      label: draft.value.label || null,
    }
    await api.forwardSave(props.sid, rule)
    draft.value = rule
    await load()
    return rule
  } catch (e) {
    err.value = toSshboxError(e).message
    return null
  } finally {
    busy.value = false
  }
}

async function saveAndStart() {
  const rule = await save()
  if (rule) await start(rule as ForwardStatus)
  resetDraft()
}

async function start(r: ForwardStatus) {
  busy.value = true
  try {
    const port = await api.forwardStart(props.sid, {
      id: r.id,
      kind: r.kind,
      listen_host: r.listen_host,
      listen_port: r.listen_port,
      target_host: r.target_host,
      target_port: r.target_port,
      auto_start: r.auto_start,
      label: r.label ?? null,
    })
    err.value = ''
    await load()
    if (port && port !== r.listen_port) {
      err.value = `已启动：${r.listen_host}:${port} → ${r.target_host}:${r.target_port}`
    }
  } catch (e) {
    err.value = toSshboxError(e).message
    await load()
  } finally {
    busy.value = false
  }
}

async function stop(r: ForwardStatus) {
  busy.value = true
  try {
    await api.forwardStop(props.sid, r.id)
    await load()
  } catch (e) {
    err.value = toSshboxError(e).message
  } finally {
    busy.value = false
  }
}

async function remove(r: ForwardStatus) {
  if (!window.confirm(`删除规则 ${r.listen_host}:${r.listen_port} → ${r.target_host}:${r.target_port}？`)) return
  busy.value = true
  try {
    await api.forwardDelete(props.sid, r.id)
    await load()
  } catch (e) {
    err.value = toSshboxError(e).message
  } finally {
    busy.value = false
  }
}

function onKey(ev: KeyboardEvent) {
  if (ev.key === 'Escape') emit('close')
}

onMounted(() => {
  window.addEventListener('keydown', onKey)
  void load()
})

onUnmounted(() => window.removeEventListener('keydown', onKey))
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
  width: min(1040px, 94vw);
  max-height: 88vh;
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
  padding: 3px 9px;
  cursor: pointer;
  font-size: 12px;
}

.icon:disabled {
  opacity: 0.45;
  cursor: default;
}

.icon.danger {
  color: var(--ctp-red);
}

.icon.primary,
.primary {
  background: var(--ctp-blue);
  color: var(--on-accent);
  border: 1px solid var(--ctp-blue);
  border-radius: 6px;
  padding: 4px 12px;
  cursor: pointer;
  font-size: 12.5px;
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

.hint {
  padding: 8px 14px;
  font-size: 12px;
  color: var(--ctp-subtext0);
  background: var(--banner-info-bg);
  line-height: 1.7;
}

.body {
  overflow: auto;
  padding: 12px 14px;
  display: flex;
  flex-direction: column;
  gap: 14px;
}

.rules {
  width: 100%;
  border-collapse: collapse;
  font-size: 12.5px;
}

.rules th {
  text-align: left;
  color: var(--ctp-subtext0);
  font-weight: 500;
  padding: 6px 8px;
  border-bottom: 1px solid var(--ctp-surface0);
}

.rules td {
  padding: 7px 8px;
  border-bottom: 1px solid var(--ctp-surface0);
  color: var(--ctp-text);
  vertical-align: top;
}

.rules tr.bad td {
  background: var(--banner-error-bg);
}

.rules .st {
  width: 90px;
}

.rules .ops {
  width: 230px;
  white-space: nowrap;
}

.dot {
  display: inline-block;
  width: 8px;
  height: 8px;
  border-radius: 50%;
  margin-right: 5px;
}

.dot.on {
  background: var(--ctp-green);
}

.dot.off {
  background: var(--ctp-overlay0);
}

.st-text {
  font-size: 11.5px;
  color: var(--ctp-subtext0);
}

.kind {
  font-size: 11.5px;
  padding: 2px 6px;
  border-radius: 4px;
}

.kind.local {
  background: var(--ctp-blue);
  color: var(--on-accent);
}

.kind.remote {
  background: var(--ctp-mauve);
  color: var(--on-accent);
}

.mono {
  font-family: ui-monospace, Consolas, monospace;
  font-size: 12px;
}

.warn-note {
  margin-top: 3px;
  font-family: var(--font-ui, inherit);
  font-size: 10.5px;
  line-height: 1.3;
  color: var(--ctp-yellow);
}

.muted {
  color: var(--ctp-overlay0);
  font-size: 11px;
}

.note {
  color: var(--ctp-subtext0);
}

.tag-auto {
  margin-left: 6px;
  font-size: 10.5px;
  padding: 1px 5px;
  border-radius: 4px;
  background: var(--ctp-surface1);
  color: var(--ctp-subtext0);
}

.row-err {
  margin-top: 4px;
  color: var(--ctp-red);
  font-size: 11.5px;
}

.empty {
  text-align: center;
  color: var(--ctp-overlay0);
  padding: 18px;
}

.form {
  border: 1px solid var(--ctp-surface0);
  border-radius: 8px;
  padding: 12px;
  background: var(--ctp-mantle);
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.form-title {
  font-size: 12.5px;
  font-weight: 600;
  color: var(--ctp-text);
}

.form-row {
  display: flex;
  flex-wrap: wrap;
  gap: 12px;
}

.form-row label {
  display: flex;
  flex-direction: column;
  gap: 4px;
  font-size: 11.5px;
  color: var(--ctp-subtext0);
}

.form-row label.chk {
  flex-direction: row;
  align-items: center;
  gap: 6px;
  align-self: flex-end;
  padding-bottom: 6px;
}

.form-row input[type='text'],
.form-row input:not([type]),
.form-row input[type='number'],
.form-row select {
  background: var(--ctp-crust);
  color: var(--ctp-text);
  border: 1px solid var(--ctp-surface1);
  border-radius: 6px;
  padding: 5px 8px;
  font-size: 12px;
  min-width: 150px;
}

.form-row input[type='number'] {
  min-width: 90px;
  width: 90px;
}

.form-foot {
  display: flex;
  align-items: center;
  gap: 8px;
}
</style>
