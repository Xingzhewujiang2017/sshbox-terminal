<script setup lang="ts">
/**
 * 「思考过程」折叠栏 —— **两种轮次共用这一份实现**。
 *
 * 对话轮放在正文上方；生成命令轮放在命令框**下方**（命令才是第一眼要看的）。
 * 两处各写一份模板的话，改标题文案/改样式必然漏改一处 —— 本项目已经吃过
 * "同一规则两份实现"的亏（见 `think.ts` 的头注释），所以抽成组件。
 *
 * 默认折叠：模型能吐几千字，展开才看。**展示的是原文**，只去掉 markdown 加粗星号。
 */
defineProps<{ text: string; streaming?: boolean }>()

function plain(s: string): string {
  return s.split('**').join('')
}
</script>

<template>
  <details class="think">
    <summary>
      <span class="tk">思考过程</span> {{ text.length }} 字<span v-if="streaming" class="thinking">正在思考…</span>
    </summary>
    <pre>{{ plain(text) }}</pre>
  </details>
</template>

<style scoped>
.think {
  margin: 0 0 8px;
  background: var(--ctp-crust);
  border-left: 3px solid var(--ctp-surface2);
  border-radius: 0 6px 6px 0;
  padding: 6px 10px;
}
.think summary {
  font-size: 10.5px; color: var(--ctp-overlay0); cursor: pointer; user-select: none; list-style: none;
}
.think summary:hover { color: var(--ctp-subtext0); }
.think summary::-webkit-details-marker { display: none; }
.think summary::before { content: '▸ '; }
.think[open] summary::before { content: '▾ '; }
.tk { color: var(--ctp-subtext0); font-weight: 600; }
.think pre {
  margin: 8px 0 0; padding-top: 8px; border-top: 1px dashed var(--ctp-surface0);
  white-space: pre-wrap; word-break: break-word;
  font-family: ui-monospace, monospace; font-size: 10.5px;
  color: var(--ctp-overlay0); line-height: 1.6;
}
.thinking { color: var(--ctp-blue); margin-left: 6px; }
</style>
