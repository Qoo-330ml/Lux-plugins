# 通知器插件参考

从 Lux 主仓库 `docs/PLUGIN-SDK.md` 迁入的各通知器说明（源码与 SDK 现在都在本仓库）。

### 通知器 SDK

`luxd::application::notifier_plugin_sdk` 提供通知插件共用的进程外壳，渠道插件只需实现上游认证和请求：

- `run_notifier(|request| async { ... })`：读取 stdin 的逐行 `PluginRequest`，把 `notification.send` 分发到
  处理器，写出 `PluginResponse`，并在 `plugin.shutdown` 后退出；无效请求行和未知方法返回错误响应但不会
  终止循环。需要自定义流时使用 `run_notifier_session(reader, writer, handler)`。
- `outbound_http_client(proxy_url)` / `outbound_http_client_from_env()`：统一的出站 client，读取 Lux 代理配置
  （`LUX_PROXY_URL`），10 秒超时且不跟随重定向。需要先钉住目的地地址时使用
  `outbound_http_client_builder(proxy_url)` / `outbound_http_client_builder_from_env()` 取到同一个配置好的
  `reqwest::ClientBuilder`，再调用 `GuardedDestination::pin(builder)` 后 `build()`。
- `guard_destination(&Url, allow_private_network)` / `guard_destination_str(&str, allow_private_network)`：解析并
  校验一个 HTTP(S) 目的地，返回 `GuardedDestination`（`url()`、`host()`、`address()`、`pin(builder)`）。
- `guard_host(host, port, allow_private_network)`：非 HTTP 渠道（SMTP、自定义 TCP）使用的 host:port 版本，返回
  `GuardedHost`（`host()`、`port()`、`addresses()`、`primary()`）。
- `classify_http_status(status)` 与 `result_from_http_status(status, retry_after_seconds)`：与宿主一致的
  `DELIVERED`/`RETRYABLE`/`FAILED` 分类（`2xx` 成功，`408`、`425`、`429` 和 `5xx` 可重试，其余永久失败）；
  `delivered()`、`retryable(code)`、`failed(code)` 用于非 HTTP 渠道。
- `render_event_text(&event)`：返回中文 `(title, body)`。优先原样转发核心生成的 `data.title`/`data.content`
  （插件不得重写通知正文）；缺失时按事件白名单渲染回退文本，且不会带出远程地址、本地路径或完整外部 URL。

插件二进制建议只做如下接线：

```rust
let client = luxd::application::notifier_plugin_sdk::outbound_http_client_from_env()?;
luxd::application::notifier_plugin_sdk::run_notifier(|request| {
    let client = client.clone();
    async move { deliver(&client, request).await }
})
.await?;
```

#### 目的地守卫

通知器必须把宿主在 `notification.send` 里给出的 `target.allowPrivateNetwork` 原样传给守卫，不能自行放宽。守卫
一次性解析目的地的全部地址并逐个校验，分类与核心策略一致：

- **批准私网后可用**：回环、RFC1918 私网、IPv6 唯一本地地址、链路本地，以及核心归类为私网的 CGNAT
  （`100.64.0.0/10`）、基准测试（`198.18.0.0/15`）和 `192.0.0.0/24`。未批准私网时这些地址一律拒绝。
- **即使批准私网也始终拒绝**：未指定地址（`0.0.0.0`、`::`）、链路本地/云元数据（`169.254.169.254` 等）、
  多播、`240.0.0.0/4` 与受限广播 `255.255.255.255`、文档网段（`192.0.2.0/24`、`198.51.100.0/24`、
  `203.0.113.0/24`、`2001:db8::/32`），以及 6to4、Teredo、NAT64 等可封装任意 IPv4 的形式。
- IPv4-mapped（`::ffff:a.b.c.d`）与已废弃的 IPv4-compatible（`::a.b.c.d`、`::/96`）都按内嵌 IPv4 地址分类；
  `::` 与 `::1` 保留其 IPv6 语义（未指定/回环）。

IP 分类复用核心的 `webhooks::is_dangerous_address`、`webhooks::is_private_address` 与
`remote_url_policy::is_private_or_reserved`，插件不应再复制一份网段判断。

