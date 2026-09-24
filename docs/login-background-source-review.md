# 登录背景图片来源审查

## TMDb 日榜横幅图

审查日期：2026-09-24

- 使用 TMDb 官方 [`Trending All` API](https://developer.themoviedb.org/reference/trending-all)，固定请求 `/3/trending/all/day`；将响应视为不可信数据，只接受 `movie`/`tv` 的有效 `backdrop_path`，按原榜单顺序选择首张横幅图。人物和没有横幅图的项目会跳过；不会改用 `poster_path`。
- 图片 URL 按 TMDb 官方[图片文档](https://developer.themoviedb.org/docs/image-basics)格式组成：`https://image.tmdb.org/t/p/w1280/{backdrop_path}`。Lux 直接按原比例显示 `SINGLE_IMAGE`，插件只组装 URL，不下载、裁切、旋转或组合图片。
- 复用仓库已有的 `TmdbClient`，其提供 HTTPS JSON 请求、超时、响应大小上限、限速和代理支持。通过 `TmdbClient::new_with_embedded_fallback` 复用该 client 内嵌的 fallback API key；该 key 被编译进独立背景插件，不读取 `org.lux.tmdb` 的配置、不放入 manifest、不由 Lux RPC/API 响应返回，也不写入日志。请求关闭重定向且不在插件内重试；日榜每次只请求一次，失败交由宿主刷新与回退策略处理。
- TMDb 官方 [API Terms](https://www.themoviedb.org/api-terms-of-use)要求对 TMDb 内容归属署名、禁止对 TMDb 内容制作衍生作品，并规定未获书面商业协议不得商业使用；官方 [FAQ](https://developer.themoviedb.org/docs/faq)要求来源说明位于 About/Credits 区域。Lux 宿主已在“关于与鸣谢”中显示获准 TMDb 标识与非背书声明。
- 本插件按项目所有者确认的非商业用途登记进 `plugins.json`；正式 `index.json`、双架构 ZIP 和 SHA-256 由仓库 `main` 分支的 release workflow 自动生成。插件仍要求管理员明确确认已核对许可；该确认不是 TMDb 授权，商业用途必须先取得书面协议。

此记录是工程来源审查，不构成法律意见。
