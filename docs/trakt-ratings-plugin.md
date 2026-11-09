# Trakt 多源评分插件 — Kodi NFO 写回与宿主对接说明

`org.lux.trakt-ratings` 是**只读查询插件**。它把 Trakt 的多源评分整理成 Kodi 规范的
`<ratings>` 片段和顶层评分字段，由宿主写入 NFO；插件本身不接触媒体库文件系统。

## 职责划分

| 环节 | 由谁完成 | 说明 |
|---|---|---|
| 查询 Trakt 多源评分 | **插件** | `ratings.lookup` / `ratings.batch` / `ratings.search` |
| 决定写什么值、生成 Kodi `<ratings>` | **插件** | `writeback` / `writeback.nfo` |
| 按选中媒体库遍历 NFO | 宿主 | `RATINGS_SYNC` 计划任务 |
| 读取/写回 NFO、触发刷新 | 宿主 | 插件 `filesystem` 权限为空，也没有 admin 身份 |
| 注册计划任务 | 宿主 | 宿主读取清单的 `scheduledTasks` 与 `configFields` |

插件的 `plugin.hello` 仍显式声明不碰文件：

```json
"doesNot": ["read_media_files", "write_nfo", "trigger_library_refresh"]
```

## RPC 方法

### `ratings.lookup`

```jsonc
// 请求（四选一即可，按优先级：id/imdb/slug > title+tmdb > title+year）
{"kind": "movie", "imdb": "tt9597190"}
{"kind": "movie", "title": "飞驰人生", "tmdb": "575219", "year": 2019}
{"kind": "show",  "title": "双世宠妃", "tmdb": "72922"}
{"kind": "episode", "show": "292359", "season": 1, "episode": 1}   // show 可以是数字 id 或 imdb
```

`kind` 支持 `movie` / `show`（同义 `series`）/ `episode`。

```jsonc
// 响应（节选）
{
  "traktId": 425981,
  "matchedBy": "id",          // id | tmdb | imdb | title+year | episode
  "lowConfidence": false,
  "minVotes": 10,
  "primary": { "rating": 6.56425, "votes": 280, ... },
  "stats": { "median": 7, "stddev": 1.57, "entropy": 0.762, "lowRate": 3.9, "highRate": 27.5, "votes": 280 },
  "distribution": { "1": 1, "2": 6, ..., "10": 11 },
  "sources": {
    "trakt": {...}, "imdb": {...}, "tmdb": {...}, "metascore": {...},
    "rottenTomatoes": {...}, "letterboxd": {...}, "mal": {...}
  },
  "writeback": {
    "confident": true,
    "source": "trakt",        // 实际采用的源
    "rating": 6.56425,        // 原有字段：未归一的原始主源分数
    "votes": 280,
    "criticRating": 7.2,      // 原有字段：metascore/10 或 RT/10
    "nfo": {
      "ratings": [
        {"name": "tomatometerallcritics", "value": 8.7, "votes": null, "max": 10, "default": false},
        {"name": "metacritic",             "value": 7.2, "votes": null, "max": 10, "default": false},
        {"name": "imdb",                   "value": 7.8, "votes": 199521, "max": 10, "default": false},
        {"name": "trakt",                  "value": 6.564, "votes": 280, "max": 10, "default": true}
      ],
      "xml": "<ratings><rating name=\"tomatometerallcritics\" max=\"10\"><value>8.7</value></rating>...</ratings>",
      "rating": 6.564,
      "votes": 280,
      "criticRating": 8.7
    }
  }
}
```

`writeback` 原有字段全部保留，新增的 `nfo` 是宿主写 Kodi NFO 时唯一需要的新字段：

- `confident: false` → `nfo: null`，**不要覆盖** NFO 里的原评分，只记录 `sources`
- `confident: true` → 按 `nfo` 写入；`writeback.rating/votes/criticRating` 仍保留旧语义供旧调用方使用

`nfo.ratings` 已按 Jellyfin 兼容顺序排好；`nfo.xml` 只包含 `<ratings>…</ratings>` 一个片段，
XML 特殊字符已转义，数值保留 1~3 位小数（不会出现 `6.56425000001` 这类浮点尾巴）。
`nfo.rating` / `nfo.votes` / `nfo.criticRating` 是给宿主写**顶层**
`<rating>` / `<votes>` / `<criticrating>` 的独立字段，插件不碰文件。

