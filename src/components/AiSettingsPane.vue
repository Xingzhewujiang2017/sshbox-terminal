<script setup lang="ts">
/**
 * 设置 → AI 模型：多套模型接入点的增删改 + 密钥 + 测试连接。
 *
 * 密钥**不经过这个组件回传**：填了就直接写进系统凭据管理器，之后只问后端「有没有」。
 * 所以这里的 `key` 只用于新输入，界面上的「已保存」状态来自 `has_key`。
 */
import { computed, onMounted, ref } from 'vue'
import { api, type AiProfile, type AiSettings } from '../api'
import { loadAiSettings, needsKey } from '../ai'

const emit = defineEmits<{ (e: 'changed'): void }>()

const settings = ref<AiSettings | null>(null)
const editing = ref<AiProfile | null>(null)
const keyInput = ref('')
const busy = ref('')
const msg = ref('')
const err = ref('')

const PROTOCOLS = [
  { v: 'openai', t: 'OpenAI 兼容（Ollama / DeepSeek / SiliconFlow / 自定义）' },
  { v: 'anthropic', t: 'Anthropic（Claude 原生协议）' },
  { v: 'gemini', t: 'Google Gemini（原生协议）' },
]

const active = computed(() =>
  settings.value?.profiles.find((p) => p.id === settings.value?.active_profile_id) ?? null,
)

/**
 * base_url 的示例随协议变。
 *
 * 写错的代价是一个 404，而 Anthropic 的 404 页面不告诉用户"你少了 /v1" ——
 * 实测有人把 base_url 填成 https://api.anthropic.com，请求就打到了 /messages（正确是 /v1/messages）。
 */
const urlExample = computed(() =>
  editing.value?.protocol === 'anthropic'
    ? 'https://api.anthropic.com/v1'
    : editing.value?.protocol === 'gemini'
      ? 'https://generativelanguage.googleapis.com/v1beta'
      : 'https://api.deepseek.com/v1',
)

const urlHint = computed(() =>
  editing.value?.protocol === 'anthropic'
    ? 'Anthropic：填到 /v1 为止（不要带 /messages），大陆直连通常需要代理'
    : editing.value?.protocol === 'gemini'
      ? 'Gemini：填到 /v1beta 为止（模型名会拼进路径），大陆直连通常需要代理'
      : 'OpenAI 兼容：填到 /v1 为止（不要带 /chat/completions）',
)

const modelExample = computed(() =>
  editing.value?.protocol === 'anthropic'
    ? 'claude-3-5-sonnet-latest'
    : editing.value?.protocol === 'gemini'
      ? 'gemini-2.0-flash'
      : 'deepseek-chat',
)

async function load() {
  try {
    settings.value = await api.aiSettings()
    await loadAiSettings()
  } catch (e) {
    err.value = `读取失败：${(e as Error).message}`
  }
}
onMounted(load)

function startNew() {
  err.value = ''
  msg.value = ''
  editing.value = {
    id: '',
    name: '',
    protocol: 'openai',
    base_url: 'http://127.0.0.1:11434/v1',
    model: '',
    temperature: 0.2,
    // 与后端 default_max_tokens() 对齐：1024 会被推理模型的思考吃光
    max_tokens: 4096,
    has_key: false,
  }
  keyInput.value = ''
}

function startEdit(p: AiProfile) {
  err.value = ''
  msg.value = ''
  editing.value = { ...p }
  keyInput.value = ''
}

function cancelEdit() {
  editing.value = null
  keyInput.value = ''
}

async function save() {
  const p = editing.value
  if (!p) return
  if (!p.base_url.trim() || !p.model.trim()) {
    err.value = 'base_url 和模型名都要填'
    return
  }
  busy.value = 'save'
  err.value = ''
  try {
    settings.value = await api.aiProfileSave(p, keyInput.value.trim() || undefined)
    await loadAiSettings()
    editing.value = null
    keyInput.value = ''
    msg.value = `已保存「${p.name || p.model}」`
    emit('changed')
  } catch (e) {
    err.value = (e as Error).message
  } finally {
    busy.value = ''
  }
}

async function remove(p: AiProfile) {
  busy.value = 'del-' + p.id
  err.value = ''
  try {
    settings.value = await api.aiProfileDelete(p.id)
    await loadAiSettings()
    msg.value = `已删除「${p.name}」及其密钥`
    emit('changed')
  } catch (e) {
    err.value = (e as Error).message
  } finally {
    busy.value = ''
  }
}

async function use(p: AiProfile) {
  busy.value = 'use-' + p.id
  err.value = ''
  try {
    settings.value = await api.aiSetActive(p.id)
    await loadAiSettings()
    msg.value = `已切换到「${p.name}」`
    emit('changed')
  } catch (e) {
    err.value = (e as Error).message
  } finally {
    busy.value = ''
  }
}

async function test(p: AiProfile) {
  busy.value = 'test-' + p.id
  err.value = ''
  msg.value = ''
  try {
    msg.value = await api.aiTest(p.id)
  } catch (e) {
    err.value = (e as Error).message
  } finally {
    busy.value = ''
  }
}