守卫只返回稳定错误码（`DestinationGuardError::code()`），插件不得把完整 URL 或主机写进日志、返回值或投递记录：

| 错误 | `code()` | `is_retryable()` |
| --- | --- | --- |
| `InvalidUrl` / `UnsupportedScheme` / `Credentials` | `invalid_destination_url` | 否 |
| `ResolutionFailed` | `destination_unresolved` | 是 |
| `PrivateAddress` / `DangerousAddress` / `ReservedAddress` | `destination_not_allowed` | 否 |

```rust
use luxd::application::notifier_plugin_sdk::{
    DestinationGuardError, failed, guard_destination, outbound_http_client_builder_from_env,
    retryable,
};

let allow_private = request
    .target
    .get("allowPrivateNetwork")
    .and_then(serde_json::Value::as_bool)
    .unwrap_or(false);
let guarded = match guard_destination(&target.url, allow_private).await {
    Ok(guarded) => guarded,
    Err(error) if error.is_retryable() => return retryable(error.code()),
    Err(error) => return failed(error.code()),
};
// 把已校验地址钉进 client，防止 DNS 重绑定；随后客户端不会再解析该主机。
let client = guarded.pin(outbound_http_client_builder_from_env()?).build()?;
let response = client.post(guarded.url().clone()).json(&payload).send().await;
```

`guarded.url()` 保持管理员配置的原值（含路径与 query），只有连接目标被替换为已校验地址；重定向仍然禁用，所以
上游的 3xx 不会绕过守卫。

代理与 `pin()` 的关系：

- 未配置 `LUX_PROXY_URL` 时，SDK 的 `outbound_http_client_builder(_from_env)` 会调用 `no_proxy()`，忽略
  `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` 与系统代理，保证请求直连守卫校验过的地址。
- 管理员显式配置了 `LUX_PROXY_URL` 时，请求按管理员配置走代理（代理仍遵守 `NO_PROXY`）。此时由代理解析目标
  主机，`pin()` 不再决定实际连接地址；守卫仍然按解析结果做拒绝判断。

通知器插件包使用 `scripts/package-notifier-plugin.sh <插件目录名> [target]` 打包：脚本从
`plugins/<目录名>/manifest.json` 读取 ID、版本、入口和声明文件，拒绝非 `notification` 或缺少
`notification.send` 的 manifest，构建入口对应的 cargo bin，并输出
`dist/plugins/<id>-<version>-<platform>-<arch>.zip`。

插件必须返回 `DELIVERED`、`RETRYABLE` 或 `FAILED`，可选返回 provider 请求 ID、重试秒数和稳定错误码：

```json
{"status":"DELIVERED","providerRequestId":"message-1"}
```

宿主负责超时、重试、退避、投递记录和失败恢复。通知插件进程不会获得 `LUX_CONFIG_DIR`，不能通过文件系统
读取其他插件或服务器 Secret。Webhook、Telegram、企业微信等平台的 payload 和认证逻辑属于各自插件，不应
写入通知核心。

Lux 核心在事件进入投递队列前统一生成 `source`、`title`、`content`、`body` 和 ISO `timestamp`，并将它们
放入 `event.data`。播放事件的进度条、播放方式、设备、大小、码率、远程 IP 和简介等展示文本也由 Lux 核心
生成；通知插件不得重新生成或覆盖这些字段。`org.lux.webhook` 的 `url` 配置可以引用 `{title}`、`{content}`
等核心字段并进行 URL 编码，但这只影响目标地址，不改变通知正文。插件可以根据目标平台增加外层字段映射，
但必须保留 Lux 提供的统一通知内容。


### 飞书 / Lark 通知器插件（`org.lux.notifier.feishu`）

`plugins/org.lux.notifier.feishu` 是把 Lux 通知投递到飞书或 Lark 自定义机器人 Webhook 的参考实现。它只
新增 `src/bin/lux-plugin-notifier-feishu.rs`、manifest 和 `tests/plugins/notifier_feishu.rs`，不修改通知
核心、SDK 或其他插件；进程外壳、出站 client、状态分类和正文渲染全部来自
`luxd::application::notifier_plugin_sdk`。

