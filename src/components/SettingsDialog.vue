<script setup lang="ts">
import { onMounted, ref, watch } from 'vue'
import type { AppPaths, KnownHostEntry, Settings } from '../api'
import type { ThemeMode } from '../theme'
import AiSettingsPane from './AiSettingsPane.vue'
import { getVersion } from '@tauri-apps/api/app'

/** 版本号从打包信息读，不在前端硬编码 —— 免得"关于页写的版本"和"装出来的版本"不一致。 */
const version = ref('…')
const versionSource = ref('读取中…')
onMounted(async () => {
  try {
    version.value = await getVersion()
    versionSource.value = '来自打包信息'
  } catch (e) {
    version.value = '未知'
    versionSource.value = `读取失败：${(e as Error).message}`
  }
})

const props = defineProps<{
  settings: Settings
  paths: AppPaths | null
  knownHosts: KnownHostEntry[]
  hostCount: number
}>()

const emit = defineEmits<{
  (e: 'save', settings: Settings): void
  (e: 'close'): void
  (e: 'removeKnownHost', host: string, port: number): void
  (e: 'exportHosts', path: string, includeSecrets: boolean): void
  (e: 'importHosts', path: string): void
  (e: 'restartMonitor'): void
  /** AI 配置变了：App 要重新读一遍，AI 入口的可用状态跟着变 */
  (e: 'ai-changed'): void
  (e: 'theme', mode: ThemeMode): void
}>()

const THEMES: { v: ThemeMode; t: string }[] = [
  { v: 'dark', t: '深色' },
  { v: 'light', t: '浅色' },
  { v: 'system', t: '跟随系统' },
]

// 主题也可能从标题栏那个按钮改（比如设置面板开着时被外部改了），
// 这里跟一下，保证面板里显示的就是当前真实模式。
watch(
  () => props.settings.theme,
  (t) => {
    if (t && t !== form.value.theme) form.value.theme = t as ThemeMode
  }
)

/** 主题和其它设置不一样：改完立刻生效并落盘，不等"保存"。
 *  否则用户点了"取消"，界面却还留着预览过的颜色。 */
function pickTheme(m: ThemeMode) {
  form.value.theme = m
  emit('theme', m)
}

const tab = ref<'general' | 'quick' | 'ai' | 'hostkeys' | 'io' | 'about'>('general')
const form = ref<Settings>({ ...props.settings })

// 采样间隔：预设按钮是 [1,2,5,10]，自定义则直接往输入框填任意秒数（1-3600）。
// 后端存的就是 u64，任何值都生效，只是预设按钮没有 UI 而已。
const customInterval = ref(2)
watch(
  () => form.value.sample_interval_secs,
  (v) => {
    if (v >= 1) customInterval.value = v
  },
  { immediate: true },
)
function applyCustomInterval() {
  let v = customInterval.value
  if (!Number.isFinite(v) || v < 1) v = 1
  if (v > 3600) v = 3600
  customInterval.value = v
  form.value.sample_interval_secs = v
}
const exportPath = ref('D:\\sshbox-hosts.json')
const importPath = ref('D:\\sshbox-hosts.json')
const includeSecrets = ref(false)
const ioMsg = ref('')

function save() {
  emit('save', { ...form.value })
}

// --- 快捷命令编辑 ---------------------------------------------------------

function addQuick() {
  form.value.quick_commands = [
    ...(form.value.quick_commands ?? []),
    { id: `qc-${Date.now().toString(36)}`, label: '', command: '', enter: true },
  ]
}

function removeQuick(id: string) {
  form.value.quick_commands = (form.value.quick_commands ?? []).filter((q) => q.id !== id)
}

/** 上移/下移：面板里按钮的顺序就是显示顺序，值得能调。 */
function moveQuick(i: number, delta: number) {
  const list = form.value.quick_commands ?? []
  const j = i + delta
  if (j < 0 || j >= list.length) return
  const next = [...list]
  ;[next[i], next[j]] = [next[j], next[i]]
  form.value.quick_commands = next
}
function parseHostToken(token: string): { host: string; port: number } {
  const m = token.match(/^\[(.+)\]:(\d+)$/)
  if (m) return { host: m[1], port: parseInt(m[2], 10) }
  return { host: token, port: 22 }
}
defineExpose({ setIoMsg: (m: string) => (ioMsg.value = m) })
</script>