async function dropKey(p: AiProfile) {
  try {
    await api.aiKeyDelete(p.id)
    settings.value = await api.aiSettings()
    await loadAiSettings()
    msg.value = `已删除「${p.name}」的密钥`
    emit('changed')
  } catch (e) {
    err.value = (e as Error).message
  }
}

async function restorePresets() {
  try {
    const list = await api.aiPresets()
    // 预设以「补齐缺失项」的方式合并：不动已填好的配置和密钥
    for (const p of list) {
      if (!settings.value?.profiles.some((x) => x.id === p.id)) {
        settings.value = await api.aiProfileSave(p)
      }
    }
    await loadAiSettings()
    msg.value = '预设已补齐'
    emit('changed')
  } catch (e) {
    err.value = (e as Error).message
  }
}

async function setExplain(id: string) {
  if (!settings.value) return
  try {
    settings.value = await api.aiSetExplainProfile(id)
    emit('changed')
  } catch (e) {
    err.value = (e as Error).message
  }
}

async function toggleSummary() {
  if (!settings.value) return
  const next = !settings.value.report_ai_summary
  await api.aiSetReportSummary(next)
  settings.value = { ...settings.value, report_ai_summary: next }
  emit('changed')
}
</script>

<template>
  <div class="pane">
    <div class="lead">
      配好几套模型接入点，随时切换。<b>密钥存在 Windows 凭据管理器里</b>，
      不会写进 <code>settings.json</code>，也不会出现在日志或巡检报告里。
    </div>

    <div v-if="err" class="alert err">{{ err }}</div>
    <div v-else-if="msg" class="alert ok">{{ msg }}</div>

    <div class="list">
      <div v-for="p in settings?.profiles ?? []" :key="p.id" class="row" :class="{ on: p.id === settings?.active_profile_id }">
        <div class="info">
          <div class="line1">
            <span class="name">{{ p.name || p.model }}</span>
            <span v-if="p.id === settings?.active_profile_id" class="tag on">当前使用</span>
            <span class="tag">{{ PROTOCOLS.find((x) => x.v === p.protocol)?.t.split('（')[0] ?? p.protocol }}</span>
            <span v-if="p.has_key" class="tag key">已存密钥</span>
            <span v-else-if="needsKey(p)" class="tag nokey">缺密钥</span>
          </div>
          <div class="line2 mono">{{ p.base_url }} · {{ p.model }}</div>
        </div>
        <div class="acts">
          <button v-if="p.id !== settings?.active_profile_id" class="mini" :disabled="!!busy" @click="use(p)">使用</button>
          <button class="mini" :disabled="!!busy" @click="test(p)">
            {{ busy === 'test-' + p.id ? '测试中…' : '测试连接' }}
          </button>
          <button class="mini" :disabled="!!busy" @click="startEdit(p)">编辑</button>
          <button v-if="p.has_key" class="mini" :disabled="!!busy" @click="dropKey(p)">删密钥</button>
          <button class="mini danger" :disabled="!!busy" @click="remove(p)">删除</button>
        </div>
      </div>
      <div v-if="!settings?.profiles.length" class="empty">还没有任何模型配置</div>
    </div>

    <div class="row-actions">
      <button class="mini" @click="startNew">＋ 添加自定义接入点</button>
      <button class="mini" @click="restorePresets">补齐预设（Ollama / SiliconFlow）</button>
    </div>

    <!-- 编辑表单 -->
    <div v-if="editing" class="form">
      <div class="form-title">{{ editing.id ? '编辑接入点' : '新建接入点' }}</div>
      <div class="grid2">
        <label>
          名称
          <input v-model="editing.name" placeholder="例如 本地 Ollama" />
        </label>
        <label>
          协议
          <select v-model="editing.protocol">
            <option v-for="x in PROTOCOLS" :key="x.v" :value="x.v">{{ x.t }}</option>
          </select>
        </label>
      </div>
      <label>
        Base URL
        <input v-model="editing.base_url" :placeholder="urlExample" />
      </label>
      <div class="hintline">{{ urlHint }}</div>
      <div class="grid2">
        <label>
          模型名
          <input v-model="editing.model" :placeholder="modelExample" />
        </label>
        <label>
          API 密钥
          <input
            v-model="keyInput"
            type="password"
            :placeholder="editing.has_key ? '已保存，留空则不修改' : needsKey(editing) ? 'sk-…' : '本地端点可留空'"
          />
        </label>
      </div>
      <div class="grid2">
        <label>
          temperature
          <input v-model.number="editing.temperature" type="number" step="0.1" min="0" max="2" />
        </label>
        <label>
          max_tokens
          <input v-model.number="editing.max_tokens" type="number" min="64" max="32000" />
        </label>
      </div>
      <div class="hintline">
        推理模型（Qwen3 / DeepSeek-R1 等）的思考也占这份额度：1024 常被思考吃光，正文一个字不剩。建议 ≥4096。
      </div>
      <div class="form-acts">
        <button class="mini" @click="cancelEdit">取消</button>
        <button class="mini primary" :disabled="busy === 'save'" @click="save">
          {{ busy === 'save' ? '保存中…' : '保存' }}
        </button>
      </div>
    </div>

    <label class="slot">
      解释这段用哪个模型
      <select
        :value="settings?.explain_profile_id ?? ''"
        :disabled="!!busy"
        @change="setExplain(($event.target as HTMLSelectElement).value)"
      >
        <option value="">跟「当前使用」的一样</option>
        <option v-for="p in settings?.profiles ?? []" :key="p.id" :value="p.id">
          {{ p.name || p.model }}
        </option>
      </select>
    </label>
    <div class="hintline">
      「解释这段」是高频又便宜的任务，可以单独丢给本地小模型；生成命令与报告仍用「当前使用」那个。
    </div>

    <label class="check">
      <input
        type="checkbox"
        :checked="settings?.report_ai_summary ?? false"
        @change="toggleSummary"
      />
      <span>巡检报告里附一段 AI 写的结论（关掉后完全由规则生成）</span>
    </label>

    <div class="note">
      提示：<b>本地 Ollama</b> 不需要密钥，但要先拉过模型（<code>ollama pull qwen2.5:7b</code>）；
      「测试连接」会真发一条极短请求，能测出密钥、地址和模型名三件事对不对。
      <span v-if="active">当前：{{ active.name }} · {{ active.model }}</span>
    </div>
  </div>
