# RFC 0003：Web 管理控制台

- 状态：**草案**
- 日期：2026-11-18
- 依赖：[RFC 0001](0001-architecture.md) §5（控制面 API）、§8（多租户隔离）
- 依据：Selvedge v0.3.3 的 `/client/v4` API（`crates/selvedge-server/src/api/`），`@cloudflare/kumo` 2.19.0

Selvedge 现在只能用 wrangler、`/client/v4` API 和 `selvedge admin` 命令行管理，没有 Web 界面。本文提议做一个类似 Cloudflare 控制台的 Web 控制台，用 Cloudflare 开源的组件库 Kumo 搭界面。结论是：**后端几乎都已具备，主要工作在前端**；它是一个静态单页应用，由 API 服务托管，不新增组件，只补少量 API。

## 1. 目标与非目标

**目标**

- 账号用户能看到并管理自己账号下的资源：Worker 及其版本、部署、绑定、secret、路由和域名，KV、D1、R2、Queues、Workflows、Containers。
- 能在浏览器里实时看 Worker 日志（等同 `wrangler tail`），在 D1 上执行 SQL，浏览 KV 键和 R2 对象。
- 平台管理员能建账号、发 token、给账号分配 zone。这些现在只能用 `curl` 调 API 或 `selvedge admin`。
- 不引入新的运行组件，不让集群多一个要做高可用的东西。
- 所有操作走现有的 `/client/v4` API，权限检查只在 API 里做一次，控制台不另起一套。

**非目标**

- **部署代码**：上传和构建仍用 wrangler。控制台不提供在线编辑器，也不接 Git 仓库。
- **按 Worker 的流量统计**：Cloudflare 控制台的请求数、错误率、CPU 时间图表依赖按脚本的分析数据。Selvedge 的 Prometheus 指标刻意不带账号 id（序列数不随租户增长，见 [metrics.md](../metrics.md)），所以 v1 没有数据可画。运维看的集群指标另外提供 Grafana 仪表盘（§9）。
- **注册、登录、计费、团队成员**：Selvedge 没有用户体系，只有 token（§4）。
- **复制 Cloudflare 控制台**：交互习惯接近，方便 Cloudflare 用户上手，但名称、标志和配色都是 Selvedge 自己的（§3.3）。

## 2. 现状：API 已覆盖什么

控制台需要的读写操作，绝大多数 wrangler 已经在用，所以 API 里都有：

| 区域 | 现有 API（`/client/v4/accounts/{account}` 下） | 控制台用途 |
| --- | --- | --- |
| 账号 | `GET/POST /client/v4/accounts`、`GET /accounts/{id}`、`GET /memberships`、`POST /selvedge/tokens` | 账号列表与切换、建账号、发 token |
| Worker | `GET /workers/scripts`，`/workers/scripts/{script}` 及其 `settings`、`secrets`、`schedules`、`subdomain`、`routes`、`content`；`/workers/scripts/{script}/versions`、`deployments` | 列表、详情、版本与部署历史、回滚、secret、Cron、workers.dev 开关 |
| 路由与域名 | `/client/v4/zones`、`/zones/{zone}/workers/routes`，`/workers/domains` | zone 分配（管理员）、路由、自定义域名 |
| 日志 | `POST /workers/scripts/{script}/tails` 创建会话，WebSocket `/tail/v1/<token>`（子协议 `trace-v1`） | 实时日志 |
| KV | `/storage/kv/namespaces`，`keys`、`values/{key}`、`metadata/{key}`、`bulk` | 命名空间、键列表、查看和编辑值 |
| D1 | `/d1/database`，`query`、`raw`、`export`、`import` | 数据库列表、SQL 控制台、导入导出 |
| R2 | `/r2/buckets`，`objects/{key}` 的读写删，`lifecycle`、`domains/managed`、`domains/custom`、`/event_notifications/r2/{bucket}/configuration` | 存储桶、对象、生命周期、公开访问、事件通知 |
| Queues | `/queues`，`consumers`、`messages`、`messages/batch`、`purge` | 队列、消费者、发送测试消息、清空 |
| Workflows | `/workflows/{name}/instances`，`status`、`events/{event}` | 实例列表、状态、暂停、恢复、终止、发事件 |
| Containers | `/containers/applications`、`rollouts` | 应用、实例数、rollout |
| DO | `/workers/durable_objects/namespaces` | 命名空间列表 |

