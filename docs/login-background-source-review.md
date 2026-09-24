# 登录背景图片来源审查

## 必应每日图片：`wefashe/bing-image`

审查日期：2026-09-24

- 仓库代码以 MIT 许可证发布；这只许可复用代码，不授予必应图片的版权或应用展示权。
- README 将所列接口限制为个人学习和研究、图片限制为个人壁纸；每日获取依赖必应内部端点，未提供面向第三方应用的图片授权。
- 未能在微软正式开发者文档中确认这些每日壁纸端点是受支持的第三方 API，也未找到允许将图片用于 Lux 登录背景的授权条款。

结论：MIT 许可不等于图片展示授权，因此必应来源不进入正式目录。只有取得适用于第三方应用展示的明确授权，并确认取数接口受支持后，才重新评估。

## TMDb 日榜横幅图

审查日期：2026-09-24

- 使用 TMDb 官方 [`Trending All` API](https://developer.themoviedb.org/reference/trending-all)，固定请求 `/3/trending/all/day`；将响应视为不可信数据，只接受 `movie`/`tv` 的有效 `backdrop_path`，按原榜单顺序选择首张横幅图。人物和没有横幅图的项目会跳过；不会改用 `poster_path`。
- 图片 URL 按 TMDb 官方[图片文档](https://developer.themoviedb.org/docs/image-basics)格式组成：`https://image.tmdb.org/t/p/w1280/{backdrop_path}`。Lux 直接按原比例显示 `SINGLE_IMAGE`，插件只组装 URL，不下载、裁切、旋转或组合图片。
- 复用仓库已有的 `TmdbClient`，其提供 HTTPS JSON 请求、超时、响应大小上限、限速和代理支持。通过 `TmdbClient::new_with_embedded_fallback` 复用该 client 内嵌的 fallback API key；该 key 被编译进独立背景插件，不读取 `org.lux.tmdb` 的配置、不放入 manifest、不由 Lux RPC/API 响应返回，也不写入日志。请求关闭重定向且不在插件内重试；日榜每次只请求一次，失败交由宿主刷新与回退策略处理。
- TMDb 官方 [API Terms](https://www.themoviedb.org/api-terms-of-use)要求对 TMDb 内容归属署名、禁止对 TMDb 内容制作衍生作品，并规定未获书面商业协议不得商业使用；官方 [FAQ](https://developer.themoviedb.org/docs/faq)要求来源说明位于 About/Credits 区域。Lux 宿主已在“关于与鸣谢”中显示获准 TMDb 标识与非背书声明。
- 本插件按项目所有者确认的非商业用途登记进 `plugins.json`；正式 `index.json`、双架构 ZIP 和 SHA-256 由仓库 `main` 分支的 release workflow 自动生成。插件仍要求管理员明确确认已核对许可；该确认不是 TMDb 授权，商业用途必须先取得书面协议。

## Wikimedia Commons Picture of the Day

审查日期：2026-09-24

- 使用 [Wikimedia Commons 官方 Action API](https://commons.wikimedia.org/w/api.php) 读取 `Template:Potd/YYYY-MM-DD` 的 wikitext，再通过 `action=query&prop=imageinfo&iiprop=url|extmetadata|mime&iiurlwidth=1920` 获取缩略图、作品页和结构化许可/作者元数据。
- 插件只接受公共领域、CC0、CC BY 或 CC BY-SA（支持 CC 1.0、2.0、2.5、3.0、4.0）；拒绝 NC、ND、未知许可及缺少作者/作品页信息的资源。作者 HTML 仅抽取为纯文本，不作为 HTML 渲染。
- 图片 URL 限定在 `thumb.wikimedia.org`；作品页和许可证 URL 必须由 API 返回并命中 manifest 主机白名单。Lux 直接显示 Commons 提供的 1920px 缩略图，不下载、缓存或重编码图片，并在背景区域呈现作者、作品页和许可证。

此记录是工程来源审查，不构成法律意见。
