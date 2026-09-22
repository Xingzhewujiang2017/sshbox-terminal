<script setup lang="ts">
import { computed, ref, watch } from 'vue'
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
  (e: 'reorderGroups', order: string[]): void
}>()

const query = ref('')
const searchEl = ref<HTMLInputElement | null>(null)
/** 清空后把焦点留在输入框 —— 接着就能打字，不用再点一下。 */
function clearSearch() {
  query.value = ''
  searchEl.value?.focus()
}
const collapsed = ref<Record<string, boolean>>({})
/** 是否显示被合并的重复配置（默认合并，点了才展开）。 */
const showAll = ref(false)

/** 同 host+port+user 只显示第一条：hosts.json 里复制出来的重复条目
 *  （hostId 不同但指向同一台机器）会在主页列表、演练台出现 N 个同名行，
 *  连接还会开出一堆重复标签。按连接目标去重，保留首次出现的那条。 */
const deduped = computed(() => {
  const seen = new Set<string>()
  const out: Host[] = []
  for (const h of props.data.hosts ?? []) {
    const key = `${h.username}@${h.host}:${h.port ?? 22}`
    if (seen.has(key)) continue
    seen.add(key)
    out.push(h)
  }
  return out
})

/** 被去重隐藏的条数（数据仍在 hosts.json 里，只是列表不重复显示）。 */
const mergedCount = computed(() => (props.data.hosts?.length ?? 0) - deduped.value.length)

/** 默认合并显示；用户点「显示全部」后按原样列出，避免"我建的主机不见了"。 */
const shown = computed(() => (showAll.value ? (props.data.hosts ?? []) : deduped.value))

const filtered = computed(() => {
  const q = query.value.trim().toLowerCase()
  if (!q) return shown.value
  return shown.value.filter(
    (h) =>
      h.name.toLowerCase().includes(q) ||
      h.host.toLowerCase().includes(q) ||
      h.username.toLowerCase().includes(q) ||
      h.group.toLowerCase().includes(q)
  )
})

/** 分组顺序取自 hosts.json 的 groups 数组（拖动排序会写回去），没记录的排后面。 */
function groupOrder(): string[] {
  return props.data.groups ?? []
}

const groups = computed(() => {
  const map = new Map<string, Host[]>()
  for (const h of filtered.value) {
    const g = h.group || '默认'
    if (!map.has(g)) map.set(g, [])
    map.get(g)!.push(h)
  }
  const order = groupOrder()
  const rank = (g: string) => {
    const i = order.indexOf(g)
    return i < 0 ? order.length : i
  }
  return [...map.entries()].sort((a, b) => rank(a[0]) - rank(b[0]) || a[0].localeCompare(b[0], 'zh'))
})

// 首次拿到数据时：第一个分组展开，其余默认收起 —— 分组一多，全部铺开列表会很长。
const inited = ref(false)
watch(
  () => props.data,
  (d) => {
    if (inited.value || !d?.hosts?.length) return
    const names = [...new Set(d.hosts.map((h) => h.group || '默认'))]
    const order = groupOrder()
    const rank = (g: string) => {
      const i = order.indexOf(g)
      return i < 0 ? order.length : i
    }
    names.sort((a, b) => rank(a) - rank(b) || a.localeCompare(b, 'zh'))
    names.slice(1).forEach((g) => (collapsed.value[g] = true))
    inited.value = true
  },
  { immediate: true }
)

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

// --- 分组拖动排序 -----------------------------------------------------------
// Tauri 开着原生 drag-drop（OS 拖文件进窗口要用），页面里的 HTML5 dragstart
// 收不到，所以内部拖动一律用指针事件自己算：按下记名字，移动超过 4px 才算拖动
// （否则普通点击被吃掉），按指针命中的分组头算落点，松手提交。
const dragFrom = ref<string | null>(null)
const dragTo = ref<string | null>(null)
let downY = 0
let dragging = false

function onGroupDown(g: string, ev: MouseEvent) {
  dragFrom.value = g
  downY = ev.clientY
  dragging = false
  window.addEventListener('mousemove', onGroupMove)
  window.addEventListener('mouseup', onGroupUp)
}

function onGroupMove(ev: MouseEvent) {
  if (!dragFrom.value) return
  if (!dragging && Math.abs(ev.clientY - downY) < 4) return
  dragging = true
  const heads = [...document.querySelectorAll<HTMLElement>('.group-head')]
  const hit = heads.find((el) => {
    const r = el.getBoundingClientRect()
    return ev.clientY >= r.top && ev.clientY <= r.bottom
  })
  dragTo.value = hit?.dataset.group ?? null
}