</template>

<style scoped>
.pane { display: flex; flex-direction: column; gap: 10px; }
.hintline { font-size: 11px; color: var(--ctp-overlay0); line-height: 1.6; }
.slot { font-size: 11px; color: var(--ctp-subtext0); display: flex; flex-direction: column; gap: 3px; }
.slot select { padding: 3px 6px; }
.lead { font-size: 11.5px; color: var(--ctp-subtext0); line-height: 1.6; }
.lead code, .note code { background: var(--ctp-crust); padding: 1px 4px; border-radius: 3px; }
.alert { font-size: 11.5px; padding: 6px 8px; border-radius: 5px; line-height: 1.5; }
.alert.err { color: var(--ctp-red); background: var(--banner-warn-bg); }
.alert.ok { color: var(--ctp-green); background: var(--ctp-surface0); }
.list { display: flex; flex-direction: column; gap: 6px; }
.row {
  display: flex; align-items: center; gap: 8px;
  background: var(--ctp-crust); border: 1px solid var(--ctp-surface0);
  border-radius: 6px; padding: 7px 9px;
}
.row.on { border-color: var(--ctp-blue); }
.info { flex: 1; min-width: 0; }
.line1 { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
.name { font-size: 12.5px; color: var(--ctp-text); font-weight: 600; }
.tag { font-size: 10px; color: var(--ctp-overlay0); border: 1px solid var(--ctp-surface1); border-radius: 3px; padding: 0 4px; }
.tag.on { color: var(--ctp-blue); border-color: var(--ctp-blue); }
.tag.key { color: var(--ctp-green); border-color: var(--ctp-surface2); }
.tag.nokey { color: var(--ctp-yellow); border-color: var(--ctp-surface2); }
.line2 { font-size: 10.5px; color: var(--ctp-overlay0); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.mono { font-family: ui-monospace, monospace; }
.acts { display: flex; gap: 4px; flex-shrink: 0; }
.mini {
  background: var(--ctp-base); border: 1px solid var(--ctp-surface1); color: var(--ctp-subtext0);
  border-radius: 4px; font-size: 11px; padding: 3px 7px; cursor: pointer;
}
.mini:hover:not(:disabled) { border-color: var(--ctp-surface2); color: var(--ctp-text); }
.mini:disabled { opacity: 0.5; cursor: default; }
.mini.primary { background: var(--ctp-blue); color: var(--on-accent); border: none; font-weight: 600; }
.mini.danger { color: var(--ctp-red); }
.empty { font-size: 11px; color: var(--ctp-surface2); padding: 6px 2px; }
.row-actions { display: flex; gap: 6px; }
.form { background: var(--ctp-crust); border: 1px solid var(--ctp-surface1); border-radius: 6px; padding: 10px; display: flex; flex-direction: column; gap: 6px; }
.form-title { font-size: 12px; color: var(--ctp-text); font-weight: 600; }
.grid2 { display: grid; grid-template-columns: 1fr 1fr; gap: 8px; }
label { font-size: 11px; color: var(--ctp-subtext0); display: flex; flex-direction: column; gap: 3px; }
input, select {
  background: var(--ctp-mantle); border: 1px solid var(--ctp-surface0); border-radius: 5px;
  color: var(--ctp-text); font-family: inherit; font-size: 12px; padding: 5px 7px; width: 100%; box-sizing: border-box;
}
input:focus, select:focus { outline: 1px solid var(--ctp-blue); }
.form-acts { display: flex; gap: 6px; justify-content: flex-end; }
.check { flex-direction: row; align-items: center; gap: 7px; }
.check input { width: auto; }
.note { font-size: 10.5px; color: var(--ctp-overlay0); line-height: 1.6; }
</style>