- manifest：`type: "notification"`、`category: "NOTIFICATION"`、`capabilities: ["notification.send"]`，
  入口沿用 `binaries/${platform}-${arch}/lux-plugin-notifier-feishu`。`permissions.network` 声明为 `["*"]`
  （与 Bark 一致），因为 `apiBase` 允许管理员指向任意 http(s) 主机；实际可达地址仍由宿主私网策略和插件目的地
  守卫约束。
- 凭据：唯一声明且仅声明一个 `secret` 字段（`type: "password"`、`sensitive: true`、`required: true`），
  格式为 `hookId` 或 `hookId#signSecret`；`hookId` 只允许 `[A-Za-z0-9-]`。带 `signSecret` 时按飞书规范
  在请求体附加 `timestamp`（Unix 秒）和 `sign = base64(HMAC-SHA256(key = timestamp + "\n" + signSecret,
  data = ""))`。凭据只来自 RPC 的 `secret` 字段，不写文件、不进日志、不进返回值或错误信息。
- 非敏感目标配置（属于每条通知目标，不落 `secret`）：`region`（`feishu`/`lark`，默认 `feishu`，决定
  `https://open.feishu.cn` 或 `https://open.larksuite.com`）、`apiBase`（可选覆盖，仅测试或企业代理使用，
  只接受不含凭据、查询参数和片段的 http(s) 基址）、`msgType`（`post`/`text`，默认 `post`）。通知正文由
  `render_event_text` 原样转发核心生成的 `title`/`content`，插件不重写。
- 请求：`POST {base}/open-apis/bot/v2/hook/{hookId}`，`post` 使用飞书富文本消息（标题放 `title` 字段、
  `content` 只放正文，避免标题重复），`text` 使用纯文本消息（标题作为首行）。出站 client 来自
  `outbound_http_client_builder_from_env`：10 秒超时、不跟随重定向、遵循 `LUX_PROXY_URL`；client 因代理
  配置无效无法构建时返回可重试的 `client_unavailable`，不会静默丢弃投递。
- 目的地守卫：在构建 client 和发起任何连接之前，插件把宿主给出的 `target.allowPrivateNetwork` 原样传给
  `guard_destination`（字段缺失或 `false` 时回环/私网目的地被拒），`169.254.169.254` 等危险保留地址即使
  批准私网也始终被拒；通过后把守卫解析出的地址 `pin` 进出站 client，防止 DNS 重绑定（配置了 `LUX_PROXY_URL`
  时由代理解析，`pin` 不再决定实际连接地址，与 SDK 守卫小节一致）。被拒目的地返回失败状态与
  `destination_not_allowed`，DNS 暂时失败返回可重试的 `destination_unresolved`，两者都不会发出上游请求。
- 结果分类：HTTP `2xx` 且响应 `code == 0` 为 `DELIVERED`；HTTP `429`/`5xx`（以及 `408`、`425`）与飞书
  唯一具有频控语义的业务码 `11232` 为 `RETRYABLE`（HTTP `Retry-After` 会转成 `retry_after_seconds`）；其余
  HTTP `4xx` 和其他非 0 业务码（包括 `19xxx` 中除 `11232` 外的码）为 `FAILED`，错误码为
  `provider_code_<业务码>`。网络错误与超时为 `RETRYABLE`，响应体超过 64 KiB 或无法解析为飞书响应时为
  `FAILED`。
- 稳定错误码：`missing_secret`、`invalid_secret`、`invalid_config`、`provider_timeout`、
  `provider_unreachable`、`provider_response_too_large`、`provider_invalid_response`、
  `provider_rate_limited`、`provider_code_<业务码>`、`client_unavailable`、`destination_not_allowed`、
  `destination_unresolved`、`invalid_destination_url`；HTTP 状态沿用 SDK 的 `PROVIDER_HTTP_<状态码>`。其中
  `provider_timeout`、`provider_unreachable`、`provider_rate_limited`、`client_unavailable` 和
  `destination_unresolved` 归类为 `RETRYABLE`，其余为 `FAILED`。这些代码和 stderr 诊断都不包含凭据、hook
  路径或完整外部 URL 与 query。