缺口不多，见 §6。

## 3. 组件库：Kumo

### 3.1 为什么选 Kumo

[Kumo](https://github.com/cloudflare/kumo)（`@cloudflare/kumo`）是 Cloudflare 开源的 React 组件库，MIT 许可，2026-11-16 发布了 2.19.0，更新很频繁。

- **交互习惯和 Cloudflare 控制台一致**：Cloudflare 自己的产品界面就用它。用户从 Cloudflare 迁过来，看到的表格、表单、确认删除弹窗都是熟悉的样式。
- **组件够用**：管理后台需要的基本都有，包括 `table`、`pagination`、`sidebar`、`breadcrumbs`、`tabs`、`dialog`、`dropdown`、`combobox`、`command-palette`、`select`、`input`、`sensitive-input`（secret）、`code`（带 Shiki 高亮）、`clipboard-text`、`toast`、`empty`、`badge`、`meter`、`chart`（ECharts）、`date-range-picker`。还有三个可以拷进项目改的 block：`page-header`、`resource-list`、`delete-resource`，正好对应资源列表页和“输入名字确认删除”的流程。
- **可访问性**：基于 Base UI，键盘导航、焦点管理和 ARIA 属性都已处理好。
- **暗色模式自带**：颜色 token 用 CSS `light-dark()`，在根元素上设 `data-mode` 即可切换。

### 3.2 它带来的约束

- **React 18 或 19**，以及 peer 依赖 `@phosphor-icons/react`（图标）、`zod`、`echarts`。只要不用 `chart` 组件，ECharts 就不会打进包里。
- **样式**：Kumo 用 Tailwind CSS v4。有两种接法：自己跑 Tailwind，并用 `@source` 扫描 Kumo 的 `dist`；或者直接引入预编译的 `@cloudflare/kumo/styles/standalone`。控制台也要写一些自己的布局，所以选前者，用 Vite 的 Tailwind 插件。
- **颜色规则**：Kumo 不许直接用 Tailwind 原始颜色（`bg-blue-500`）和 `dark:` 变体，只能用语义 token，控制台的代码照此执行。
- **只发 ESM**：Kumo 自己的开发环境要求 Node 24，构建控制台用同一版本。
- **版本跟进**：Kumo 还在 2.x，迭代快。锁定精确版本，和 workerd 一样由机器人定期提升级 PR，测试通过再合并。

### 3.3 品牌

Kumo 自带一个 `cloudflare-logo` 组件，默认配色也是 Cloudflare 的。控制台**不用这个组件**，也不用 Cloudflare 的名称和标志。标题是“Selvedge”，主色通过覆盖 Kumo 的语义 token 换成 Selvedge 自己的颜色，页脚注明“Selvedge 是独立项目，与 Cloudflare 无关”。组件库本身是 MIT 许可，可以使用；要避免的是让人误以为这是 Cloudflare 的官方产品。

### 3.4 备选

- **shadcn/ui、Mantine 等通用组件库**：同样可行，但做出来不像 Cloudflare，迁移用户的熟悉感会少一些。Kumo 本身也是 Base UI 加 Tailwind，和 shadcn 一路，维护难度相近。
- **服务端渲染**（在 Rust 里用模板加 htmx）：不需要 Node 构建，但实时日志、SQL 控制台、对象浏览这类交互要写大量手工脚本，也用不上现成的组件库。不选。

## 4. 认证与权限

Selvedge 只有 token，没有用户名和密码：

- **管理员 token**：`api_tokens.account_id` 为空，可以访问所有账号，能建账号、分配 zone。
- **账号 token**：只能访问自己的账号。

控制台的登录页让用户粘贴 token，然后调 `GET /client/v4/user/tokens/verify` 验证，再用 `GET /client/v4/accounts` 列出可访问的账号：管理员看到全部，账号 token 只看到一个。之后每个请求都带 `Authorization: Bearer`，权限完全由 API 判断，控制台只根据身份决定显示哪些菜单。

**token 存在哪里**：v1 存在 `sessionStorage`，关掉标签页就要重新登录，不写 `localStorage`。代价是页面一旦被注入脚本（XSS），token 就可能被偷走。为此：

- 严格的 CSP：`default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'`，不允许内联脚本，所有资源从同源加载，不用任何 CDN；
- 不渲染用户提供的 HTML，KV 值、R2 对象、D1 结果和日志都按文本显示；
- 控制台和 API 同源，API 不需要开 CORS。

**备选**：用 token 换一个 HttpOnly 的会话 cookie，脚本读不到 token。但 API 现在只认 Bearer，要改成 cookie 就得再加会话表和 CSRF 防护，而且会让 wrangler 在用的认证路径变复杂。v1 先不做；如果以后要接 OIDC 单点登录，再一起设计。

## 5. 架构与部署

```
浏览器 ──► 网关/负载均衡 ──► API 服务（已有，多副本）
                              ├─ /dash/*      静态文件：index.html、JS、CSS
                              ├─ /client/v4/* 现有 API
                              └─ /tail/v1/*   日志 WebSocket
```

- **代码位置**：仓库新增 `dash/` 目录，用 Vite、React、TypeScript 和 Kumo；路由用 TanStack Router，请求缓存用 TanStack Query。API 的类型在 `dash/src/api/` 里手写，覆盖控制台用到的接口即可。
- **构建**：Dockerfile 加一个 Node 24 构建阶段，产出 `dash/dist/`，拷进最终镜像的 `/usr/share/selvedge/dash/`。Rust 的构建不依赖 Node，`cargo build` 和现有 CI 不受影响。
- **托管**：API 加一个参数 `--dash-dir <dir>`（环境变量 `SELVEDGE_DASH_DIR`），设了就用 `tower-http` 的 `ServeDir` 在 `/dash/` 下提供文件；不认识的路径回退到 `index.html`，交给前端路由。带哈希的静态文件长期缓存，`index.html` 不缓存，并加上 §4 的 CSP 头。`selvedge all` 单进程模式同样支持。
- **Helm**：新增 `api.dash.enabled`，默认开启，打开时给 API 传 `--dash-dir`。访问入口就是现有 API 的地址（`http://<api>/dash/`）。不新增 Deployment 和 Service，高可用跟着 API 走。
- **开发**：`pnpm dev` 起 Vite 开发服务器，把 `/client/v4` 和 `/tail` 代理到本地的 `selvedge all`。

## 6. 需要补的 API

| 缺口 | 补法 | 说明 |
| --- | --- | --- |
| token 只能创建，不能列出和吊销 | `GET /accounts/{id}/selvedge/tokens` 列出名称和创建时间；`DELETE /accounts/{id}/selvedge/tokens/{token_id}` 吊销 | 表里只存 token 的 SHA-256，列表只返回 id、名称和创建时间，原文创建时只显示一次。需要给 `api_tokens` 加一个 id 列 |
| R2 不能列出对象 | `GET /r2/buckets/{bucket}/objects`，支持 `prefix`、`delimiter`、`cursor`、`per_page` | Cloudflare 有同名接口。数据服务里已有 S3 的 ListObjectsV2 逻辑，可以复用 |
| 不能删除账号 | `DELETE /client/v4/accounts/{id}`（仅管理员） | 要先停掉该账号的进程，再清理它的数据：数据库里的记录、S3 里的脚本、D1、R2、DO 存储。删除不可恢复，界面上要输入账号名确认 |
| 查看队列消息会改变消息状态 | v1 不提供“查看消息” | 现有的 `messages/pull` 会租出消息，它们暂时不再投给消费者，所以控制台只提供发送测试消息和清空 |

其余页面只用现有接口。

## 7. 页面

按 Cloudflare 控制台的结构，左侧是边栏，顶部可以切换账号：

- **概览**：各类资源的数量，以及最近的部署。
- **Workers**：列表（名称、最近部署时间、workers.dev 地址）。详情页按标签分为：
  - 部署：版本列表和当前部署，可以回滚或分流；
  - 设置：绑定只读展示，secret 可增删，兼容日期和标志；
  - 触发器：路由、自定义域名、Cron；
  - 日志：实时 tail，可按状态、方法、文本过滤，可暂停，最多保留 1000 条。
- **KV**：命名空间，键列表（前缀搜索、分页），查看和编辑值与元数据，删除。
- **D1**：数据库列表，SQL 控制台（结果表格，`raw` 模式），表结构（查 `sqlite_schema`），导入导出。
- **R2**：存储桶，对象浏览（按 `/` 分目录、上传、下载、删除），生命周期规则，公开访问，自定义域名，事件通知。
- **Queues**：队列、消费者配置、积压数、发送测试消息、清空。
- **Workflows**：实例列表与状态，可暂停、恢复、终止，或给实例发事件。
- **Containers**：应用、实例数、最近的 rollout。
- **Durable Objects**：命名空间列表。
- **管理**（仅管理员）：账号列表、建账号、给账号发 token 和吊销 token、zone 分配。

危险操作（删除 Worker、存储桶、数据库、账号，清空队列）一律用 `delete-resource` 流程，要求输入资源名确认。

## 8. 测试

- **单元测试**：Vitest，覆盖 API 客户端和数据转换。
- **端到端**：Playwright 对着 `selvedge all` 跑，加进 `ci` 工作流的 e2e 任务。流程是：用 wrangler 部署一个带 KV、D1、R2 绑定的 Worker，再在控制台里走一遍——登录、看到 Worker、开日志并发请求、看到日志行、执行 SQL、上传 R2 对象、吊销 token 后被拒绝。
- **安全**：检查 CSP 头存在，控制台在 `<iframe>` 里打不开。
- **包体积**：首屏 JS 压缩后不超过 300 KB，超了 CI 报错。

## 9. 运维仪表盘（不属于控制台）

Prometheus 指标已经很全，但没有现成的面板。在 `deploy/grafana/selvedge.json` 提供一份 Grafana 仪表盘，内容包括：网关请求量和延迟、runner 健康、账号进程的启停与原因、DO 交接、连接复用、各类绑定调用。Helm 可选生成一个带 `grafana_dashboard` 标签的 ConfigMap，供 Grafana sidecar 自动加载。这部分和控制台互不依赖，可以先做。

## 10. 分阶段

| 阶段 | 内容 | 估计 |
| --- | --- | --- |
| P0 | Grafana 仪表盘（§9） | 1–2 天 |
| P1 | 构建与托管（§5）、登录、账号切换、Workers 列表与详情（只读）、实时日志、管理页（建账号、发 token、吊销 token），以及 §6 的 token 接口 | 1–1.5 周 |
| P2 | KV、D1 SQL 控制台、R2 对象浏览（含 §6 的列对象接口），以及 Queues、Workflows、Containers 页面 | 1–1.5 周 |
| P3 | 写操作：secret、回滚和分流、路由和域名、R2 设置、删除账号（§6） | 1 周 |

每个阶段是独立的 PR，合并前通过 CI，包括 Playwright。

## 11. 待定

- **入口路径**：放在 `/dash/` 下与 API 同源最简单。也有人想单独用一个域名，但那样就要开 CORS，tail 的 WebSocket 地址也要跟着改。v1 只支持同源。
- **单点登录**：要不要在 P3 之后接 OIDC，把外部身份映射到账号？这需要一套会话机制（§4 的备选），先观察实际需求。
- **国际化**：界面先只做英文，和 API、wrangler 的报错语言一致；以后再按需加中文。
- **Kumo 的变化**：如果 Kumo 出 3.x 并有大的不兼容改动，控制台可能要跟着改。§3.2 的锁版本加机器人升级可以让每次升级都经过测试。
