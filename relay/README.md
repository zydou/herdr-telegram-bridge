# herdr-relay

bot token 只存在这个 Cloudflare Worker 的 secret 里，设备侧不持有。设备与它对话的接口就是**标准 Bot API 的形状**——RELAY_KEY 占据 token 位，设备名放在路径前缀里：

```text
POST /d/<device>/bot<RELAY_KEY>/<method>   # 设备出站，白名单转发 Telegram
```

任何 SDK 都能直连：`base_url` 设为 `https://…workers.dev/d/<device>/bot`、token 填 RELAY_KEY 即可，插件侧没有任何手写 HTTP 层。

**流量是严格单向的**：设备只往外推消息，没有任何入站。白名单只有 `getMe`（SDK initialize 时用一次）、`sendMessage`、`sendRichMessage`；`deleteWebhook` 也放行——它不是本地空应答，而是原样转发，专为撤掉 Telegram 侧配置过的 webhook（bot 若设过 webhook 而无人接收入站，Telegram 会一直往一个 404 推 update）。其余方法一律 400。

## D1 审计

库中只有一张 `api_calls` 表：每次设备调用，无论成败，记录参数、Telegram 的 error_code/description 与耗时。排查「发送失败」直接看 Telegram 的原话；`duration_ms` 异常高说明网络抖动。行按 `created_at` 保留 30 天，cron 每日清理（`idx_api_calls_created` 让 DELETE 走索引，否则全表扫会烧 D1 的按行读取额度）。

D1 免费层的每日 500 万行读取是**账号级**额度（所有库共用，UTC 0 点重置）；超额后任何会碰到行的读查询报 `code 7500`，写入不受限。审计写入失败**不会**破坏转发——Telegram 已受理的发送不能因为审计库挂了而对设备报 503，否则设备重试就是重复发送。看单条查询的真实成本用 `npx wrangler d1 insights herdr-relay --time-period 24h --json --limit 100`（`--limit` 默认 5 会静默截断）。

## 部署

日常部署一条命令——curl 直调 CF API，不依赖其他工具链：

```sh
relay/deploy.sh
```

凭据在仓库根 `.env`（gitignored）：`CLOUDFLARE_ACCOUNT_ID`、`CLOUDFLARE_API_TOKEN`。脚本上传 worker.js 并声明 D1 绑定；**BOT_TOKEN / RELAY_KEY 两个 secret 不进 metadata——API 拒收空值 secret（10021），而已存在的 secret 不随部署变动**。 cron 触发器是独立资源，不受影响。

改密钥用同一条路径：

```sh
echo "<新值>" | relay/deploy.sh secret BOT_TOKEN    # 或 RELAY_KEY；值经 stdin 传入，不落参数
```

首次搭建（一次性）：

```sh
# 建 D1（脚本按名字 herdr-relay 自动解析它的 uuid，无需手工记录）
curl -X POST ".../accounts/<id>/d1/database" -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" \
  -d '{"name":"herdr-relay"}'
# 导入表结构——注意 query 端点只认 uuid 不认名字，先按名查出 uuid：
curl -sS ".../accounts/<id>/d1/database/herdr-relay" -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN"
curl -X POST ".../accounts/<id>/d1/database/<uuid>/query" -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" \
  -H "content-type: application/json" \
  -d "$(python3 -c 'import json; print(json.dumps({"sql": open("migrations.sql").read()}))')"

# 上传脚本 + 放两个 secret（各一次；BOT_TOKEN 来自 @BotFather，RELAY_KEY 用 openssl rand -hex 24）
relay/deploy.sh
echo "<生成的 key>" | relay/deploy.sh secret RELAY_KEY
```

只读排查可用 `npx wrangler tail herdr-relay` 抓实时日志。

### 纯 dashboard 搭建（无终端时）

1. **D1**：dashboard → Storage & Databases → D1 → Create，名字 `herdr-relay`，然后在 Console 里执行 `migrations.sql` 的内容。
2. **Worker**：dashboard → Workers & Pages → Create → 选 "Deploy from dashboard" 粘贴 `worker.js`（模块 Worker）。在 Settings → Bindings 里加：
   - D1 绑定 `DB` → `herdr-relay`
   - Secret `BOT_TOKEN`、`RELAY_KEY`（Settings → Secrets）
3. **Cron**：Settings → Triggers → Cron Triggers → `17 3 * * *`（每日清理过期行）。

## 验证

```sh
# 未带 key 应被拒（401 Unauthorized —— 与真 Bot API 对坏 token 的回答一致）
curl -X POST "https://herdr-relay.<子域>.workers.dev/d/testdev/botWRONGKEY/getMe"

# 白名单外的方法被拒
curl -X POST "https://herdr-relay.<子域>.workers.dev/d/testdev/bot<RELAY_KEY>/getUpdates" \
  -H "content-type: application/json" -d '{}'
```

然后按插件主 README 配置各台设备的 `config.toml`（`relay_url` / `relay_key` / `device` / `chat_id` / `topic_id`）。

## 密钥轮换（revoke 之后怎么办）

| Secret      | 存在于                   | 泄露波及                                        | 设备侧要不要改 |
| ----------- | ------------------------ | ----------------------------------------------- | -------------- |
| `BOT_TOKEN` | Worker secret            | 有人能用你的 bot 说话、读不到历史但可收发新消息 | 不用           |
| `RELAY_KEY` | Worker secret + 每台设备 | 能借 bot 发消息                                 | **每台都要改** `relay_key` |

RELAY_KEY 同时出现在 Worker secret 和每台设备的配置里；它在 URL 路径上传输，所以只走 HTTPS，且插件会把诊断文本里的 key 抹掉再记日志。

**只换了 BOT_TOKEN**（@BotFather `/revoke`）：

```sh
echo "<新 token>" | relay/deploy.sh secret BOT_TOKEN
```

从 revoke 到更新 secret 之间，Worker 的出站调用全部失败——设备发不出任何消息，直到新 token 就位。设备侧零改动。

**只换了 RELAY_KEY**：没有权威颁发方，「revoke」就是自己换个新值（`openssl rand -hex 24`）：

```sh
echo "<新 key>" | relay/deploy.sh secret RELAY_KEY
```

然后把**每一台**设备的 `config.toml` 里 `relay_key` 改成新值——key 是全体设备共享的，漏改哪台哪台就连不上（日志里发送失败退避，改完即愈：配置现读，无需重启轮询器）。

**换群 / 换 bot**：中继无状态，两边都只是 Worker 侧换 secret、设备侧改 config（换群改各机的 `chat_id`/`topic_id`，新 bot 记得拉进群设为管理员）。

## 注意

- bot 的 token 只存在 Worker secrets 里；`.env` 里放的是 CF 的 API 凭据。
- 设备名允许 Unicode 字母/数字/`_`/`-`（1–64 字符，中文可用）；URL 里按标准百分号编码。它进的是 `api_calls` 的 device 列，用来区分哪台机器发出的调用。
- 消息内容会经过 Cloudflare；`api_calls` 表里暂存调用参数（含消息正文），30 天兜底清理。
