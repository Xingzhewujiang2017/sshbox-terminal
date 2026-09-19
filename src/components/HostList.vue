<script setup lang="ts">
import { computed, ref } from 'vue'
import type { Host, HostsFile } from '../api'

const props = defineProps<{
  data: HostsFile
  activeHostId?: string | null
  busyId?: string | null
}>()

const emit = defineEmits<{
  (e: 'connect', host: Host): void
  (e: 'edit', host: Host): void
  (e: 'delete', host: Host): void
  (e: 'duplicate', host: Host): void
  (e: 'new'): void
  (e: 'settings'): void
}>()

const query = ref('')
const collapsed = ref<Record<string, boolean>>({})

const filtered = computed(() => {
  const q = query.value.trim().toLowerCase()
  if (!q) return props.data.hosts
  return props.data.hosts.filter(
    (h) =>
      h.name.toLowerCase().includes(q) ||
      h.host.toLowerCase().includes(q) ||
      h.username.toLowerCase().includes(q) ||
      h.group.toLowerCase().includes(q)
  )
})

const groups = computed(() => {
  const map = new Map<string, Host[]>()
  for (const h of filtered.value) {
    const g = h.group || '默认'
    if (!map.has(g)) map.set(g, [])
    map.get(g)!.push(h)
  }
  return [...map.entries()].sort((a, b) => a[0].localeCompare(b[0], 'zh'))
})

const menu = ref<{ host: Host; x: number; y: number } | null>(null)

function openMenu(host: Host, ev: MouseEvent) {
  ev.preventDefault()
  menu.value = { host, x: ev.clientX, y: ev.clientY }
}
function closeMenu() {
  menu.value = null
}
function toggle(g: string) {
  collapsed.value[g] = !collapsed.value[g]
}
</script>

<template>
  <aside class="sidebar" @click="closeMenu">
    <div class="head">
      <div class="logo">SSH<span>Box</span></div>
      <button class="icon" title="设置" @click="emit('settings')">⚙</button>
    </div>

    <button class="connect-btn" @click="emit('new')">＋ 新建连接</button>

    <input v-model="query" class="search" placeholder="搜索主机 / 地址 / 用户" />

    <div class="list">
      <div v-if="!filtered.length" class="empty-hint">
        {{ data.hosts.length ? '没有匹配的主机' : '还没有主机，点「新建连接」添加' }}
      </div>
      <template v-for="[group, hosts] in groups" :key="group">
        <div class="group-head" @click="toggle(group)">
          <span class="caret">{{ collapsed[group] ? '▸' : '▾' }}</span>
          <span class="group-name">{{ group }}</span>
          <span class="count">{{ hosts.length }}</span>
        </div>
        <div v-show="!collapsed[group]" class="group-body">
          <div
            v-for="h in hosts"
            :key="h.id"
            class="host"
            :class="{ active: h.id === activeHostId, busy: h.id === busyId }"
            :title="`${h.username}@${h.host}:${h.port}`"
            @dblclick="emit('connect', h)"
            @contextmenu="openMenu(h, $event)"
          >
            <span class="dot" :style="{ background: h.color || 'var(--ctp-blue)' }"></span>
            <span class="host-text">
              <span class="host-name">{{ h.name || h.host }}</span>
              <span class="host-sub">{{ h.username }}@{{ h.host }}<template v-if="h.port !== 22">:{{ h.port }}</template></span>
            </span>
            <span v-if="h.auth === 'key'" class="tag" title="私钥认证">🔑</span>
            <span v-if="h.save_password" class="tag" title="已保存密码">🔒</span>
            <span v-if="h.id === busyId" class="spinner"></span>
            <button class="play" title="连接" @click.stop="emit('connect', h)">▶</button>
          </div>
        </div>
      </template>
    </div>

    <div class="foot">
      <span class="hint">双击连接 · 右键更多</span>
    </div>

    <div
      v-if="menu"
      class="ctx"
      :style="{ left: menu.x + 'px', top: menu.y + 'px' }"
      @click.stop
    >
      <div class="ctx-item" @click="emit('connect', menu!.host); closeMenu()">连接</div>
      <div class="ctx-item" @click="emit('edit', menu!.host); closeMenu()">编辑</div>
      <div class="ctx-item" @click="emit('duplicate', menu!.host); closeMenu()">复制一份</div>
      <div class="ctx-sep"></div>
      <div class="ctx-item danger" @click="emit('delete', menu!.host); closeMenu()">删除</div>
    </div>
  </aside>
