# CLAUDE.md

本文件为 Claude Code 在此仓库工作时提供指引。

## 架构

一个把 agent pane（Claude Code）的会话过程流到 Telegram 的 herdr 插件——**单向只读**：transcript 是唯一数据源，Telegram 只是纯输出频道；没有任何 Telegram → herdr 的通路：bot 对手机而言是一个只发不收的账号。 herdr 的角色只有两个：`herdr agent list` 发现 pane 与定位 transcript，`[[startup]]` 钩子拉起守护进程。 本插件不消费 herdr 的状态事件（`pane.agent_status_changed`）——完成就是流里最后一条 text，卡住就是流里最后一条 AskUserQuestion / ExitPlanMode 工具调用。

插件本体是一个 Rust 单文件二进制 `htb`（`cargo build --release` 后用户放 PATH，无 `[[build]]` 段），加一个中继 Worker：

- **`herdr-plugin.toml`** —— 清单，声明钩子与动作；钩子全部经 PATH 上的 `htb <子命令>` 拉起：
  - `[[startup]]` 在 herdr 启动时跑一次 `ensure`（拉起守护进程）
  - `[[actions]]` 注册 `test` / `status` / `restart` 菜单动作

- **`Cargo.toml`** —— 依赖四族：`ureq 3`（rustls、blocking；`json` 与 `socks-proxy` 两个 feature 必须显式开——默认只有 rustls+gzip）、`serde`/`serde_json`（`preserve_order` 保对象键原序，tool 参数按原序显示）、`toml`；dev 只有 tempfile。无 tokio、无 libc、无 signal-hook、无 clap。

- **`src/`** —— 模块导入方向无环：

  ```text
  config, logging, lock → render(footer) → resolve, tailer → herdr, telegram → spawn, feed → main
  ```

  | 模块          | 职责 |
  | ------------- | ---- |
  | `main.rs`     | CLI 分发：daemon/ensure/stop/restart/status/test（手写 match，argparse 也不引入） |
  | `config.rs`   | env/XDG 目录解析、TOML 现读（**`toml::Table` 才是文档解析——`Value::from_str` 解析的是单个值**）、DEFAULTS 合并、`usable()` 门禁；`device` 必填 |
  | `logging.rs`  | JSONL 行 → stderr + flush；RFC3339 毫秒时间戳手写（Hinnant civil 算法），无 time 依赖 |
  | `lock.rs`     | `File::try_lock` flock 单例 + pid 读写；`stop_cli` 以**锁释放为唯一判据**（kill 后轮询 try_lock，10s 超时报拒绝） |
  | `spawn.rs`    | `ensure`：`current_exe` spawn 守护进程，stderr append 接管到 `<state>/log.jsonl`（接管前超 10MB truncate） |
  | `herdr.rs`    | `herdr agent list` 子进程（stdout 管道读线程防死锁 + 30s 超时 kill）；错误以返回值呈现——herdr 失败时退出码为 0、错误在 stdout 的 `{"error":{...}}` 里 |
  | `resolve.rs`  | transcript 定位（纯同步）：id glob → cwd 编码 store 兜底 → mtime → `last_reply_at` 新鲜度评分（300s 窗）；`Agent`/`AgentSession` serde 类型也在这 |
  | `tailer.rs`   | 内存游标 `HashMap<PathBuf, u64>`：EOF 起、完整行、`min(offset,size)` **写回**（截断自愈） |
  | `render.rs`   | Row/Block serde 建模 + 分类/合并/截断 → `Vec<Outbound>`；`Target`/`Usage`/`Kind` 定义处 |
  | `footer.rs`   | footer 模板渲染：零值藏段（label 只在本段字面里找，不碰已发射文本）、token K 化、空行消失、【】清空 |
  | `telegram.rs` | ureq POST + 重试分类 + redact + `send_text`/`send_rich`/`split_text`；`classify()` 泛型注入 transport 与 sleep 供测试 |
  | `feed.rs`     | 守护进程主循环：门禁 → listing 缓存/stale → resolve/tail/render → 按 pane 分桶 `thread::scope` 并发发送；rich 失败降级 plain |