- 测试：`cargo test --locked --test plugins notifier_feishu::` 用 127.0.0.1 随机端口的本地 mock 覆盖成功、
  签名、4xx、429/5xx、频控业务码 `11232` 可重试、其他 `19xxx` 业务码永久失败、超时、过大响应、坏配置、
  缺失/畸形凭据、默认未批准私网时拒绝回环目的地，
  以及批准私网仍拒绝云元数据地址 `169.254.169.254`；mock 用例都显式携带 `allowPrivateNetwork: true`，并断言
  凭据不出现在返回值和捕获的 stderr 日志中。不访问真实飞书服务，仓库里只有合成测试值。
- 打包：`scripts/package-notifier-plugin.sh org.lux.notifier.feishu` 生成
  `dist/plugins/org.lux.notifier.feishu-0.1.0-<platform>-<arch>.zip`（`dist/` 已被忽略，不提交产物）。

### Bark 通知器插件（`org.lux.notifier.bark`）

`org.lux.notifier.bark` 是参考实现，演示通知器插件如何只做渠道接线：使用
`run_notifier`、`guard_destination`、`outbound_http_client_builder_from_env`、
`result_from_http_status`、`retryable`、`failed` 与 `render_event_text`，不读取 `LUX_CONFIG_DIR`、
不访问 Lux 数据库或媒体目录。

manifest 声明 `type: "notification"`、`category: "NOTIFICATION"`、`capabilities: ["notification.send"]`，
配置字段分为两类：

- `secret`（`type: "password"`、`sensitive: true`、必填、无默认值）：Bark 设备密钥，只允许
  `[A-Za-z0-9_-]`、长度不超过 128（与 Bark 密钥格式一致，manifest 描述里同步写明）；仅通过
  `notification.send` 请求的 `secret` 字段到达插件，不写入日志、不出现在返回值或错误信息中，也不放进任何 URL。
- 非敏感目标配置：`serverUrl`（默认 `https://api.day.app`，只允许 http/https，去尾斜杠，拒绝凭据、
  query 和 fragment）、`group`、`level`（`active`/`timeSensitive`/`passive`，默认 `active`）、
  `icon`（可选 http(s) URL）、`sound`。这些字段走按目标保存的 `providerConfig`。

投递行为：

- 正文来自核心生成的 `data.title`/`data.content`（`render_event_text` 原样转发，插件不重写文案）。
- 请求为 `POST {serverUrl}/push`，JSON 体是
  `{device_key,title,body,group,level,icon,sound}`，未设置的可选字段省略；设备密钥只在请求体里。
- 目的地守卫：插件把宿主给出的 `target.allowPrivateNetwork` 原样传给 `guard_destination`，默认（字段缺失或
  `false`）拒绝回环/私网目的地，`169.254.169.254` 等危险保留地址即使批准私网也始终拒绝；通过后把已解析地址
  钉进出站 client（仅在未配置 `LUX_PROXY_URL` 时生效，见上文“代理与 `pin()` 的关系”），`serverUrl` 的路径与
  query 保持原样，重定向仍禁用。
- 出站 client 使用宿主代理策略、10 秒超时且不跟随重定向；`2xx` 响应体最多读取 64 KiB。若进程启动时无法按
  宿主代理策略构建 client，插件直接以失败退出（不返回伪造的投递结果），因此没有额外的
  `*_client_unavailable` 错误码。
- 分类：Bark 返回 `code == 200` → `DELIVERED`（`timestamp` 作为 `providerRequestId`）；
  `408`/`425`/`429`/`5xx`、连接失败、请求超时，以及读响应体阶段的传输错误/超时 → `RETRYABLE`
  （`Retry-After` 整数形式写入 `retryAfterSeconds`，其余情况为空）；其它 `4xx`、非 200 的 Bark `code`、
  无法解析的响应和超过 64 KiB 的响应体 → `FAILED`；`Retry-After` 的 HTTP-date 形式被忽略。
