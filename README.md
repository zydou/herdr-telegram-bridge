# htb

一个把 agent pane（Claude Code）的会话过程流到 Telegram 的 herdr 插件——**单向只读**：transcript 是唯一数据源，Telegram 只是纯输出频道。没有任何 Telegram → herdr 的通路（回复、命令、审批按钮都不存在），bot 对手机而言是一个只发不收的账号。

htb 是一个单文件 Rust 二进制：`cargo build --release` 后把 `target/release/htb` 放进 PATH（如 `~/bin`），herdr 的钩子经 PATH 拉起它。无 `[[build]]` 段——升级就是重新编译替换二进制。

## 它推送什么

把 transcript 当日志 tail：每条 thinking、每条 text 各自成一条消息（💭 / ⏳），相邻 tool_use 合并成一条（🔧，参数全量不截断），tool_result 永不发送。`AskUserQuestion` / `ExitPlanMode` 作为普通 tool_use 流出去——这正是「agent 卡住等输入」在流里的可见形态。每条消息以 footer 收尾（标题、设备、目录、token 用量），模板可配。

多台设备共用一个 bot token：所有出站 Telegram 调用经过 [herdr-relay](relay/README.md) Cloudflare Worker（纯出站代理，白名单转发），bot token 只存在 Worker 的 secrets 里。

## 安装

```sh
cargo build --release
cp target/release/htb ~/bin/          # 任意 PATH 目录
herdr plugin link ~/Projects/herdr/herdr-telegram-bridge
```

配置（必填：`device`、`chat_id`、`topic_id`、`relay_url`、`relay_key`）：

```sh
cp config.example.toml "$(herdr plugin config-dir htb)/config.toml"
$EDITOR "$(herdr plugin config-dir htb)/config.toml"
```

herdr 启动时经 `[[startup]]` 钩子跑 `htb ensure` 拉起守护进程；也可手动：

```sh
htb ensure      # 未运行则启动守护进程
htb status      # 锁状态 + pid
htb stop        # 停止（flock 是唯一判据：真见到锁释放才算成功）
htb restart     # stop + ensure
htb test        # 发一条测试消息到中继
htb daemon      # 前台跑（开发用，日志直接落终端）
```

## 命令

```sh
cargo test              # 单测 + 子进程级 lock 集成测试
cargo clippy --all-targets
cargo build --release
```

## 调试

- 日志是插件状态目录的 `log.jsonl`（每行一个 JSON 对象：`ts` / `msg` 必有，发送类记录再带 `content` / `pane` / `agent` / `source`）。守护进程把 stderr 接到这个文件，接管前超 10MB 先截断；前台 `htb daemon` 的同一批日志直接落终端。
- 手动前台跑：`HERDR_PLUGIN_STATE_DIR=… HERDR_PLUGIN_CONFIG_DIR=… htb daemon`（env 不设则回退 XDG 布局，与生产共享同一份状态）。
- 排查「发送失败」看中继的 D1 `api_calls` 表（Telegram 的原话在里面），见 `relay/README.md`。

## 架构速览

单线程采集循环（5s 一轮）：现读配置 → 刷新 herdr pane 列表（失败时继续用冻结列表扫）→ 逐 pane 定位 transcript、tail 增量行、渲染成消息 → 按 pane 分组 `thread::scope` 并行发送（组内串行保序，组间并行）。游标全在内存，重启从 EOF 起绝不回放。守护进程持 flock 单例，`stop` 以锁释放为唯一成功判据。零全局状态文件、零信号 handler——SIGTERM 直接杀死进程，内核释放锁。

模块（导入方向无环）：

```text
config, logging, lock → render(footer) → resolve, tailer → herdr, telegram → spawn, feed → main
```