> `writeback.criticRating` 为兼容旧调用方仍优先取 Metascore；`nfo.criticRating` 则按
> Jellyfin 从 `<ratings>` 实际读到的结果取番茄值（没有番茄时回退到 Metascore/10）。
> 两者都保留，宿主写 NFO 时应使用 `nfo.criticRating`。

### Kodi `<ratings>` 示例

```xml
<ratings>
  <rating name="tomatometerallcritics" max="10">
    <value>8.7</value>
  </rating>
  <rating name="metacritic" max="10">
    <value>7.2</value>
  </rating>
  <rating name="imdb" max="10">
    <value>7.8</value>
    <votes>199521</votes>
  </rating>
  <rating name="trakt" max="10" default="true">
    <value>6.564</value>
    <votes>280</votes>
  </rating>
</ratings>
```

源名称使用 Kodi 惯例：`trakt`、`imdb`、`themoviedb`（TMDb）、`metacritic`（Metascore）、
`tomatometerallcritics`（烂番茄）、`letterboxd`、`myanimelist`（MAL）。

### 刻度归一

所有 `<value>` 统一换算到 0–10，并写 `max="10"`。Jellyfin 忽略 `max`，所以归一必须在插件内完成：

| 源 | Kodi 名称 | Trakt 原始范围 | 换算 |
|---|---|---|---|
| Trakt | `trakt` | 0–10 | ×1 |
| IMDb | `imdb` | 0–10 | ×1 |
| TMDb | `themoviedb` | 0–10 | ×1 |
| Metascore | `metacritic` | 0–100 | ÷10 |
| 烂番茄 | `tomatometerallcritics` | 0–100 | ÷10 |
| Letterboxd | `letterboxd` | 0–5 | ×2 |
| MyAnimeList | `myanimelist` | 0–10 | ×1 |

（Trakt 官方响应 schema 对 Letterboxd 标注 0–5、MAL 标注 0–10；Metascore 与烂番茄为整数百分制。）

没有评分（`present == false`）的源不输出。`votes` 只在源确实提供票数时输出，Metascore 和
烂番茄在 Trakt 响应里没有票数，因此为 `null` 且 XML 中省略 `<votes>`。

### Jellyfin 读取规则（已核对 `BaseNfoParser.cs`）

1. Jellyfin 能读 `<ratings><rating name="…"><value>…</value></rating></ratings>`。
   **所有名字不含 `tomato` 的条目都写进同一个 `CommunityRating`，最后一个读到的生效**；
   `default="true"` 与 `max` 属性被忽略；`<votes>` 被完全忽略。
2. 名字包含 `tomato`（不区分大小写）、且不含 `audience`、`avg` 的条目 → 写入 `CriticRating`。
   含 `audience` 的番茄条目会落到 `CommunityRating`；含 `avg` 的番茄条目既不写 critic 也不写
   community（被丢弃）。因此插件只生成 `tomatometerallcritics`，不生成含 `audience` / `avg` 的名字。
3. 顶层 `<rating>`、`<criticrating>` 也会被读取。

上述行为按 Jellyfin 的 `MediaBrowser.XbmcMetadata/Parsers/BaseNfoParser.cs` 核对；
**Emby 的同类解析行为未核实**，不要把这里的结论直接套到 Emby。

### 为什么主源放最后

Jellyfin 对非番茄条目只保留最后一个 `CommunityRating`。插件把主源（`default: true`）排在
`ratings` 最后，于是 Jellyfin 的 `CommunityRating` 恰好等于主源，不需要依赖 `default` 属性。
番茄条目写的是 `CriticRating`，不参与这个覆盖，所以可以安全地放在最前面。

## 计划任务与媒体库选择

清单新增两个配置字段：

- `libraryIds`：`select` + `multiple` + `optionsSource: "media-libraries"`，只对选中的媒体库查询并
  回写评分；不选择任何媒体库时宿主不会创建同步任务。
- `schedule`：UTC cron，默认 `0 4 * * 0`。

