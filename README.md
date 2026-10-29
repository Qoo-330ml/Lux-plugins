# Lux Plugins

[Lux](https://github.com/Qoo-330ml/Lux) 的官方默认插件商店。Lux 默认从本仓库的 `main/index.json`
读取插件目录；管理员也可以在 Lux 中配置其他 HTTPS 目录，多个目录按插件 ID 合并并取最高 SemVer
版本。

仓库同时包含插件源代码与发布工具链：Rust 代码编译成独立插件进程，manifest 描述每个插件的类型、
能力、可配置项与权限，打包脚本把二进制与 manifest 压成 ZIP，发布工作流再把它们发布到 GitHub
Release 并刷新 `index.json`。

## 插件能做什么

插件是独立进程，通过 RPC 与 Lux 通信。它们只处理 Lux 主动传入的窄接口数据，不直接访问 Lux
数据库、媒体根目录、任务对象或通知队列；任务调度、结果持久化、ACL 与旁车写入仍由宿主负责。按
能力划分，本项目提供以下几类插件：

| 类别 | 作用 |
| --- | --- |
| 元数据（SCRAPER） | 提供搜索、详情、图片、演职员、外部 ID 与预告片，例如 TMDb、豆瓣。 |
| 媒体处理（MEDIA） | 媒体探测、弹幕匹配、片头片尾检测与在线章节查询。 |
| 网络（NETWORK） | IP 归属地查询。 |
| 通知（NOTIFICATION） | 把 Lux 生成的统一通知内容投递到外部 HTTP(S) 端点。 |
| 迁移（MIGRATION） | 只读地从其他媒体服务器拉取数据，供 Lux 单向迁移。 |
| 界面资源（UTILITY） | 为登录页提供背景图等展示素材。 |

完整的 RPC 合同、manifest 字段、错误码与目录格式见 Lux 主仓库的 `docs/PLUGIN-SDK.md`；本仓库的
`docs/` 记录各插件来源与许可证审查等实现细节。单个插件的具体边界、配置项和限制以该插件的
manifest 与源码为准。

## 安装

在 Lux 管理界面打开插件商店安装即可。Lux 会校验 ZIP 与 manifest，再解压到 `/config/plugins`，
重启服务后生效。插件配置由 manifest 的 `configFields` 声明，只能通过插件自身的配置入口保存，
不走普通服务器设置。

## 仓库结构

```text
manifests/          每个插件的 manifest（类型、能力、configFields、权限）
plugins.json        发布清单：插件 ID、二进制名、版本、manifest 路径
src/application/    共享的 HTTP 客户端、探测、协议与设置逻辑
src/bin/            每个插件进程的入口（一个插件一个二进制）
scripts/            增量版本选择、打包、发布与目录生成脚本
docs/               插件来源、许可证与迁移说明
tests/              Rust 合同测试与 Python 发布管线测试
index.json          生成的插件目录，由 CI 提交，不要手工编辑
```

`manifests/` 中的 `version` 是 `__VERSION__` 占位符，实际版本由 `plugins.json` 决定并在打包时
注入。

## 开发

需要 Rust 1.85 或更高版本（edition 2024）。

```bash
cargo build --locked --bins                          # 构建全部插件进程
cargo test --locked                                  # 运行插件测试
python3 -m unittest discover -s tests -p '*.py'      # 运行发布管线等 Python 测试
```

开发时可以把 manifest 与二进制按 SDK 描述的目录结构解压到 `/config/plugins/<插件 ID>/` 直接
调试。新增或升级插件时，至少需要：

1. 在 `src/bin/` 加入插件入口，复用 `src/application/` 中已有的客户端与协议代码。
2. 在 `manifests/` 加入 manifest，声明 `apiVersion`、`type`、`category`、`capabilities`、
   `permissions` 与 `configFields`。
3. 在 `plugins.json` 登记插件 ID、二进制名与版本。
4. 补充覆盖 RPC 边界的测试，尤其是网络失败、超时、响应过大与清单缺失等路径。

## 发布

发布是**增量**的：推送到 `main` 且改动涉及 `Cargo.toml`、`Cargo.lock`、`src/`、`manifests/`、
`plugins.json`、`scripts/` 或 `.github/workflows/release.yml` 时，`.github/workflows/release.yml`
会比对新旧 `plugins.json`，**只打包和发布新增或版本号变化的插件**。

1. 在 x86_64 与 aarch64 的 GitHub 托管 runner 上分别构建，产物名为
   `<插件 ID>-<版本>-linux-<架构>.zip`，例如
   `org.lux.tmdb-0.1.12-linux-x86_64.zip` 与 `org.lux.tmdb-0.1.12-linux-aarch64.zip`。Lux 按
   `packages` 选择匹配架构的包，并以自身的规范 ZIP 名保存。
2. 每个插件对应一个稳定 Release，tag 即插件 ID。版本资产是**只追加**的：已发布资产不会被
   覆盖，若要更新内容必须提升版本号。移除某个插件 ID 只会把它从目录中删除，不会删除其历史
   Release 与资产。
3. 把变化的条目合并进现有 `index.json` 并提交回 `main`，未变化的条目原样保留。

**只有改源码或 manifest 而不提升 `plugins.json` 里的版本号，不会有任何发布。** 仅修改文档
（如本 README）不会触发构建。本地验证打包效果：

```bash
cargo build --release --locked --target x86_64-unknown-linux-gnu --bins
python3 scripts/build-packages.py --target x86_64-unknown-linux-gnu --arch x86_64 --output dist
python3 scripts/generate-index.py --artifacts dist --existing index.json \
  --repository Qoo-330ml/Lux-plugins --output dist/index.json
```

## 安全要求

- 禁止提交凭据、本地配置、媒体数据或未审查的可执行包。
- 插件不得记录凭据、访问令牌、Cookie、完整查询串或完整外部 URL。
- 对外请求要有超时、响应大小上限、重试策略与 HTTPS 校验；网络权限必须在 manifest 的
  `permissions` 中声明。
- `index.json` 只由发布工作流生成，手工改动会在下次发布时被覆盖。
