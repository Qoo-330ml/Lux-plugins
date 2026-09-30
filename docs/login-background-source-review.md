# 登录背景图片来源审查

## 必应每日图片：`wefashe/bing-image`

审查日期：2026-09-28（按项目所有者重新确认的产品需求更新）

- 仓库代码以 MIT 许可证发布；这只许可复用代码，不授予必应图片的版权或应用展示权。
- README 将所列接口限制为个人学习和研究、图片限制为个人壁纸；每日获取依赖必应站点端点，未提供面向第三方应用的图片授权。
- 未能在微软正式开发者文档中确认 `HPImageArchive.aspx` 是受支持的第三方 API，也未找到普遍允许将每日图片嵌入第三方网站/应用的授权条款。微软支持文档说明可下载作壁纸的图片也可能受单图许可限制。
- 项目所有者重新确认产品目标是 Lux 登录页显示 Bing 每日大图；因此统一插件的 `BING_DAILY` 来源作为显式 opt-in，仅传递 Bing 直链，不将图片字节下载、缓存、重编码或分发。上游用途限制仍适用，插件要求部署管理员核对具体场景与单图许可，并确认仅作个人用途；该独立确认开关不构成授权。

结论：MIT 许可不等于图片展示授权，且该接口不是微软承诺的公共 API。本插件只适用于管理员已确认符合个人用途与图片许可的部署；不得据此宣称微软授权/背书，不应用于商业或未经许可的公开再分发。接口变化时应失败回退，不改用未审查的第三方服务。

## TMDb 日榜横幅图

审查日期：2026-09-24

- 使用 TMDb 官方 [`Trending All` API](https://developer.themoviedb.org/reference/trending-all)，固定请求 `/3/trending/all/day`；将响应视为不可信数据，只接受 `movie`/`tv` 的有效 `backdrop_path`，按原榜单顺序选择首张横幅图。人物和没有横幅图的项目会跳过；不会改用 `poster_path`。
- 图片 URL 按 TMDb 官方[图片文档](https://developer.themoviedb.org/docs/image-basics)格式组成：`https://image.tmdb.org/t/p/w1280/{backdrop_path}`。作为登录页横幅，插件返回 `HERO_IMAGE`，由 Lux 固定布局用 `object-fit: cover` 铺满左侧视觉区；插件只组装 URL，不下载、代理或改写图片。
- 复用仓库已有的 `TmdbClient`，其提供 HTTPS JSON 请求、超时、响应大小上限、限速和代理支持。通过 `TmdbClient::new_with_embedded_fallback` 复用该 client 内嵌的 fallback API key；该 key 被编译进独立背景插件，不读取 `org.lux.tmdb` 的配置、不放入 manifest、不由 Lux RPC/API 响应返回，也不写入日志。请求关闭重定向且不在插件内重试；日榜每次只请求一次，失败交由宿主刷新与回退策略处理。
- TMDb 官方 [API Terms](https://www.themoviedb.org/api-terms-of-use)要求对 TMDb 内容归属署名、禁止对 TMDb 内容制作衍生作品，并规定未获书面商业协议不得商业使用；官方 [FAQ](https://developer.themoviedb.org/docs/faq)要求来源说明位于 About/Credits 区域。Lux 宿主已在“关于与鸣谢”中显示获准 TMDb 标识与非背书声明。
- TMDb 模式整合在 `org.lux.login-background` 的 `TMDB_TRENDING` 来源中；管理员许可确认仍与 Bing 和自定义图片的确认分开，默认关闭。正式 `index.json`、双架构 ZIP 和 SHA-256 由仓库 `main` 分支的 release workflow 自动生成。该确认不是 TMDb 授权，商业用途必须先取得书面协议。

此记录是工程来源审查，不构成法律意见。