清单同时声明：

```json
{
  "taskType": "RATINGS_SYNC",
  "ownerType": "LIBRARY",
  "ownerConfigKey": "libraryIds",
  "name": "Trakt 评分同步",
  "scheduleConfigKey": "schedule",
  "defaultSchedule": "0 4 * * 0",
  "requiredConfigKeys": ["clientId", "libraryIds"]
}
```

`ownerType: LIBRARY` + `ownerConfigKey` 会按选中的每个媒体库各注册一条任务；这符合宿主
`PluginScheduledTask::validate` 对 `ownerConfigKey` 必须引用 `select` + `multiple` 字段的要求。

### `RATINGS_SYNC` 需要宿主实现

当前宿主 `src/application/scheduled_tasks.rs` 的 `normalize_scheduled_task_request` 允许列表里
还没有 `RATINGS_SYNC`，未知任务类型会返回 `ScheduledTaskError::UnsupportedTask`；`run_task_with_config`
也没有对应分支。宿主实现前，清单可以被解析和注册，但任务运行会失败，这是预期状态。

宿主任务的期望流程：

1. 按 `libraryIds` 找到选中的媒体库，遍历其中的 `<movie>` / `<tvshow>` / `<episodedetails>` NFO
2. 取 `uniqueid`、`title`、`originaltitle`、`year`、`season`、`episode`，每 **200** 条调用一次
   `ratings.batch`
3. 仅当 `writeback.confident == true` 时写回：
   - 用 `nfo.xml` 替换/写入 `<ratings>…</ratings>`
   - 用 `nfo.rating` / `nfo.votes` / `nfo.criticRating` 更新顶层
     `<rating>` / `<votes>` / `<criticrating>`
   - `confident == false` 时不写任何评分字段
4. 对发生变化的目录调用
   `POST /api/v1/admin/libraries/{id}/refresh-local-metadata`
   （body `{"rootId": null, "paths": ["相对库根的目录", ...]}`，单次最多 2000 个目录）
5. 写入后打时间戳标记，同日不重复处理；保留 `sources` 便于回溯与回滚

## `ratings.batch`

```jsonc
{"items": [ {"kind":"movie","imdb":"tt9597190"}, {"kind":"episode","show":"292359","season":1,"episode":1} ]}
```

- 单批上限 **200** 条，超出返回 `TRAKT_BATCH_TOO_LARGE`
- 插件内部串行、间隔 350ms（约 2.8 rps，低于 Trakt 的 1000 次/5 分钟）
- 单进程内按查询参数缓存，重复条目不再请求上游
- 单条失败不影响整批，失败项带 `error.code`

## `ratings.search`

纯搜索，返回候选列表，用于人工确认或构建映射表。

## 错误码

| code | 含义 | 宿主处理建议 |
|---|---|---|
| `TRAKT_NOT_FOUND` | Trakt 无此条目（204 或 404） | 记入未命中清单，不重试 |
| `TRAKT_NOT_CONFIGURED` | 未配置 Client ID | 提示用户配置，暂停任务 |
| `TRAKT_UNAUTHORIZED` | key 或 api-version 被拒 | 暂停任务 |
| `TRAKT_RATE_LIMITED` | 触发限流 | 退避后重试 |
| `TRAKT_UPSTREAM_ERROR` | 网络/上游异常 | 指数退避重试 |
| `TRAKT_INVALID_REQUEST` | 参数不合法 | 修正调用 |
| `TRAKT_BATCH_TOO_LARGE` | 超过 200 条 | 拆分批次 |

## 实测数据（本机媒体库，6359 个 NFO）

- 可映射条目：movie 45 + tvshow 126 = 171，命中 **120（70%）**
- 分集 6188 个，走父剧集映射后按季/集取评分，整季 12 集全部成功
- 未命中 51 条：43 条无任何 ID（多为分集被误存为 `<movie>`），8 条 Trakt 未收录的国产综艺/纪录片
- 多源覆盖（命中条目）：tmdb 100%、imdb 68%、烂番茄 22%、MAL 13%、Metascore 2%、Letterboxd 2%
- **58% 的命中条目 Trakt 票数 < 50**，因此票数阈值与主源回退是必需项
