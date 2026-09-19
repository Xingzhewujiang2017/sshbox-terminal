<script setup lang="ts">
import { computed, ref } from 'vue'
import { emptyHost, type Host } from '../api'

const props = defineProps<{
  host: Host | null
  groups: string[]
  existingGroups: string[]
}>()

const emit = defineEmits<{
  (e: 'save', host: Host, password: string | null): void
  (e: 'close'): void
}>()

const form = ref<Host>(props.host ? { ...props.host } : emptyHost())
const password = ref('')
const showPassword = ref(false)
const isEdit = computed(() => !!props.host?.id)
const hadPassword = computed(() => !!props.host?.save_password)
const valid = computed(() => !!form.value.host.trim() && !!form.value.username.trim())

const groupOptions = computed(() => {
  const set = new Set([...props.existingGroups, ...props.groups, '默认'])
  return [...set].filter(Boolean)
})

function submit() {
  if (!valid.value) return
  const h = { ...form.value }
  if (!h.name.trim()) h.name = h.host.trim()
  // Only send a password when the user typed one; empty means "leave as is".
  const pw = form.value.auth === 'password' && password.value ? password.value : null
  if (pw) h.save_password = true
  emit('save', h, pw)
}
</script>

<template>
  <div class="mask" @click.self="emit('close')">
    <div class="dlg">
      <h3>{{ isEdit ? '编辑主机' : '新建 SSH 连接' }}</h3>

      <div class="grid2">
        <div>
          <label>名称</label>
          <input v-model="form.name" placeholder="例如 测试机 / 生产 Web" @keyup.enter="submit" />
        </div>
        <div>
          <label>分组</label>
          <input v-model="form.group" list="group-list" placeholder="默认" />
          <datalist id="group-list">
            <option v-for="g in groupOptions" :key="g" :value="g" />
          </datalist>
        </div>
      </div>

      <label>主机地址</label>
      <div class="row">
        <input v-model="form.host" placeholder="192.168.x.x 或域名" autofocus @keyup.enter="submit" />
        <input v-model.number="form.port" type="number" class="port" />
      </div>

      <label>用户名</label>
      <input v-model="form.username" @keyup.enter="submit" />

      <label>认证方式</label>
      <div class="seg">
        <button :class="{ sel: form.auth === 'password' }" @click="form.auth = 'password'">密码</button>
        <button :class="{ sel: form.auth === 'key' }" @click="form.auth = 'key'">私钥</button>
      </div>

      <template v-if="form.auth === 'password'">
        <label>
          密码
          <span v-if="hadPassword" class="note">（已保存，留空则不修改）</span>
        </label>
        <div class="row">
          <input
            v-model="password"
            :type="showPassword ? 'text' : 'password'"
            :placeholder="hadPassword ? '••••••••' : '登录密码'"
            @keyup.enter="submit"
          />
          <button class="eye" @click="showPassword = !showPassword">{{ showPassword ? '🙈' : '👁' }}</button>
        </div>
        <label class="check">
          <input v-model="form.save_password" type="checkbox" />
          <span>保存到系统凭据管理器（Windows 凭据管理器）</span>
        </label>
      </template>

      <template v-else>
        <label>私钥文件路径</label>
        <input v-model="form.key_path" placeholder="C:\Users\you\.ssh\id_ed25519" />
        <label class="check">
          <input v-model="form.save_password" type="checkbox" />
          <span>保存到系统凭据管理器</span>
        </label>
        <div class="hintline">加密私钥在连接时会提示输入口令（口令可选保存）。</div>
      </template>

      <label class="check">
        <input v-model="form.auto_reconnect" type="checkbox" />
        <span>断线后自动重连</span>
      </label>

      <label>主机密钥校验</label>
      <div class="seg">
        <button
          :class="{ sel: (form.host_key_policy || 'strict') === 'strict' }"
          @click="form.host_key_policy = 'strict'"
        >严格（推荐）</button>
        <button
          :class="{ sel: form.host_key_policy === 'accept_any' }"
          @click="form.host_key_policy = 'accept_any'"
        >跳过校验</button>
      </div>
      <div v-if="form.host_key_policy === 'accept_any'" class="warn">
        ⚠ 跳过校验后无法察觉中间人攻击，仅对完全可信的内网测试机使用。
      </div>

      <label>备注</label>
      <input v-model="form.note" placeholder="可选" />

      <div class="btns">
        <button @click="emit('close')">取消</button>
        <button class="primary" :disabled="!valid" @click="submit">保存</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.mask {
  position: fixed; inset: 0; background: var(--mask);
  display: flex; align-items: center; justify-content: center; z-index: 100;
}
.dlg {
  background: var(--ctp-base); border: 1px solid var(--ctp-surface0); border-radius: 10px;
  padding: 18px; width: 460px; max-height: 88vh; overflow-y: auto;
  display: flex; flex-direction: column; gap: 5px;
}
h3 { font-size: 15px; margin-bottom: 6px; color: var(--ctp-text); }
label { font-size: 11px; color: var(--ctp-subtext0); margin-top: 6px; }
.note { color: var(--ctp-overlay0); }
input[type='text'], input[type='password'], input[type='number'], input:not([type]) {
  background: var(--ctp-crust); border: 1px solid var(--ctp-surface0); border-radius: 6px;
  color: var(--ctp-text); padding: 7px 9px; font-size: 13px; width: 100%; box-sizing: border-box;
  font-family: inherit;
}
input:focus { outline: 1px solid var(--ctp-blue); }
.grid2 { display: grid; grid-template-columns: 1fr 1fr; gap: 10px; }
.row { display: flex; gap: 8px; }
.row .port { width: 84px; flex-shrink: 0; }
.eye {
  background: var(--ctp-crust); border: 1px solid var(--ctp-surface0); border-radius: 6px;
  color: var(--ctp-subtext0); cursor: pointer; padding: 0 10px; flex-shrink: 0;
}
.seg { display: flex; gap: 8px; }
.seg button {
  flex: 1; background: var(--ctp-crust); border: 1px solid var(--ctp-surface0); color: var(--ctp-subtext0);
  border-radius: 6px; padding: 6px; cursor: pointer; font-size: 12px;
}
.seg button.sel { border-color: var(--ctp-blue); color: var(--ctp-blue); }
.check { display: flex; align-items: center; gap: 7px; margin-top: 8px; cursor: pointer; }
.check input { width: auto; }
.check span { font-size: 12px; color: var(--ctp-subtext0); }
.warn {
  font-size: 11px; color: var(--ctp-yellow); background: var(--banner-warn-bg);
  border-radius: 5px; padding: 6px 8px; margin-top: 4px; line-height: 1.5;
}
.hintline { font-size: 11px; color: var(--ctp-overlay0); margin-top: 4px; }
.btns { display: flex; gap: 8px; justify-content: flex-end; margin-top: 14px; }
.btns button {
  padding: 7px 16px; border-radius: 6px; border: 1px solid var(--ctp-surface0);
  background: var(--ctp-crust); color: var(--ctp-subtext0); cursor: pointer; font-size: 13px;
}
.btns .primary { background: var(--ctp-blue); color: var(--on-accent); border: none; font-weight: 600; }
.btns .primary:disabled { opacity: 0.5; cursor: not-allowed; }
</style>
