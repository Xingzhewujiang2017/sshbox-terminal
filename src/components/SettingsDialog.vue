<script setup lang="ts">
import { ref } from 'vue'
import type { AppPaths, KnownHostEntry, Settings } from '../api'

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
}>()

const tab = ref<'general' | 'hostkeys' | 'io' | 'about'>('general')
const form = ref<Settings>({ ...props.settings })
const exportPath = ref('D:\\sshbox-hosts.json')
const importPath = ref('D:\\sshbox-hosts.json')
const includeSecrets = ref(false)
const ioMsg = ref('')

function save() {
  emit('save', { ...form.value })
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
        <button :class="{ on: tab === 'hostkeys' }" @click="tab = 'hostkeys'">
          已知主机密钥 <span class="badge">{{ knownHosts.length }}</span>
        </button>
        <button :class="{ on: tab === 'io' }" @click="tab = 'io'">导入导出</button>
        <button :class="{ on: tab === 'about' }" @click="tab = 'about'">关于</button>
      </div>

      <!-- 常规 -->
      <div v-if="tab === 'general'" class="pane">
        <label>监控采样间隔（秒）</label>
        <div class="seg">
          <button
            v-for="s in [1, 2, 5, 10]"
            :key="s"
            :class="{ sel: form.sample_interval_secs === s }"
            @click="form.sample_interval_secs = s"
          >{{ s }}s</button>
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
        <div class="about-title">SSHBox</div>
        <div class="hintline">SSH 终端 + 虚拟机实时监控 · 开源 (MIT)</div>
        <div class="paths">
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
  position: fixed; inset: 0; background: rgba(0,0,0,0.6);
  display: flex; align-items: center; justify-content: center; z-index: 105;
}
.dlg {
  background: #1e1e2e; border: 1px solid #313244; border-radius: 10px;
  padding: 16px; width: 520px; max-height: 86vh; display: flex; flex-direction: column; gap: 10px;
}
.tabs { display: flex; gap: 4px; border-bottom: 1px solid #313244; padding-bottom: 8px; }
.tabs button {
  background: none; border: none; color: #6c7086; font-size: 12.5px;
  padding: 5px 9px; border-radius: 5px; cursor: pointer;
}
.tabs button.on { background: #313244; color: #cdd6f4; }
.badge { color: #89b4fa; font-size: 11px; }
.pane { display: flex; flex-direction: column; gap: 5px; overflow-y: auto; }
label { font-size: 11px; color: #a6adc8; margin-top: 6px; }
input:not([type='checkbox']) {
  background: #11111b; border: 1px solid #313244; border-radius: 6px;
  color: #cdd6f4; padding: 7px 9px; font-size: 12.5px; width: 100%; box-sizing: border-box;
}
input:focus { outline: 1px solid #89b4fa; }
.seg { display: flex; gap: 6px; }
.seg button {
  flex: 1; background: #11111b; border: 1px solid #313244; color: #a6adc8;
  border-radius: 6px; padding: 6px; cursor: pointer; font-size: 12px;
}
.seg button.sel { border-color: #89b4fa; color: #89b4fa; }
.check { display: flex; align-items: center; gap: 7px; margin-top: 9px; cursor: pointer; }
.check input { width: auto; }
.check span { font-size: 12px; color: #a6adc8; }
.rowline { display: flex; align-items: center; justify-content: space-between; gap: 10px; margin-top: 9px; }
.rowline span { font-size: 12px; color: #a6adc8; }
.nums { display: flex; gap: 6px; }
.num { width: 64px !important; }
.hintline { font-size: 11px; color: #6c7086; line-height: 1.6; }
.hintline.warn { color: #f9e2af; }
.btns { display: flex; gap: 8px; justify-content: flex-end; margin-top: 14px; }
.ghost {
  background: #11111b; border: 1px solid #313244; color: #a6adc8;
  border-radius: 6px; padding: 7px 12px; cursor: pointer; font-size: 12.5px;
}
.primary {
  background: #89b4fa; color: #11111b; border: none; border-radius: 6px;
  padding: 7px 16px; font-weight: 600; cursor: pointer; font-size: 12.5px;
}
.kh-list { max-height: 300px; overflow-y: auto; display: flex; flex-direction: column; gap: 4px; }
.kh {
  display: flex; align-items: center; gap: 8px; background: #11111b;
  border-radius: 6px; padding: 7px 9px;
}
.kh-main { flex: 1; min-width: 0; }
.kh-host { font-size: 12.5px; color: #cdd6f4; }
.kh-fp { font-size: 10.5px; color: #6c7086; font-family: Consolas, monospace; word-break: break-all; }
.del {
  background: none; border: 1px solid #313244; color: #f38ba8;
  border-radius: 5px; padding: 3px 8px; cursor: pointer; font-size: 11px; flex-shrink: 0;
}
.hashnote { font-size: 10px; color: #45475a; }
.empty { color: #45475a; font-size: 12px; padding: 16px 0; text-align: center; }
.sep { height: 1px; background: #313244; margin: 14px 0 4px; }
.msg { font-size: 11.5px; color: #a6e3a1; margin-top: 8px; }
.about-title { font-size: 20px; font-weight: 700; color: #89b4fa; }
.paths { display: flex; flex-direction: column; gap: 5px; margin-top: 10px; }
.p { display: flex; gap: 10px; font-size: 11.5px; }
.p span { color: #6c7086; min-width: 84px; flex-shrink: 0; }
.p code { color: #a6adc8; font-family: Consolas, monospace; word-break: break-all; }
.close-row { display: flex; justify-content: flex-end; }
.close-row button {
  padding: 6px 14px; border-radius: 6px; border: 1px solid #313244;
  background: #11111b; color: #a6adc8; cursor: pointer; font-size: 12.5px;
}
</style>
