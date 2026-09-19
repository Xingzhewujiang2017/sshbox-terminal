<script setup lang="ts">
import { computed } from 'vue'
import type { ErrPayload } from '../api'

const props = defineProps<{ payload: ErrPayload }>()
const emit = defineEmits<{ (e: 'trust'): void; (e: 'cancel'): void }>()

const changed = computed(() => props.payload.kind === 'host_key_changed')
const target = computed(() =>
  props.payload.port && props.payload.port !== 22
    ? `${props.payload.host}:${props.payload.port}`
    : props.payload.host
)
</script>

<template>
  <div class="mask" @click.self="emit('cancel')">
    <div class="dlg" :class="{ danger: changed }">
      <h3>{{ changed ? '⚠ 主机密钥已改变' : '首次连接：确认主机密钥' }}</h3>

      <p v-if="changed" class="lead danger-text">
        服务器 <b>{{ target }}</b> 提供的密钥与上次记录<b>不一致</b>。这可能是服务器重装/换密钥，
        也可能是<b>中间人攻击</b>。请先通过其他可信渠道核对指纹，再决定是否继续。
      </p>
      <p v-else class="lead">
        这是第一次连接 <b>{{ target }}</b>。请核对下面的指纹与该服务器真实指纹是否一致，
        确认后会被记入 known_hosts，之后连接将自动校验。
      </p>

      <div class="fp-box">
        <div class="fp-row">
          <span class="k">主机</span><span class="v">{{ target }}</span>
        </div>
        <div class="fp-row">
          <span class="k">算法</span><span class="v">{{ payload.key_type || '未知' }}</span>
        </div>
        <div class="fp-row">
          <span class="k">{{ changed ? '服务器现提供' : '指纹' }}</span>
          <span class="v mono">{{ payload.fingerprint || '未知' }}</span>
        </div>
        <div v-if="changed" class="fp-row">
          <span class="k">本机已记录</span>
          <span class="v mono danger-text">{{ payload.expected_fingerprint || '未知' }}</span>
        </div>
      </div>

      <div class="hintline">
        在服务器上核对指纹：<code>ssh-keygen -lf /etc/ssh/ssh_host_{{ (payload.key_type || 'ed25519').replace('ssh-', '') }}_key.pub</code>
      </div>

      <div class="btns">
        <button @click="emit('cancel')">取消</button>
        <button class="primary" :class="{ danger: changed }" @click="emit('trust')">
          {{ changed ? '删除旧记录并信任新密钥' : '信任并保存' }}
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.mask {
  position: fixed; inset: 0; background: rgba(0,0,0,0.65);
  display: flex; align-items: center; justify-content: center; z-index: 110;
}
.dlg {
  background: #1e1e2e; border: 1px solid #313244; border-radius: 10px;
  padding: 18px; width: 470px; display: flex; flex-direction: column; gap: 10px;
}
.dlg.danger { border-color: #f38ba8; }
h3 { font-size: 15px; color: #cdd6f4; }
.lead { font-size: 12.5px; color: #a6adc8; line-height: 1.7; }
.danger-text { color: #f38ba8; }
.fp-box { background: #11111b; border-radius: 7px; padding: 10px; display: flex; flex-direction: column; gap: 5px; }
.fp-row { display: flex; gap: 10px; font-size: 12px; }
.fp-row .k { color: #6c7086; min-width: 76px; flex-shrink: 0; }
.fp-row .v { color: #cdd6f4; word-break: break-all; }
.mono { font-family: Consolas, monospace; font-size: 11.5px; }
.hintline { font-size: 11px; color: #6c7086; line-height: 1.6; }
.hintline code {
  background: #11111b; padding: 1px 4px; border-radius: 3px;
  font-family: Consolas, monospace; color: #a6adc8;
}
.btns { display: flex; gap: 8px; justify-content: flex-end; }
.btns button {
  padding: 7px 15px; border-radius: 6px; border: 1px solid #313244;
  background: #11111b; color: #a6adc8; cursor: pointer; font-size: 13px;
}
.btns .primary { background: #89b4fa; color: #11111b; border: none; font-weight: 600; }
.btns .primary.danger { background: #f38ba8; }
</style>
