<script setup lang="ts">
import { computed, ref } from 'vue'

const props = defineProps<{
  mode: 'setup' | 'unlock'
  error?: string
}>()

const emit = defineEmits<{
  (e: 'submit', master: string): void
  (e: 'skip'): void
}>()

const master = ref('')
const confirm = ref('')
const show = ref(false)

const isSetup = computed(() => props.mode === 'setup')
const mismatch = computed(() => isSetup.value && confirm.value.length > 0 && confirm.value !== master.value)
const canSubmit = computed(() => {
  if (isSetup.value) return master.value.trim().length >= 6 && master.value === confirm.value
  return master.value.length > 0
})

function submit() {
  if (!canSubmit.value) return
  emit('submit', master.value)
}
</script>

<template>
  <div class="mask">
    <div class="dlg">
      <h3>{{ isSetup ? '设置主密码' : '解锁凭据保险箱' }}</h3>
      <div class="hint">
        {{
          isSetup
            ? '主机密码会用这个主密码加密后存在本机。主密码本身不落盘 —— 忘记它就只能删掉凭据文件重新来。'
            : '输入主密码解锁后，已保存的主机密码才能自动使用。'
        }}
      </div>

      <div v-if="error" class="err">{{ error }}</div>

      <label>主密码</label>
      <div class="row">
        <input
          v-model="master"
          :type="show ? 'text' : 'password'"
          autofocus
          placeholder="至少 6 位"
          @keyup.enter="submit"
        />
        <button class="eye" type="button" @click="show = !show">{{ show ? '🙈' : '👁' }}</button>
      </div>

      <template v-if="isSetup">
        <label>再输一次</label>
        <input v-model="confirm" :type="show ? 'text' : 'password'" @keyup.enter="submit" />
        <div v-if="mismatch" class="err">两次输入不一致</div>
      </template>

      <div class="btns">
        <button @click="emit('skip')">以后再说</button>
        <button class="primary" :disabled="!canSubmit" @click="submit">
          {{ isSetup ? '设置' : '解锁' }}
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.mask {
  position: fixed; inset: 0; background: var(--mask);
  display: flex; align-items: center; justify-content: center; z-index: 130;
}
.dlg {
  background: var(--ctp-base); border: 1px solid var(--ctp-surface0); border-radius: 10px;
  padding: 18px; width: 430px; display: flex; flex-direction: column; gap: 6px;
}
h3 { font-size: 15px; color: var(--ctp-text); }
.hint { font-size: 12px; color: var(--ctp-subtext0); line-height: 1.6; margin-bottom: 4px; }
label { font-size: 11px; color: var(--ctp-subtext0); margin-top: 6px; }
input[type='password'], input[type='text'] {
  background: var(--ctp-crust); border: 1px solid var(--ctp-surface0); border-radius: 6px;
  color: var(--ctp-text); padding: 8px 9px; font-size: 13px; width: 100%; box-sizing: border-box;
}
input:focus { outline: 1px solid var(--ctp-blue); }
.row { display: flex; gap: 8px; }
.eye {
  background: var(--ctp-crust); border: 1px solid var(--ctp-surface0); border-radius: 6px;
  color: var(--ctp-subtext0); cursor: pointer; padding: 0 10px; flex-shrink: 0;
}
.err { color: var(--ctp-red); font-size: 12px; line-height: 1.5; word-break: break-all; }
.btns { display: flex; gap: 8px; justify-content: flex-end; margin-top: 14px; }
.btns button {
  padding: 7px 16px; border-radius: 6px; border: 1px solid var(--ctp-surface0);
  background: var(--ctp-crust); color: var(--ctp-subtext0); cursor: pointer; font-size: 13px;
}
.btns .primary { background: var(--ctp-blue); color: var(--on-accent); border: none; font-weight: 600; }
.btns .primary:disabled { opacity: 0.5; cursor: not-allowed; }
</style>