- **`tests/lock_integration.rs`** —— 子进程级（`CARGO_BIN_EXE_htb`）：第二实例退出、stop 的 flock 判据、拒绝路径（pid 篡改后 10s 超时报非零）。

- **`relay/`** —— 所有 Telegram 调用经过的 Cloudflare Worker（`worker.js`、`deploy.sh`、`migrations.sql`；部署见 `relay/README.md`）。 bot token 只存在 Worker 的 secrets 里。部署与 D1 变更一律 `relay/deploy.sh` / curl 直调 CF API，凭据在仓库根 `.env`；**不要用 MCP cloudflare-api**——脚本内容手抄易损坏、响应形状多变，坑多。

### 中继契约（纯出站代理）

设备侧接口就是 Bot API：RELAY_KEY 占据 token 位，设备名放在路径前缀（Unicode 字母/数字/`_`/`-`，1–64 字符，中文可用，URL 百分号编码；中继先 decodeURIComponent 再校验）——任何 SDK 直连可用。

```text
POST /d/<device>/bot<RELAY_KEY>/<method>   # 设备出站，白名单转发 Telegram
```

白名单：`getMe`、`sendMessage`、`sendRichMessage`、`deleteWebhook`，其余方法 400。转发把 Telegram 的 **HTTP 状态原样带回**（429 限流最要紧，丢了状态码设备侧会把 429 读成解析失败，一秒一轮硬打 Telegram、永远出不了限流窗）。 D1 只有 `api_calls` 审计表；**审计写失败只打日志、绝不 503**。 cron 每日清理 30 天前的审计行。详见 `relay/README.md` 与仓库根 `.env`（CF 凭据 + BOT_TOKEN，**绝不能输出内容**）。

塑造了代码的关键约束：

- 多台设备共用一个 bot token 是硬需求；插件只发**出站**请求——无隧道、无端口、无入站。
- bot 对手机是纯输出：没有 webhook、没有 getUpdates、没有按钮回调。
- 守护进程由 flock 单例守卫；SIGTERM/SIGINT/SIGHUP 全走**默认终止**（不装信号 handler），内核释放锁——优雅退出协议不存在：游标全在内存，重启从 EOF 起本来就是设计。

## 工作原理

### 过程详情（render.rs + feed.rs，仅 Claude Code）：唯一的推送管线

把 transcript 当日志 tail——字节偏移游标存内存（`tailer::Tails`），**键是 transcript 路径**，只消费完整行；首次 attach 或 session 轮换一律从 EOF 起，绝不回放历史。

键是路径而不是 pane：`session_file()` 会让多个 pane 解析到同一个文件（compaction 后父会话与后继并存），路径计键天然去重——同一路径一轮只 tail 一次，第一个到达的 pane 拥有该批消息的 footer。

**serde 把 transcript 过滤规则编码成类型保证**：`#[serde(tag = "type")] enum Row` 只认顶层 `"type": "assistant"`——`api-request-blob`（Claude Code 为校验 prompt cache 而哈希的整轮请求，把 thinking/text/tool_use 原样重放在 role "assistant" 下且无 timestamp/usage/uuid）落入 `#[serde(other)] Ignored`，重放防御不依赖运行时检查；`enum Block` 同理吃掉 tool_result（永不发送）。**注意 serde tag 匹配区分大小写**：变体必须 `rename` 成小写 `"assistant"`，且 `#[serde(tag="type")]` 属性本身别漏写——漏了就退化成外部标签表示，整行解析失败返回 None（测试全空，症状是「什么都没推」）。每条 thinking/text 独立成条（thinking 截断 RICH-500），tool_use → 名字+JSON 围栏（参数全文、缩进排版、preserve_order 保序，超长由发送端 `split_text` 分条，绝不静默截断）且相邻合并至 RICH-300。 `AskUserQuestion` / `ExitPlanMode` 没有 特例——它们作为普通 tool_use 流出去。

每条消息 rich 先发（`{body}\n\n<footer>{footer 换行→<br/>}</footer>`），被拒降级 plain（`{body}\n\n{footer}`）——**这是唯一安全的重试**（Telegram 可能已受理 rich）。

### 守护进程主循环（feed.rs）

