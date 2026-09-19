<script setup lang="ts">
import { computed, ref } from 'vue'
import type { ErrPayload } from '../api'

const props = defineProps<{
  payload: ErrPayload
  hostLabel: string
  defaultSave: boolean
  /** Message from a previous failed attempt (wrong password / passphrase). */
  attemptError?: string
}>()

const emit = defineEmits<{
  (e: 'submit', password: string, save: boolean): void
  (e: 'cancel'): void
}>()

const value = ref('')
const save = ref(props.defaultSave)
const show = ref(false)

const isPassphrase = computed(
  () => props.payload.kind === 'need_passphrase' || props.payload.kind === 'key_passphrase_wrong'
)
const title = computed(() => {
  switch (props.payload.kind) {
    case 'need_passphrase':
      return '私钥需要口令'
    case 'key_passphrase_wrong':
      return '私钥口令错误'
    case 'auth_failed':
      return '认证失败，请重新输入'
    default:
      return '输入密码'
  }
})

function submit() {
  if (!value.value) return
  emit('submit', value.value, save.value)
}
</script>

<template>
  <div class="mask" @click.self="emit('cancel')">
    <div class="dlg">
      <h3>{{ title }}</h3>
      <div class="who">{{ hostLabel }}</div>

      <div v-if="attemptError" class="err">{{ attemptError }}</div>

      <label>{{ isPassphrase ? '私钥口令' : '密码' }}</label>
      <div class="row">
        <input
          v-model="value"
          :type="show ? 'text' : 'password'"
          autofocus
          @keyup.enter="submit"
        />
        <button class="eye" @click="show = !show">{{ show ? '🙈' : '👁' }}</button>
      </div>

      <label v-if="!isPassphrase" class="check">
        <input v-model="save" type="checkbox" />
        <span>保存到系统凭据管理器</span>
      </label>

      <div class="btns">
        <button @click="emit('cancel')">取消</button>
        <button class="primary" :disabled="!value" @click="submit">连接</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.mask {
  position: fixed; inset: 0; background: var(--mask);
  display: flex; align-items: center; justify-content: center; z-index: 115;
}
.dlg {
  background: var(--ctp-base); border: 1px solid var(--ctp-surface0); border-radius: 10px;
  padding: 18px; width: 400px; display: flex; flex-direction: column; gap: 6px;
}
h3 { font-size: 15px; color: var(--ctp-text); }
.who { font-size: 12px; color: var(--ctp-blue); margin-bottom: 4px; }
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
.check { display: flex; align-items: center; gap: 7px; margin-top: 10px; cursor: pointer; }
.check input { width: auto; }
.check span { font-size: 12px; color: var(--ctp-subtext0); }
.err { color: var(--ctp-red); font-size: 12px; line-height: 1.5; word-break: break-all; }
.btns { display: flex; gap: 8px; justify-content: flex-end; margin-top: 14px; }
.btns button {
  padding: 7px 16px; border-radius: 6px; border: 1px solid var(--ctp-surface0);
  background: var(--ctp-crust); color: var(--ctp-subtext0); cursor: pointer; font-size: 13px;
}
.btns .primary { background: var(--ctp-blue); color: var(--on-accent); border: none; font-weight: 600; }
.btns .primary:disabled { opacity: 0.5; cursor: not-allowed; }
</style>
