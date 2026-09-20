<script setup lang="ts">
/**
 * 列表分页条：每页固定 `size` 条（默认 10），也可以一键展开「全部」。
 *
 * 列表长的卡片（网络接口 / 磁盘 / 进程 / 端口）都用它，行为保持一致：
 * 默认分页而不是一次倒出上百行，需要看全时再点「全部」。
 */
import { computed } from 'vue'

const props = withDefaults(
  defineProps<{
    total: number
    page: number
    all: boolean
    /** 每页条数，默认 10 */
    size?: number
    /** 「全部」按钮上的单位，如「项」「个进程」 */
    unit?: string
  }>(),
  { size: 10, unit: '项' },
)

const emit = defineEmits<{
  (e: 'update:page', v: number): void
  (e: 'update:all', v: boolean): void
}>()

const pages = computed(() => Math.max(1, Math.ceil(props.total / props.size)))
const cur = computed(() => Math.min(Math.max(1, props.page), pages.value))
const shown = computed(() => (props.all ? props.total : Math.min(props.total, props.size)))
</script>

<template>
  <div v-if="total > size" class="pager">
    <template v-if="!all">
      <button class="pg" :disabled="cur <= 1" title="上一页" @click="emit('update:page', cur - 1)">‹</button>
      <span class="pg-info">{{ cur }} / {{ pages }}</span>
      <button class="pg" :disabled="cur >= pages" title="下一页" @click="emit('update:page', cur + 1)">›</button>
    </template>
    <span class="pg-info">显示 {{ shown }} / {{ total }} {{ unit }}</span>
    <button class="pg-all" @click="emit('update:all', !all)">{{ all ? '收起分页' : '全部' }}</button>
  </div>
</template>

<style scoped>
.pager {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-top: 6px;
  font-size: 11px;
  color: var(--ctp-overlay0);
}
.pg,
.pg-all {
  background: var(--ctp-base);
  color: var(--ctp-subtext0);
  border: 1px solid var(--ctp-surface0);
  border-radius: 4px;
  padding: 1px 6px;
  font-size: 11px;
  cursor: pointer;
}
.pg:hover:not(:disabled),
.pg-all:hover {
  border-color: var(--ctp-surface2);
  color: var(--ctp-text);
}
.pg:disabled {
  opacity: 0.4;
  cursor: default;
}
.pg-all {
  margin-left: auto;
  color: var(--ctp-blue);
  border-color: var(--ctp-surface1);
}
.pg-info {
  font-variant-numeric: tabular-nums;
}
</style>