单线程采集循环，每 1s 一轮（`POLL_SECONDS`）——每轮现读配置 → listing 过期（≥1s）就刷一次 `herdr agent list`（**刷新失败静默**：继续用冻结列表扫，不写日志——1s 节奏下掉线每秒一行 JSONL 是纯磁盘损耗，掉线去 herdr 自身查，恢复自动）→ 逐 pane resolve/tail/render → 按 pane 分桶 `thread::scope` 并发发送（**组内串行保序**=Telegram 到达顺序即阅读顺序，**组间并行**=一条慢 lane 不拖垮别的 pane——每条消息 RTT 几百毫秒，串行发送在多个活跃会话下会超周期；sleep `saturating_sub` 归零即下轮立即开跑，1s 是节奏下限不是死线）。1s 节奏**零磁盘写**：游标全在内存、无状态文件，唯一持续写是发送日志（内容驱动）；resolve 重评分读经 **mtime 门控缓存**（`Resolver.scores`，`path → (mtime, 评分)`）——文件没变评分不可能变，未变文件只付一次 stat，稳态下每轮只重扫真正在被写的 transcript（~2MB × 活跃 pane 数，全页缓存读）。缓存以 store 里的 session 文件数为界、永不淘汰（几百条目封顶）；**给评分加绕过缓存的新数据源时，记得 mtime 必须同步变**，否则缓存会把旧评分供到底。相邻 tool_use 合并窗口=1 个周期，1s 下工具密集回合的独立消息数比 5s 时多。配置分两级：**load 失败（文件读不了、TOML 语法坏、chat_id/topic_id 形状不合）记日志后 daemon 直接退出**——输错配置是操作者错误，空转的 daemon 在 `htb status` 里和正常工作的一模一样；缺 relay 或 `agent_details` 为空只是「没配好/没人订阅」，按普通节奏空转。没有探活请求：每个发送都现读配置独立成调用，中继宕机时只有真实待发消息才产生重试。

### 发送（telegram.rs）

`classify()` 的分支顺序是锁死的语义（测试逐条锁定）：传输错误与 5xx → 重试（退避 1+attempt 秒）；**429 独立分支且排在 ok:false 快败之前**——429 的响应体带 ok:false，先撞快败分支就永远到不了限流等待，一秒一轮硬打 Telegram、永远出不了限流窗；4xx 快败；200 无 `result` 快败（Telegram 已受理，重试即重复发送）。429 睡满 `parameters.retry_after` +1 秒再试。每 attempt 整体 `timeout_global` 40s（覆盖 DNS 解析——getaddrinfo 挂死这类阶段级超时盖不住的挂点也在它的兜底范围内）。UA 设 `htb`（CF 1010 封 well-known 客户端签名）。`redact()` 把错误文本里的 relay key 抹成 `<relay-key>` 再落日志。代理支持 HTTP（CONNECT 隧道）与 SOCKS5（`socks-proxy` feature），解析位置遵循 scheme 语义（对齐 curl，不做任何额外转换）：`socks5://` 本地解析目标主机名、`socks5h://` 交给代理解析——本地解析这条路在本机网络下往往正是要绕开的那条，选 `socks5h://`。

### transcript 定位（resolve.rs）

读 Claude Code 自己的 session jsonl（`~/.claude/projects/`），由 herdr 上报的 session id 定位。 herdr 的 id 会失效（compaction/resume 轮换文件而 herdr 不更新 id，旧文件 mtime 还活着但最后真实回复冻结）——候选按「最后一条 assistant 文本的时间」评分（`last_reply_at` 读尾部 2MB 倒序找；`api-request-blob` 无 timestamp 且 type 不合，天然不计数），5 分钟内有发言才算新鲜，否则回退 mtime 最新的 id 匹配文件；ref 缺失时按 cwd 编码的 store 目录兜底（**编码 = 非字母数字字符（按码点）替换成 '-'**）。插件 claude-only：`agent != "claude"` 的 pane 在主循环即被跳过。

## 命令

```sh
cargo build --release             # 编译后放 ~/bin（用户手动）
cargo test                        # 68 单测 + 2 子进程集成测试
cargo clippy --all-targets        # 零警告

htb ensure|daemon|stop|restart|status|test

herdr plugin action invoke htb.test        # 经 herdr 的同一批入口
herdr plugin logs --plugin htb             # herdr 侧命令执行日志（不是 log.jsonl）
```