function onGroupUp() {
  window.removeEventListener('mousemove', onGroupMove)
  window.removeEventListener('mouseup', onGroupUp)
  const from = dragFrom.value
  const to = dragTo.value
  if (dragging && from && to && from !== to) {
    const names = groups.value.map(([g]) => g)
    const i = names.indexOf(from)
    const j = names.indexOf(to)
    if (i >= 0 && j >= 0) {
      names.splice(j, 0, ...names.splice(i, 1))
      emit('reorderGroups', names)
    }
  }
  dragFrom.value = null
  dragTo.value = null
  dragging = false
}

/** 拖动结束的那一下不要顺手把分组折叠了。 */
function onGroupClick(g: string) {
  if (dragging) return
  toggle(g)
}
</script>

<template>
  <aside class="sidebar" @click="closeMenu">
    <div class="head">
      <div class="logo">SSH<span>Box</span></div>
      <button class="icon" title="设置" @click="emit('settings')">⚙</button>
    </div>

    <button class="connect-btn" @click="emit('new')">＋ 新建连接</button>

    <div class="search-wrap">
      <input
        ref="searchEl"
        v-model="query"
        class="search"
        placeholder="搜索主机 / 地址 / 用户"
        @keydown.esc="clearSearch"
      />
      <button v-if="query" class="search-clear" title="清空（Esc）" @click="clearSearch">×</button>
    </div>

    <div v-if="mergedCount > 0 && !query" class="merge-hint">
      <span>已合并 {{ mergedCount }} 条指向相同目标的配置</span>
      <button class="merge-toggle" @click="showAll = !showAll">
        {{ showAll ? '合并显示' : '显示全部' }}
      </button>
    </div>

    <div class="list">
      <div v-if="!filtered.length" class="empty-hint">
        {{ data.hosts.length ? '没有匹配的主机' : '还没有主机，点「新建连接」添加' }}
      </div>
      <template v-for="[group, hosts] in groups" :key="group">
        <div
          class="group-head"
          :class="{ dragging: dragFrom === group, 'drop-target': dragTo === group && dragFrom !== group }"
          :data-group="group"
          :title="`${group} · 拖动可调整分组顺序`"
          @mousedown="onGroupDown(group, $event)"
          @click="onGroupClick(group)"
        >
          <span class="grip">⠿</span>
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
.search-wrap { position: relative; }
.search-wrap .search { width: 100%; box-sizing: border-box; padding-right: 26px; }
/* × 给足 20px 热区：13px 的字号直接点很难点中 */
.search-clear {
  position: absolute; right: 3px; top: 50%; transform: translateY(-50%);
  width: 20px; height: 20px; display: flex; align-items: center; justify-content: center;
  background: none; border: none; border-radius: 4px; padding: 0;
  color: var(--ctp-overlay0); font-size: 14px; line-height: 1; cursor: pointer;
}
.search-clear:hover { color: var(--ctp-text); background: var(--ctp-surface0); }
.merge-hint {
  display: flex; align-items: center; justify-content: space-between; gap: 6px;
  margin: 6px 0 2px; padding: 5px 8px; border-radius: 6px;
  background: var(--ctp-surface0); color: var(--ctp-subtext0); font-size: 11px;
}
.merge-toggle {
  background: none; border: none; padding: 0; cursor: pointer;
  color: var(--ctp-blue); font-size: 11px; text-decoration: underline;
}
.merge-toggle:hover { color: var(--ctp-sapphire); }
.list { flex: 1; overflow-y: auto; margin: 0 -4px; padding: 0 4px; }
.empty-hint { color: var(--ctp-surface1); font-size: 11px; padding: 10px 2px; line-height: 1.6; }
.group-head {
  display: flex; align-items: center; gap: 5px; cursor: pointer;
  color: var(--ctp-overlay0); font-size: 11px; padding: 5px 2px 3px; user-select: none;
}
.group-head:hover { color: var(--ctp-subtext0); }
.grip { font-size: 10px; color: var(--ctp-surface2); cursor: grab; letter-spacing: -1px; }
.group-head:hover .grip { color: var(--ctp-overlay0); }
.group-head.dragging { color: var(--ctp-blue); opacity: 0.7; }
.group-head.drop-target { box-shadow: inset 0 -2px 0 var(--ctp-blue); }
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