- 稳定错误码：`missing_secret`、`invalid_secret`、`invalid_server_url`、`invalid_config`、
  `destination_not_allowed`（私网未批准或危险保留地址）、`destination_unresolved`（DNS 暂时失败，可重试）、
  `invalid_destination_url`、`provider_unavailable`、`provider_response_too_large`、`invalid_response`、
  `bark_code_<code>`，HTTP 分类失败沿用 SDK 的 `PROVIDER_HTTP_<status>`。配置校验失败或被守卫拒绝的请求不会
  发出任何上游请求，日志只记录稳定错误码，不含凭据、主机或完整 URL。

打包：`scripts/package-notifier-plugin.sh org.lux.notifier.bark [target]` 读取该目录的 manifest 并输出
`dist/plugins/org.lux.notifier.bark-0.1.0-<platform>-<arch>.zip`。测试位于
`tests/plugins/notifier_bark.rs`（`cargo test --locked --test plugins notifier_bark::`），全部使用
`127.0.0.1` 随机端口的本地 mock Bark 服务（测试显式携带 `allowPrivateNetwork: true`），覆盖成功、4xx、
429/5xx、超时、网络失败、读响应体中断开/超时、重定向不跟随、超限响应、坏配置、缺失/非法凭据、默认未批准私网
时拒绝回环目的地，以及批准私网仍拒绝云元数据地址；并断言凭据与完整外部 URL 不出现在返回值和捕获的日志里。

## 企业微信应用通知插件