调试：

- 日志是插件状态目录的 `log.jsonl`（每行一个 JSON 对象：`ts` / `msg` 必有，发送类记录再带 `content` / `pane` / `agent` / `source`），超 10MB 在 `ensure` 接管前截断。
- 手动前台跑：`HERDR_PLUGIN_STATE_DIR=… HERDR_PLUGIN_CONFIG_DIR=… htb daemon`（env 不设则回退 XDG 布局，与生产共享同一份状态）。

## 配置

`config.toml` 在插件配置目录（不入库，已 gitignore）。复制编辑：

```sh
cp config.example.toml "$(herdr plugin config-dir htb)/config.toml"
```

样例注释了全部键；开发最相关：`relay_url` / `relay_key` / `chat_id`（**-100 开头的负整数**，超级群 id）/ `topic_id`（**正整数**，本机消息落在哪个话题）/ `agent_details`（推什么，空=不推）/ `footer_format`（可含 token 用量占位符）/ `device`（**必填**，每机唯一）/ `proxy`（HTTP 或 SOCKS5 代理 URL，空串=直连）。`chat_id`/`topic_id` 宽松读取（TOML 整数或能解析的字符串，`required_i64`）但**形状硬校验**：不合即 `Config::load` 返回 Err、daemon 记日志退出——它们是用户手打的 id，静默吞掉一个坏值就是让 feed 无声死亡（字符串 topic_id 曾静默折叠成 0，`htb status` 全绿却一条不推）；其余键原生 TOML 类型、缺失走 DEFAULTS。配置每轮现读，改完无需重启。

## 状态文件

都在 `HERDR_PLUGIN_STATE_DIR`（herdr 注入；裸跑回退 XDG）：

| 文件        | 写入者 | 内容 |
| ----------- | ------ | ---- |
| `bot.lock`  | daemon | flock 单例锁 + pid |
| `log.jsonl` | daemon | JSONL 日志（stderr 接管；超 10MB 接管前截断） |

游标在内存（`Tails`），无状态文件——重启从 EOF 起流是设计。

## 中继的 D1 调试面

排查「发送失败」看中继的 D1（Cloudflare dashboard 的 D1 console，或 curl POST `/accounts/<id>/d1/database/<uuid>/query`，凭据在 `.env`）。 `api_calls` 表按 created_at 保留 30 天，记录全部出站 Bot API 调用：参数、成败、error_code/description、耗时——「发送失败」直接读 Telegram 的原话；duration_ms 异常高说明网络抖动。D1 免费层的关键坑位：每日 500 万行读取是**账号级**（账号下所有库共用，UTC 0 点重置）、**按库记账、账号级判罚**；读 0 行的查询（`SELECT 1`、空表）在超额后照样成功，用它们探额度无效；额度烧完时能取数的路只有 `POST /d1/database/{uuid}/export`（polling 的 bookmark 要放 POST body 的 `current_bookmark` 里）；行读取按扫描计而非按返回计。错误签名：D1 失败时 worker 只在日志里留 `audit write failed: …`，出站转发不受影响——**不要看到发送成功就以为 D1 也健康**，排查审计断档要直接查表或看 `wrangler tail`。Workers Free 另有 10 万请求/天（超限报 `1027`）。

## Rust 惯例

- MSRV 1.89（`File::try_lock`）；edition 2024。
- 外部 API（herdr CLI、Telegram）的错误以**返回值**呈现——`herdr::agent_list` 返回 `Result<Vec<Agent>, String>`，`telegram::call` 返回 `Result<Value, String>`。调用方检查并优雅降级，不向上传播。
- 测试注入用泛型闭包（`classify` 的 post/sleep），不引入 mock 框架。
- 竞态相关的注释紧贴逻辑；改游标推进顺序、label 切割、重试分支顺序的任何一行之前，先读懂对应注释里的为什么。

## 仓库红线

- `.env` 含 CF 凭据与 BOT_TOKEN：绝不输出内容；查变量名只用 `grep -oE "^[0-9a-zA-Z_]+=" .env`。
- relay key 不回显：curl 调试用 shell 变量拼接，不写进命令行字面量。
