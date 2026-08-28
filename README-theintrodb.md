# TheIntroDB 章节源

`org.lux.theintrodb-chapter-source` 是 Lux 的独立在线章节插件。它调用
[TheIntroDB](https://theintrodb.org/) 的公开 API，查询已标注的片头和片尾，并把结果写入 Lux
现有的特殊章节存储。

manifest 声明 `supportedMediaSourceKinds: ["LOCAL_FILE", "STRM_URL"]`。这只告诉 Lux 哪些媒体源
可以进入在线查询候选；插件仍然不会收到本地路径或 `.strm` URL。

插件只接收 Lux 已保存的元数据：TMDb/TVDb/IMDb ID、季号、集号和可选时长。它不接收媒体路径、
`.strm` URL、音频指纹或任务对象，也不主动调用 ffmpeg/ffprobe。没有 TheIntroDB 数据的分集会保留
已有章节，不会因一次空响应删除旧结果。

查询优先级为 TMDb、TVDb、IMDb，与 TheIntroDB 的 Emby 插件保持一致。片头需要有效结束时间；片尾
只接受明确的开始时间，避免把只有 `end_ms` 的不完整数据误认为从视频开头跳过。当前只映射片头和
片尾，Recap/Preview 不会写成普通章节。

TheIntroDB API Key 是可选的敏感配置，默认不记录任何请求 URL、ID 或响应内容。插件进程按服务端
30 次/10 秒的限制保守地以约 350ms 间隔请求，并对 429/5xx 做有限重试。
