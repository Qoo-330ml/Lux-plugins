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
