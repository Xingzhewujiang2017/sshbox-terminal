import { createApp } from 'vue'
import { createPinia } from 'pinia'
import '@xterm/xterm/css/xterm.css'
import './assets/main.css'
import './assets/theme.css'
import { primeTheme } from './theme'

// 首帧前定色：settings.json 要异步读，先用上次的解析结果，避免闪一下深色再变白。
primeTheme()
import App from './App.vue'

createApp(App).use(createPinia()).mount('#app')
