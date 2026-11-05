# Trakt 多源评分插件 — 能力边界与宿主对接说明

`org.lux.trakt-ratings` 是**只读查询插件**。它把 Trakt 的多源评分整理成宿主可直接消费的结构，
但不接触媒体库文件系统。

## 职责划分

| 环节 | 由谁完成 | 说明 |
|---|---|---|
| 查询 Trakt 多源评分 | **插件** | `ratings.lookup` / `ratings.batch` / `ratings.search` |
| 决定写什么值 | **插件** | 返回 `writeback` 建议块（含票数校验后的主源） |
| 读取媒体库 NFO | 宿主 | 插件 `filesystem` 权限只允许符号目录，拿不到库路径 |
| 写回 NFO | 宿主 | 同上 |
| 触发 `refresh-local-metadata` | 宿主 | 需要 admin 鉴权，插件没有该身份 |

插件的 `plugin.hello` 会显式声明这一点：

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
  "minVotes": 50,
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
    "rating": 6.56425,
    "votes": 280,
    "criticRating": null      // metascore/10 或 RT/10，都没有则为 null
  }
}
```

`writeback` 是宿主唯一需要消费的字段。它按 `trakt -> imdb -> tmdb` 取第一个票数达到 `minVotes` 的源：

- `confident: false` → **不要覆盖** NFO 里的原评分，只记录 `sources`
- `confident: true` → 把 `rating` / `votes` / `criticRating` 写进 NFO

### `ratings.batch`

```jsonc
{"items": [ {"kind":"movie","imdb":"tt9597190"}, {"kind":"episode","show":"292359","season":1,"episode":1} ]}
```

- 单批上限 **200** 条，超出返回 `TRAKT_BATCH_TOO_LARGE`
- 插件内部串行、间隔 350ms（约 2.8 rps，低于 Trakt 的 1000 次/5 分钟）
- 单进程内按查询参数缓存，重复条目不再请求上游
- 单条失败不影响整批，失败项带 `error.code`

### `ratings.search`

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

## 宿主侧建议实现

1. **遍历**：扫出 `<movie>` / `<tvshow>` / `<episodedetails>`，取 `uniqueid`、`title`、`originaltitle`、`year`、`season`、`episode`
2. **分批查询**：每 200 条调一次 `ratings.batch`
3. **写 NFO**：仅当 `writeback.confident == true` 时覆盖 `<rating>` / `<votes>` / `<criticrating>`；
   无论是否覆盖，都把完整 `sources` 落到自定义命名空间（建议 `xmlns:trakt`），便于回溯与回滚
4. **触发刷新**：`POST /api/v1/admin/libraries/{id}/refresh-local-metadata`
   body `{"rootId": null, "paths": ["相对库根的目录", ...]}`，单次最多 2000 个目录
5. **幂等**：写入后打时间戳标记，同日不重复处理

## 实测数据（本机媒体库，6359 个 NFO）

- 可映射条目：movie 45 + tvshow 126 = 171，命中 **120（70%）**
- 分集 6188 个，走父剧集映射后按季/集取评分，整季 12 集全部成功
- 未命中 51 条：43 条无任何 ID（多为分集被误存为 `<movie>`），8 条 Trakt 未收录的国产综艺/纪录片
- 多源覆盖（命中条目）：tmdb 100%、imdb 68%、烂番茄 22%、MAL 13%、Metascore 2%、Letterboxd 2%
- **58% 的命中条目 Trakt 票数 < 50**，因此票数阈值与主源回退是必需项
