<template>
  <div class="qc">
    <button
      class="toggle"
      :class="{ on: open }"
      :title="open ? '收起快捷命令' : '展开快捷命令（Ctrl+Shift+P）'"
      @click="toggle"
    >
      ⚡ 快捷命令
      <span v-if="!open && commands.length" class="count">{{ commands.length }}</span>
    </button>
    <div v-if="open" class="list">
      <button
        v-for="c in commands"
        :key="c.id"
        class="chip"
        :title="`${subst(c.command)}${c.enter ? '' : '（只填入不执行）'}`"
        @click="send(c)"
      >
        {{ c.label || c.command }}
      </button>
      <span v-if="!commands.length" class="muted">
        还没有快捷命令 —— 到「设置 → 快捷命令」里添加
      </span>
      <span class="spacer" />
      <span class="muted hint">支持 {host} {user} {port} 占位</span>
    </div>
  </div>
</template>

<script setup lang="ts">
/**
 * 快捷命令条（终端上方，可折叠）。
 *
 * 折叠状态存 localStorage：多数人要么一直用要么一直不用，每次都要点一下很烦。
 */
import { onMounted, ref } from 'vue'
import type { QuickCommand } from '../api'

const props = defineProps<{
  commands: QuickCommand[]
  host?: string | null
  user?: string | null
  port?: number | null
}>()

const emit = defineEmits<{ (e: 'send', text: string): void }>()

const KEY = 'sshbox-quickcmd-open'
const open = ref(localStorage.getItem(KEY) === '1')

function toggle() {
  open.value = !open.value
  localStorage.setItem(KEY, open.value ? '1' : '0')
}

/** 占位替换：{host} {user} {port} */
function subst(cmd: string): string {
  return cmd
    .replace(/\{host\}/g, props.host ?? '')
    .replace(/\{user\}/g, props.user ?? '')
    .replace(/\{port\}/g, String(props.port ?? 22))
}

function send(c: QuickCommand) {
  emit('send', subst(c.command) + (c.enter ? '\r' : ''))
}

onMounted(() => {
  // 支持 Ctrl+Shift+P 从外部展开
  window.addEventListener('sshbox-quickcmd-toggle', () => toggle())
})
</script>

<style scoped>
.qc {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 4px 8px;
  border-bottom: 1px solid var(--ctp-surface0);
  background: var(--ctp-mantle);
  flex-wrap: wrap;
}

.toggle {
  background: var(--ctp-surface0);
  color: var(--ctp-text);
  border: 1px solid var(--ctp-surface1);
  border-radius: 6px;
  padding: 2px 8px;
  font-size: 11.5px;
  cursor: pointer;
  white-space: nowrap;
}

.toggle.on {
  background: var(--ctp-blue);
  color: var(--on-accent);
  border-color: var(--ctp-blue);
}

.count {
  margin-left: 4px;
  opacity: 0.8;
}

.list {
  display: flex;
  align-items: center;
  gap: 6px;
  flex-wrap: wrap;
  flex: 1;
  min-width: 0;
}

.chip {
  background: var(--ctp-surface0);
  color: var(--ctp-text);
  border: 1px solid var(--ctp-surface1);
  border-radius: 12px;
  padding: 2px 10px;
  font-size: 11.5px;
  cursor: pointer;
  max-width: 240px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.chip:hover {
  background: var(--ctp-surface1);
}

.spacer {
  flex: 1;
}

.muted {
  color: var(--ctp-overlay0);
  font-size: 11px;
}

.hint {
  white-space: nowrap;
}
</style>