</template>

<style scoped>
.sidebar {
  width: 208px;
  flex-shrink: 0;
  background: var(--ctp-crust);
  border-right: 1px solid var(--ctp-surface0);
  padding: 10px;
  display: flex;
  flex-direction: column;
  gap: 8px;
  position: relative;
}
.head { display: flex; align-items: center; justify-content: space-between; }
.logo { font-size: 17px; font-weight: 700; color: var(--ctp-text); }
.logo span { color: var(--ctp-blue); }
.icon {
  background: none; border: none; color: var(--ctp-overlay0); font-size: 15px; cursor: pointer;
}
.icon:hover { color: var(--ctp-text); }
.connect-btn {
  background: var(--ctp-blue); color: var(--on-accent); border: none; border-radius: 6px;
  padding: 7px 10px; font-size: 12.5px; font-weight: 600; cursor: pointer;
}
.connect-btn:hover { background: var(--ctp-lavender); }
.search {
  background: var(--ctp-mantle); border: 1px solid var(--ctp-surface0); border-radius: 6px;
  color: var(--ctp-text); padding: 5px 8px; font-size: 12px; outline: none;
}
.search:focus { border-color: var(--ctp-blue); }
.list { flex: 1; overflow-y: auto; margin: 0 -4px; padding: 0 4px; }
.empty-hint { color: var(--ctp-surface1); font-size: 11px; padding: 10px 2px; line-height: 1.6; }
.group-head {
  display: flex; align-items: center; gap: 5px; cursor: pointer;
  color: var(--ctp-overlay0); font-size: 11px; padding: 5px 2px 3px; user-select: none;
}
.group-head:hover { color: var(--ctp-subtext0); }
.caret { font-size: 9px; }
.group-name { flex: 1; text-transform: uppercase; letter-spacing: 0.4px; }
.count { color: var(--ctp-surface1); }
.host {
  display: flex; align-items: center; gap: 6px; padding: 5px 6px;
  border-radius: 5px; cursor: pointer; position: relative;
}
.host:hover { background: var(--ctp-base); }
.host.active { background: var(--ctp-base); box-shadow: inset 2px 0 0 var(--ctp-blue); }
.host.busy { opacity: 0.75; }
.dot { width: 6px; height: 6px; border-radius: 50%; flex-shrink: 0; }
.host-text { flex: 1; min-width: 0; display: flex; flex-direction: column; }
.host-name {
  font-size: 12.5px; color: var(--ctp-text); overflow: hidden;
  text-overflow: ellipsis; white-space: nowrap;
}
.host-sub {
  font-size: 10px; color: var(--ctp-overlay0); overflow: hidden;
  text-overflow: ellipsis; white-space: nowrap;
}
.tag { font-size: 9px; opacity: 0.8; }
.play {
  background: none; border: none; color: var(--ctp-overlay0); cursor: pointer;
  font-size: 10px; padding: 0 2px; opacity: 0;
}
.host:hover .play { opacity: 1; }
.play:hover { color: var(--ctp-green); }
.spinner {
  width: 10px; height: 10px; border: 1.5px solid var(--ctp-surface1);
  border-top-color: var(--ctp-blue); border-radius: 50%; animation: spin 0.8s linear infinite;
}
@keyframes spin { to { transform: rotate(360deg); } }
.foot { border-top: 1px solid var(--ctp-surface0); padding-top: 6px; }
.hint { color: var(--ctp-surface1); font-size: 10px; }
.ctx {
  position: fixed; z-index: 200; background: var(--ctp-base); border: 1px solid var(--ctp-surface0);
  border-radius: 6px; padding: 4px; min-width: 120px; box-shadow: 0 6px 20px var(--mask);
}
.ctx-item { padding: 5px 10px; font-size: 12px; color: var(--ctp-text); border-radius: 4px; cursor: pointer; }
.ctx-item:hover { background: var(--ctp-surface0); }
.ctx-item.danger { color: var(--ctp-red); }
.ctx-sep { height: 1px; background: var(--ctp-surface0); margin: 4px 2px; }
</style>
