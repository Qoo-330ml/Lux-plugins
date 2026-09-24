# 登录背景图片来源审查

审查日期：2026-09-24

## 必应每日图片：`wefashe/bing-image`

审查范围：项目的 [MIT 许可证](https://github.com/wefashe/bing-image/blob/main/LICENSE)、[README](https://github.com/wefashe/bing-image/blob/main/README.md) 和 `code/crawl/bing.py`。

- 仓库代码以 MIT 许可证发布；这只许可复用该仓库代码，不授予必应图片的版权或应用展示权。
- README 明确称其列出的接口仅供个人学习和研究，图片仅限个人壁纸使用，版权归原作者所有。
- `code/crawl/bing.py` 的每日图片获取依赖 `HPImageArchive.aspx` 和必应首页内部 `_model` 数据；故事获取还会抓取必应搜索 HTML。仓库没有提供面向第三方应用的图片授权。
- 未能在微软正式开发者文档中确认这些每日壁纸端点是受支持的第三方 API，也未找到允许将这些图片用作 Lux 登录背景的授权条款。

结论：可从 MIT 角度复用代码，但不能据此启用或分发必应图片提供者。按照 Lux 的 LUX-262 来源门槛，目前不实现可运行的必应图片插件，也不将其加入插件目录。只有取得适用于第三方应用展示的明确授权，并确认受支持的取数接口后，才重新评估该来源。

此记录是工程来源审查，不构成法律意见。

## TMDb 日榜横幅图

审查日期：2026-09-24

- 使用 TMDb 官方 [`Trending All` API](https://developer.themoviedb.org/reference/trending-all)，固定请求 `/3/trending/all/day`；将响应视为不可信数据，只接受 `movie`/`tv` 的有效 `backdrop_path`，按原榜单顺序选择首张横幅图。人物和没有横幅图的项目会跳过；不会改用 `poster_path`。
- 图片 URL 按 TMDb 官方[图片文档](https://developer.themoviedb.org/docs/image-basics)格式组成：`https://image.tmdb.org/t/p/w1280/{backdrop_path}`。Lux 直接按原比例显示 `SINGLE_IMAGE`，插件只组装 URL，不下载、裁切、旋转或组合图片。
- 复用仓库已有的 `TmdbClient`，其提供 HTTPS JSON 请求、超时、响应大小上限、限速和代理支持。通过 `TmdbClient::new_with_embedded_fallback` 复用该 client 内嵌的 fallback API key；该 key 被编译进独立背景插件，不读取 `org.lux.tmdb` 的配置，不放进 manifest/RPC/API，也不写入日志。请求关闭重定向且不在插件内重试；日榜每次只请求一次，失败交由宿主刷新与回退策略处理。
- TMDb 官方 [API Terms](https://www.themoviedb.org/api-terms-of-use)要求对 TMDb 内容归属署名、禁止对 TMDb 内容制作衍生作品，并规定未获书面商业协议不得商业使用；官方 [FAQ](https://developer.themoviedb.org/docs/faq)要求来源说明位于 About/Credits 区域。Lux 宿主已在“关于与鸣谢”中显示获准 TMDb 标识与非背书声明。
- 本插件按项目所有者确认的非商业用途登记进 `plugins.json`；正式 `index.json`、双架构 ZIP 和 SHA-256 由仓库 `main` 分支的 release workflow 自动生成。插件仍要求管理员明确确认已核对许可；该确认不是 TMDb 授权，商业用途必须先取得书面协议。
- 背景插件请求关闭 HTTP 自动重定向，TMDb 背景刷新只发起一次请求并由 Lux worker 退避重试；Commons 两个 Action API 请求之间限速，遇到重定向、超量响应或非白名单文件/署名主机时拒绝本轮结果。

此记录是工程来源审查，不构成法律意见。

## Wikimedia Commons Picture of the Day

审查日期：2026-09-24

- 使用 [Wikimedia Commons 官方 Action API](https://commons.wikimedia.org/w/api.php)读取 `Template:Potd/YYYY-MM-DD` 的 wikitext，再通过 `action=query&prop=imageinfo&iiprop=url|extmetadata|mime&iiurlwidth=1920` 获取该文件的缩略图、描述页、MIME 与结构化许可/作者元数据。
- 已对 Commons 当日 POTD API 做只读实测：模板返回了单一 `{{Potd filename|1=...}}` 文件名；`imageinfo/extmetadata` 返回图片 URL、描述页、`LicenseShortName`、`LicenseUrl`、`Artist` 和 `ImageDescription`。示例为 `CC BY-SA 4.0`，其图片由 `thumb.wikimedia.org` 按长边 1920px 提供，Lux 不下载、缓存、裁切或重编码图片。
- 插件只允许公共领域/CC0/CC BY/CC BY-SA（支持 CC 1.0、2.0、2.5、3.0、4.0）；逐文件检查准确的许可证短名与 Creative Commons 官方 HTTPS 许可 URL，拒绝 NC、ND、未知许可和缺作者/作品描述页。原始作者 HTML 仅用有界 XML 文本抽取器转成纯文本，不会作为 HTML 渲染。
- 返回的作品页与许可证 URL 受 manifest `network` 主机白名单约束；图片 URL 限定在 `thumb.wikimedia.org`。宿主将署名作为图像外部的独立文字/链接呈现。

此记录是工程来源审查，不构成法律意见。