`org.lux.notifier.wecom-app` 通过企业微信自建应用发送文本通知，二进制为
`lux-plugin-notifier-wecom-app`，源码在 `src/bin/lux-plugin-notifier-wecom-app.rs`。插件只做渠道认证和
payload 组装，进程外壳、出站 client、状态分类和通知正文来自
[通知器 SDK](#通知器-sdk)。

`configFields` 都是**每条通知目标**的非敏感设置，凭据只通过唯一的 `secret` 字段下发：

```json
{
  "id": "org.lux.notifier.wecom-app",
  "type": "notification",
  "category": "NOTIFICATION",
  "capabilities": ["notification.send"],
  "configFields": [
    {"key": "corpId", "label": "企业 ID", "type": "text", "required": true},
    {"key": "agentId", "label": "应用 AgentId", "type": "number", "required": true, "minimum": 1, "maximum": 2147483647},
    {"key": "toUser", "label": "接收成员", "type": "text", "required": false, "defaultValue": "@all"},
    {"key": "apiBase", "label": "API 基地址", "type": "text", "required": false, "defaultValue": "https://qyapi.weixin.qq.com"},
    {"key": "secret", "label": "应用 Secret", "type": "password", "required": true, "sensitive": true}

## SMTP 邮件通知器（`org.lux.notifier.email`）

`org.lux.notifier.email` 是一个参考实现的通知器插件，二进制为 `lux-plugin-notifier-email`，只负责
`notification.send`：把宿主生成的通知标题作为邮件主题、正文作为纯文本邮件正文，通过管理员配置的 SMTP
服务器投递。进程外壳、10 秒投递窗口、目的地守卫和状态分类来自
`luxd::application::notifier_plugin_sdk`，正文来自 `render_event_text`，插件不重写通知文本。SMTP 没有 HTTP
跳，因此本渠道不使用 `outbound_http_client`；单次投递由 `NOTIFIER_HTTP_TIMEOUT`（10 秒）的
`tokio::time::timeout` 硬性截断，SMTP 应答由 `lettre` 解析，插件不读取、不记录也不转发上游响应体。

manifest 核心字段：

```json
{
  "id": "org.lux.notifier.email",
  "type": "notification",
  "category": "NOTIFICATION",
  "capabilities": ["notification.send"],
  "runtime": {"kind": "process", "entrypoint": "binaries/${platform}-${arch}/lux-plugin-notifier-email"},
  "configFields": [
    {"key": "secret", "label": "SMTP 密码", "type": "password", "required": true, "sensitive": true},
    {"key": "smtpHost", "label": "SMTP 服务器", "type": "text", "required": true},
    {"key": "smtpPort", "label": "SMTP 端口", "type": "number", "required": false, "minimum": 1, "maximum": 65535},
    {"key": "security", "label": "加密方式", "type": "select", "required": true, "defaultValue": "starttls", "options": [{"value": "starttls", "label": "STARTTLS"}, {"value": "ssl", "label": "SSL/TLS"}]},
    {"key": "username", "label": "SMTP 用户名", "type": "text", "required": false},
    {"key": "from", "label": "发件人地址", "type": "text", "required": true},
    {"key": "to", "label": "收件人地址", "type": "text", "required": true}
  ]
}
```

`secret` 是自建应用的 corpsecret，只从 `notification.send` 请求的 `NotificationSendRpcRequest.secret`
取得，不读取配置文件、不写入 `config`/`providerConfig`，也不出现在日志、RPC 结果或错误信息里。`apiBase`
仅用于代理或测试，必须是 http/https 基地址，且不得包含凭据、query 或 fragment；基地址里已有的路径段
（包括百分号转义）按原样保留，插件只在其后追加 `cgi-bin/*` 路径段。

配置边界与宿主一致：`corpId`、`toUser` 和 `secret` 都按宿主的 4096 字符目标值上限校验（`corpId` 另要求
字母、数字、`-`、`_`；`toUser` 另要求字母、数字与 `@._-|`），因此插件不会拒绝宿主已经接受的配置值。
`toUser` 只在**键缺失**时使用 manifest 默认值 `@all`；显式清空（`""`、纯空白或 `null`）属于配置错误，返回
`Failed(invalid_config)`，避免管理员误清空后被静默改成全员通知。

目的地守卫：插件在发起任何出站连接之前，把 `config.apiBase` 和宿主给出的 `target.allowPrivateNetwork`
（缺失或 `false` 表示不批准）原样交给 SDK 的 `guard_destination`，自己不放宽策略。未批准的私网/回环地址和
始终禁止的保留地址（含 `169.254.169.254` 等云元数据）返回 `Failed(destination_not_allowed)`，DNS 暂时失败
返回 `Retryable(destination_unresolved)`；被拒的请求不会建立任何连接，也不会写入主机或完整 URL。通过后插件
用 `GuardedDestination::pin` 把已校验地址钉进按宿主代理策略构建的 client：未配置 `LUX_PROXY_URL` 时 SDK
禁用一切环境/系统代理，请求直连该地址；管理员显式配置代理时由代理解析目标，`pin()` 不再决定连接地址。
重定向仍然禁用，上游 `3xx` 无法绕过守卫。

投递流程：

1. `GET {apiBase}/cgi-bin/gettoken?corpid=&corpsecret=` 换取 `access_token`。token 缓存在插件进程内，key 为
   `corpId + agentId + apiBase(scheme/host/port/path) + secret` 的哈希（同一凭据指向不同地址或代理前缀时不会
   复用 token），到期前 5 分钟刷新；缓存用 `Mutex` 保护且不跨 `.await` 持锁。
2. `POST {apiBase}/cgi-bin/message/send?access_token=` 发送 `msgtype: "text"` 消息，正文为 SDK
   `render_event_text` 返回的标题与正文（`标题\n\n正文`），超过企业微信 2048 字节限制时按 UTF-8 字符边界截断。
3. 上游 `errcode` 为 `40001`/`40014`/`42001` 时清除缓存 token、重新获取后重试一次；再次失败返回
   `Failed(token_rejected)`。`-1`（系统繁忙）与 `45009`（频率限制）返回 `Retryable`，有 `Retry-After` 时填充
   `retry_after_seconds`；其他非 0 返回 `Failed(upstream_errcode_<n>)`。

重试预算：整次投递（含 token 刷新重试路径）有一个 25 秒的总截止时间，小于宿主的 30 秒通知 RPC 超时；单次
上游请求另有 6 秒截止时间，因此最坏情况（两次尝试各一次 gettoken 加一次 message/send）为 24 秒，仍在总预算
内。总截止时间耗尽时按“消息是否可能已经发出”分类：已经发出过 `message/send` 时返回
`Failed(timeout_after_send)`，避免宿主重试造成重复通知；尚未发出任何消息时返回
`Retryable(delivery_timeout)`，可以安全重试。单次 `message/send` 请求自身超时仍按 `Retryable(network_error)`
返回，宿主按至少一次语义投递并负责去重。

收件人判定：企业微信返回 `errcode: 0` 后，插件把 `invaliduser`、`invalidparty`、`invalidtag` 和
`unlicenseduser` 与配置的 `toUser` 逐项比对。全部收件人都无效时返回 `Failed(no_valid_recipient)`；部分无效
仍然 `DELIVERED`，只在 stderr 记录无效数量，不记录 userid。`toUser` 为 `@all` 时无法逐项枚举，按企业微信的
`errcode: 0` 视为已投递。

稳定结果：

- `DELIVERED`：企业微信返回 `errcode: 0`，`msgid` 作为 `providerRequestId` 返回。
- `RETRYABLE`：网络错误/超时（`network_error`）、`408`/`425`/`429`/`5xx`（`PROVIDER_HTTP_<status>`）、
  `rate_limited`、`system_busy`、目的地 DNS 暂时失败（`destination_unresolved`），或总截止时间内尚未发出任何
  消息（`delivery_timeout`）。
- `FAILED`：凭据缺失或空白（`missing_secret`）、凭据存在但无法安全转发（`invalid_secret`，超过 4096 字符或
  含控制字符）、目标配置非法（`invalid_config`，如 corpId 为空、agentId 非正数、toUser 显式清空或含非法
  字符、apiBase 非 http/https 或带凭据/query）、目的地被守卫拒绝（`destination_not_allowed`）、全部收件人
  无效（`no_valid_recipient`）、已发出消息后超出总截止时间（`timeout_after_send`）、响应超 64 KiB
  （`response_too_large`）、响应不是预期 JSON（`invalid_response`）、其他永久 4xx 与业务错误。

插件日志只写稳定结果码和无效收件人数量，不写 access_token、token 请求的 query、完整外部 URL、userid 或凭据。

打包：`scripts/package-notifier-plugin.sh org.lux.notifier.wecom-app`，产物为
`dist/plugins/org.lux.notifier.wecom-app-0.1.0-<platform>-<arch>.zip`。相关测试在
`tests/plugins/notifier_wecom_app.rs`，使用 127.0.0.1 随机端口的 mock 企业微信服务（显式携带
`allowPrivateNetwork: true`），不访问真实租户；其中一条断言默认未批准私网时回环目的地被拒且 mock 收不到任何
请求，另一条断言批准私网后 `169.254.169.254` 仍被拒。

配置语义：`secret` 是每条通知目标的 SMTP 密码，只从受控的 `secret` 字段到达插件，不进 `config`、不进
日志、不进返回值或错误消息。SMTP 认证只在 `username` 非空时发生；`username` 留空时宿主仍要求 `secret`
非空，此时它是管理员填写的占位值（例如 `unused`），插件不会使用它。其余字段是普通目标配置：

- `smtpHost` 必填，只接受 ASCII 字母、数字、`.`、`-`、`_`，或裸 IPv6 字面量；`@`、`\`、`[`、`]`、
  非 IPv6 的 `:`、路径、空白和控制字符一律判为 `invalid_config`。TLS 模式下还要求它是合法的 DNS 名或 IP
  字面量（标签 1–63 字节、不以 `-` 开头或结尾、末级标签不是纯数字），否则在解析阶段就返回 `invalid_config`。
- `smtpPort` 可选，范围 1–65535；manifest 不声明 `defaultValue`（界面不会预填），留空时由 `security` 决定：
  `starttls` 用 587，`ssl` 用 465。
- `security` 只接受 `starttls` 和 `ssl`；生产构建默认不提供明文 SMTP 目标，`starttls` 要求服务器支持
  STARTTLS，否则投递失败而不是降级为明文。明文通道只作为集成测试的进程内测试缝存在：需要显式设置测试用环境
  变量，**并且**守卫解析出的实际连接地址是回环地址，两者缺一不可；它不是产品功能，也不在 manifest 中声明。
- `username` 可选，首尾空白或控制字符会被判为非法。
- `from` 必填，必须是纯地址（不含显示名）；`to` 是逗号分隔的 1–20 个纯地址，两级都会重新校验地址格式。

目的地守卫：插件在发起任何出站连接前调用 `guard_host(smtpHost, port, allow_private_network)`，把宿主给出的
`target.allowPrivateNetwork` 原样传入（字段缺失或非布尔值视为 `false`，插件不得放宽）。守卫一次性解析
`smtpHost:port` 的全部地址并逐个校验；通过后 SMTP 传输只连接守卫返回的 `primary()` 地址，连接的端口、TLS
握手与证书校验仍使用管理员配置的主机名和端口，因此第二次 DNS 应答无法把投递改道（DNS 重绑定）。SMTP 渠道不走
HTTP 代理，`LUX_PROXY_URL` 对它不生效；守卫校验过的地址始终是实际连接目标。被守卫拒绝时插件不打开任何连接，也
不会把主机或地址写进日志。

结果映射使用稳定的 snake_case 错误码：

| 情况 | 状态 | `error_code` |
| --- | --- | --- |
| 邮件被 SMTP 服务器接受 | `DELIVERED` | 无 |
| `secret` 缺失或只含空白 | `FAILED` | `missing_secret` |
| 目标配置非法（主机、端口、加密方式、TLS 主机名、地址、收件人数量） | `FAILED` | `invalid_config` |
| 守卫拒绝：主机未批准私网、危险保留地址或不可路由的保留地址 | `FAILED` | `destination_not_allowed` |
| 守卫无法解析主机（临时 DNS 失败） | `RETRYABLE` | `destination_unresolved` |
| 守卫判定目的地不是合法的 URL/host:port（空主机、非法端口等） | `FAILED` | `invalid_destination_url` |
| SMTP `4xx` | `RETRYABLE` | `smtp_transient` |
| 连接被拒、连接被断开或对话被提前终止 | `RETRYABLE` | `smtp_unavailable` |
| 超过投递窗口（连接、握手或应答） | `RETRYABLE` | `smtp_timeout` |
| SMTP `5xx`（认证、发件人或收件人被拒） | `FAILED` | `smtp_rejected` |
| STARTTLS 不被服务器支持或协议不兼容 | `FAILED` | `smtp_protocol_error` |
| TLS 握手或证书校验失败 | `FAILED` | `smtp_tls_failed` |

`lettre` 的 rustls 后端把握手和证书失败包装成连接错误，把对话被提前关闭包装成响应错误。插件沿错误源链检查
稳定的错误文本片段，把前一类判为永久失败、后一类判为可重试网络错误；除上述测试缝外，插件不会在加密未建立时把
密码或收件人发给对端。插件每次投递向 stderr 写一行
`org.lux.notifier.email: event <eventId> transport=<starttls|ssl> port=<端口> finished with <状态> (<错误码>)`，
不包含密码、收件人地址、SMTP 主机或任何完整外部 URL。`LUX_PLUGIN_SMTP_TIMEOUT_SECONDS` 仅供测试把投递窗口
收紧到 1–10 秒，不能放宽 SDK 的上限。

测试（`cargo test --locked --test plugins notifier_email::`）用 `127.0.0.1` 随机端口的假 SMTP 服务器覆盖成功、
无认证、`4xx`、`5xx`、认证被拒、连接被断开、超时、默认端口、STARTTLS 不被支持、SSL 握手失败、坏配置（含带
`@`/`\`/方括号/内嵌端口的主机与不可作为 TLS 主机名的名字）和缺凭据；守卫用例覆盖默认未批准私网时拒绝回环中继、
批准私网后仍拒绝云元数据地址 `169.254.169.254`，以及"仅设置测试环境变量但实际地址不是回环"时明文测试缝仍不可用。
所有 mock 用例显式携带 `allowPrivateNetwork: true`，并断言凭据和目的主机不出现在返回值与捕获的日志中；测试不
使用任何真实凭据或真实邮件服务。

打包：`scripts/package-notifier-plugin.sh org.lux.notifier.email`。
