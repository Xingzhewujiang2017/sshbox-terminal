/**
 * 命令文本的清洗（无依赖模块 —— 与 think.ts 同套路：这样 `node --experimental-strip-types`
 * 能直接加载它做单测，见 tests/cmd.test.ts）。
 */

/**
 * 命令块插入/复制前的提示符清理：**逐行**去掉 `$ ` 提示符。
 *
 * 为什么不复用 ai.ts 的 cleanCommand：它只去整串开头的那一个 `$ `（`^\$\s+` 没有 m 标志），
 * 而模型常这样回多行命令：
 *     $ df -h
 *     $ free -h
 * 只剥第一行的话，第二行会带着 `$ ` 进终端 —— 回车后报 `$: command not found`。
 *
 * `#` / `>` **故意不剥**：`#` 在 shell 里是注释（`# 重启 nginx` 剥成 `重启 nginx`
 * 反而变成一条会报错的命令），`>` 是合法的重定向开头。宁可少剥，不可把正文改坏。
 * `$` 后面必须有空白才算提示符 —— 否则 `$HOME/bin/run` 会被剥成 `HOME/bin/run`。
 */
export function stripPromptPrefix(s: string): string {
  return s
    .split('\n')
    .map((l) => l.replace(/^\s*\$\s+/, ''))
    .join('\n')
    .trim()
}