<template>
  <div class="mask" @click.self="emit('close')">
    <div class="dlg">
      <div class="tabs">
        <button :class="{ on: tab === 'general' }" @click="tab = 'general'">常规</button>
        <button :class="{ on: tab === 'quick' }" @click="tab = 'quick'">
          快捷命令 <span class="badge">{{ (form.quick_commands ?? []).length }}</span>
        </button>
        <button :class="{ on: tab === 'ai' }" @click="tab = 'ai'">AI 模型</button>
        <button :class="{ on: tab === 'hostkeys' }" @click="tab = 'hostkeys'">
          已知主机密钥 <span class="badge">{{ knownHosts.length }}</span>
        </button>
        <button :class="{ on: tab === 'io' }" @click="tab = 'io'">导入导出</button>
        <button :class="{ on: tab === 'about' }" @click="tab = 'about'">关于</button>
      </div>

      <!-- 快捷命令 -->
      <div v-if="tab === 'quick'" class="pane">
        <p class="hintline">
          显示在终端上方的命令条，点一下直接发到当前会话。命令里可以用
          <code>{host}</code>、<code>{user}</code>、<code>{port}</code> 占位，
          发送时会替换成当前主机的值。开着广播输入时，快捷命令也会发给所有选中会话。
        </p>
        <div v-for="(q, i) in form.quick_commands ?? []" :key="q.id" class="quick-row">
          <input v-model="q.label" class="q-label" placeholder="按钮名（如：看磁盘）" spellcheck="false" />
          <input
            v-model="q.command"
            class="q-cmd"
            placeholder="命令（如：df -h）"
            spellcheck="false"
          />
          <label class="q-enter" title="取消勾选则只填入终端、不自动回车">
            <input v-model="q.enter" type="checkbox" />
            回车
          </label>
          <button class="q-btn" title="上移" :disabled="i === 0" @click="moveQuick(i, -1)">↑</button>
          <button
            class="q-btn"
            title="下移"
            :disabled="i === (form.quick_commands ?? []).length - 1"
            @click="moveQuick(i, 1)"
          >
            ↓
          </button>
          <button class="q-btn danger" title="删除" @click="removeQuick(q.id)">×</button>
        </div>
        <p v-if="!(form.quick_commands ?? []).length" class="hintline">还没有快捷命令。</p>
                <button class="add-quick" @click="addQuick">+ 添加一条</button>
                <div class="btns">
                  <button class="primary" @click="save">保存设置</button>
                </div>
              </div>

      <!-- 常规 -->
      <div v-if="tab === 'general'" class="pane">
        <label>主题</label>
        <div class="seg">
          <button
            v-for="m in THEMES"
            :key="m.v"
            :class="{ sel: form.theme === m.v }"
            @click="pickTheme(m.v)"
          >{{ m.t }}</button>
        </div>
        <p class="hintline">
          立即生效并保存。选“跟随系统”时，系统切换深/浅色会跟着变。
        </p>

        <label>监控采样间隔（秒）</label>
        <div class="seg">
          <button
            v-for="s in [1, 2, 5, 10]"
                        :key="s"
                        :class="{ sel: form.sample_interval_secs === s }"
                        @click="form.sample_interval_secs = s"
                      >{{ s }}s</button>
                      <label class="custom-int" :class="{ sel: ![1, 2, 5, 10].includes(form.sample_interval_secs) }">
                        <input
                          v-model.number="customInterval"
                          type="number"
                          min="1"
                          max="3600"
                          step="1"
                          @change="applyCustomInterval"
                          @focus="($event.target as HTMLInputElement).select()"
                        />
                        <span>秒 自定义</span>
                      </label>
                    </div>
        <div class="hintline">不可见标签页会自动降频到 5 倍间隔以省带宽。</div>

        <label class="check">
          <input v-model="form.monitor_enabled" type="checkbox" />
          <span>启用实时监控面板</span>
        </label>
        <label class="check">
          <input v-model="form.auto_reconnect" type="checkbox" />
          <span>断线后自动重连（退避重试）</span>
        </label>
        <label class="rowline">
          <span>最大重试次数</span>
          <input v-model.number="form.reconnect_max_attempts" type="number" min="0" max="10" class="num" />
        </label>
        <label class="check">
          <input v-model="form.confirm_on_close_tab" type="checkbox" />
          <span>关闭已连接的标签页时二次确认</span>
        </label>

        <label class="check">
          <input v-model="form.alerts_enabled" type="checkbox" />
          <span>阈值告警（v0.3 生效）</span>
        </label>
        <div class="rowline">
          <span>CPU / 内存 / 磁盘 告警阈值 %</span>
          <div class="nums">
            <input v-model.number="form.cpu_alert_pct" type="number" min="1" max="100" class="num" />
            <input v-model.number="form.mem_alert_pct" type="number" min="1" max="100" class="num" />
            <input v-model.number="form.disk_alert_pct" type="number" min="1" max="100" class="num" />
          </div>
        </div>

        <label class="check">
          <input v-model="form.history_enabled" type="checkbox" />
          <span>历史落盘（SQLite，断线后仍可回看）</span>
        </label>
        <div class="rowline">
          <span>历史采样间隔（秒）</span>
          <input
            v-model.number="form.history_interval_secs"
            type="number"
            min="1"
            max="3600"
            class="num"
          />
        </div>
        <div class="rowline">
          <span>历史保留天数（过期自动清理）</span>
          <input
            v-model.number="form.history_retention_days"
            type="number"
            min="1"
            max="3650"
            class="num"
          />
        </div>
        <div class="hintline">
          默认每次采样都落盘（2 秒）。调大间隔可省磁盘：10 秒约省 5 倍空间。数据在本机，导出走「历史」面板。
        </div>

        <div class="btns">
          <button class="ghost" @click="emit('restartMonitor')">重启监控任务</button>
          <button class="primary" @click="save">保存设置</button>
        </div>
      </div>

      <!-- 已知主机密钥 -->
      <!-- AI 模型 -->
      <div v-else-if="tab === 'ai'" class="pane">
        <AiSettingsPane @changed="emit('ai-changed')" />
      </div>

      <div v-else-if="tab === 'hostkeys'" class="pane">
        <div class="hintline">
          这些是已信任的服务器密钥。删除后下次连接会重新询问指纹。
        </div>
        <div class="kh-list">
          <div v-if="!knownHosts.length" class="empty">还没有记录</div>
          <div v-for="e in knownHosts" :key="e.line" class="kh">
            <div class="kh-main">
              <div class="kh-host">{{ e.host }}</div>
              <div class="kh-fp">{{ e.key_type }} · {{ e.fingerprint }}</div>
            </div>
            <button
              v-if="!e.host.startsWith('|1|')"
              class="del"
              @click="emit('removeKnownHost', parseHostToken(e.host).host, parseHostToken(e.host).port)"
            >删除</button>
            <span v-else class="hashnote">哈希条目</span>
          </div>
        </div>
      </div>

      <!-- 导入导出 -->
      <div v-else-if="tab === 'io'" class="pane">
        <div class="hintline">当前共有 {{ hostCount }} 台主机。</div>
        <label>导出到文件</label>
        <input v-model="exportPath" />
        <label class="check">
          <input v-model="includeSecrets" type="checkbox" />
          <span>同时导出密码（明文 JSON，注意保管）</span>
        </label>
        <button class="ghost" @click="emit('exportHosts', exportPath, includeSecrets)">导出</button>

        <div class="sep"></div>

        <label>从文件导入</label>
        <input v-model="importPath" />
        <button class="ghost" @click="emit('importHosts', importPath)">导入（合并，不覆盖同 id）</button>

        <div v-if="ioMsg" class="msg">{{ ioMsg }}</div>
      </div>

      <!-- 关于 -->
      <div v-else class="pane">
        <div class="about-title">SSHBox <span class="about-ver">v{{ version }}</span></div>
        <div class="hintline">SSH 终端 + 虚拟机实时监控 + AI 助手 · 开源 (MIT)</div>
        <div class="paths">
          <div class="p"><span>版本</span><code>{{ version }}（{{ versionSource }}）</code></div>
          <div class="p"><span>配置目录</span><code>{{ paths?.data_dir }}</code></div>
          <div class="p"><span>主机列表</span><code>{{ paths?.hosts_json }}</code></div>
          <div class="p"><span>设置</span><code>{{ paths?.settings_json }}</code></div>
          <div class="p"><span>known_hosts</span><code>{{ paths?.known_hosts }}</code></div>
        </div>
        <div class="hintline" :class="{ warn: paths && !paths.credential_store_ok }">
          系统凭据管理器：
          {{ paths?.credential_store_ok ? '可用（密码加密保存）' : '不可用（密码不会被保存）' }}
        </div>
      </div>

      <div class="close-row">
        <button @click="emit('close')">关闭</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.mask {
  position: fixed; inset: 0; background: var(--mask);
  display: flex; align-items: center; justify-content: center; z-index: 105;
}
.dlg {
  background: var(--ctp-base); border: 1px solid var(--ctp-surface0); border-radius: 10px;
  padding: 16px; width: 520px; max-height: 86vh; display: flex; flex-direction: column; gap: 10px;
}
.quick-row { display: flex; align-items: center; gap: 6px; margin-bottom: 6px; }
.quick-row input[type='text'],
.quick-row input:not([type]) {
  background: var(--ctp-crust);
  color: var(--ctp-text);
  border: 1px solid var(--ctp-surface1);
  border-radius: 6px;
  padding: 4px 8px;
  font-size: 12px;
}
.q-label { width: 130px; }
.q-cmd { flex: 1; min-width: 200px; font-family: ui-monospace, Consolas, monospace; }
.q-enter { display: flex; align-items: center; gap: 4px; font-size: 11.5px; color: var(--ctp-subtext0); }
.q-btn {
  background: var(--ctp-surface0);
  color: var(--ctp-text);
  border: 1px solid var(--ctp-surface1);
  border-radius: 5px;
  width: 26px;
  height: 24px;
  cursor: pointer;
  font-size: 12px;
}
.q-btn:disabled { opacity: 0.4; cursor: default; }
.q-btn.danger { color: var(--ctp-red); }
.add-quick {
  margin-top: 8px;
  background: var(--ctp-blue);
  color: var(--on-accent);
  border: none;
  border-radius: 6px;
  padding: 5px 12px;
  cursor: pointer;
  font-size: 12.5px;
}
code {
  background: var(--ctp-surface0);
  padding: 1px 4px;
  border-radius: 3px;
  font-size: 11.5px;
}
.tabs { display: flex; gap: 4px; border-bottom: 1px solid var(--ctp-surface0); padding-bottom: 8px; }
.tabs button {
  background: none; border: none; color: var(--ctp-overlay0); font-size: 12.5px;
  padding: 5px 9px; border-radius: 5px; cursor: pointer;
}
.tabs button.on { background: var(--ctp-surface0); color: var(--ctp-text); }
.badge { color: var(--ctp-blue); font-size: 11px; }
.pane { display: flex; flex-direction: column; gap: 5px; overflow-y: auto; }
label { font-size: 11px; color: var(--ctp-subtext0); margin-top: 6px; }
input:not([type='checkbox']) {
  background: var(--ctp-crust); border: 1px solid var(--ctp-surface0); border-radius: 6px;
  color: var(--ctp-text); padding: 7px 9px; font-size: 12.5px; width: 100%; box-sizing: border-box;
}
input:focus { outline: 1px solid var(--ctp-blue); }
.seg { display: flex; gap: 6px; }
.custom-int {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: 4px;
  flex: 1; /* 与预设按钮等宽平分 */
  background: var(--ctp-crust); /* 与预设按钮同底，视觉统一 */
  border: 1px solid var(--ctp-surface0);
  border-radius: 6px;
  padding: 6px 8px;
  color: var(--ctp-subtext0);
  font-size: 12px;
  height: 32px;
  box-sizing: border-box;
}
.custom-int.sel { border-color: var(--ctp-blue); color: var(--ctp-blue); }
.custom-int input {
  width: 56px;
  background: var(--ctp-mantle);
  color: var(--ctp-text);
  border: none;
  font-size: 12px;
  padding: 2px 1px;
  box-sizing: border-box;
}
.seg button {
  flex: 1; background: var(--ctp-crust); border: 1px solid var(--ctp-surface0); color: var(--ctp-subtext0);
  border-radius: 6px; padding: 6px; cursor: pointer; font-size: 12px; height: 32px;
  display: inline-flex; align-items: center; justify-content: center; box-sizing: border-box;
}
.seg button.sel { border-color: var(--ctp-blue); color: var(--ctp-blue); }
.check { display: flex; align-items: center; gap: 7px; margin-top: 9px; cursor: pointer; }
.check input { width: auto; }
.check span { font-size: 12px; color: var(--ctp-subtext0); }
.rowline { display: flex; align-items: center; justify-content: space-between; gap: 10px; margin-top: 9px; }
.rowline span { font-size: 12px; color: var(--ctp-subtext0); }
.nums { display: flex; gap: 6px; }
.num { width: 64px !important; }
.hintline { font-size: 11px; color: var(--ctp-overlay0); line-height: 1.6; }
.hintline.warn { color: var(--ctp-yellow); }
.btns { display: flex; gap: 8px; justify-content: flex-end; margin-top: 14px; }
.ghost {
  background: var(--ctp-crust); border: 1px solid var(--ctp-surface0); color: var(--ctp-subtext0);
  border-radius: 6px; padding: 7px 12px; cursor: pointer; font-size: 12.5px;
}
.primary {
  background: var(--ctp-blue); color: var(--on-accent); border: none; border-radius: 6px;
  padding: 7px 16px; font-weight: 600; cursor: pointer; font-size: 12.5px;
}
.kh-list { max-height: 300px; overflow-y: auto; display: flex; flex-direction: column; gap: 4px; }
.kh {
  display: flex; align-items: center; gap: 8px; background: var(--ctp-crust);
  border-radius: 6px; padding: 7px 9px;
}
.kh-main { flex: 1; min-width: 0; }
.kh-host { font-size: 12.5px; color: var(--ctp-text); }
.kh-fp { font-size: 10.5px; color: var(--ctp-overlay0); font-family: Consolas, monospace; word-break: break-all; }
.del {
  background: none; border: 1px solid var(--ctp-surface0); color: var(--ctp-red);
  border-radius: 5px; padding: 3px 8px; cursor: pointer; font-size: 11px; flex-shrink: 0;
}
.hashnote { font-size: 10px; color: var(--ctp-surface1); }
.empty { color: var(--ctp-surface1); font-size: 12px; padding: 16px 0; text-align: center; }
.sep { height: 1px; background: var(--ctp-surface0); margin: 14px 0 4px; }
.msg { font-size: 11.5px; color: var(--ctp-green); margin-top: 8px; }
.about-title { font-size: 20px; font-weight: 700; color: var(--ctp-blue); }
.about-ver { color: var(--ctp-overlay0); font-size: 12px; font-weight: 400; }
.paths { display: flex; flex-direction: column; gap: 5px; margin-top: 10px; }
.p { display: flex; gap: 10px; font-size: 11.5px; }
.p span { color: var(--ctp-overlay0); min-width: 84px; flex-shrink: 0; }
.p code { color: var(--ctp-subtext0); font-family: Consolas, monospace; word-break: break-all; }
.close-row { display: flex; justify-content: flex-end; }
.close-row button {
  padding: 6px 14px; border-radius: 6px; border: 1px solid var(--ctp-surface0);
  background: var(--ctp-crust); color: var(--ctp-subtext0); cursor: pointer; font-size: 12.5px;
}
</style>
