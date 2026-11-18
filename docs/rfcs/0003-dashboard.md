# RFC 0003：Web 管理控制台

- 状态：**草案**。入口、登录方式和界面语言已定（2026-11-18）：单独域名、标准 OIDC、中英文
- 日期：2026-11-18
- 依赖：[RFC 0001](0001-architecture.md) §5（控制面 API）、§8（多租户隔离）
- 依据：Selvedge v0.3.3 的 `/client/v4` API（`crates/selvedge-server/src/api/`），`@cloudflare/kumo` 2.19.0，OpenID Connect Core 1.0 与 Discovery 1.0

Selvedge 现在只能用 wrangler、`/client/v4` API 和 `selvedge admin` 命令行管理，没有 Web 界面。本文提议做一个类似 Cloudflare 控制台的 Web 控制台，用 Cloudflare 开源的组件库 Kumo 搭界面。控制台有自己的域名，用标准 OIDC 登录，界面提供中文和英文。

结论是：资源管理的后端几乎都已具备，新增的工作主要有三块：

- 前端；
- 控制台专用的监听端口和会话（§5），由 API 进程提供，不新增组件；
- 把 OIDC 身份映射到账号（§4）。

## 1. 目标与非目标

**目标**

- 账号成员能看到并管理自己账号下的资源：Worker 及其版本、部署、绑定、secret、路由和域名，KV、D1、R2、Queues、Workflows、Containers。
- 能在浏览器里实时看 Worker 日志（等同 `wrangler tail`），在 D1 上执行 SQL，浏览 KV 键和 R2 对象。
- 用公司现有的身份提供方登录。任何符合 OpenID Connect 标准的提供方都能接，例如 Keycloak、Dex、Authentik、Okta、Microsoft Entra ID、Google Workspace。没有身份提供方时，可以用 API token 登录。
- 平台管理员能建账号、管理成员、发和吊销 token、给账号分配 zone。这些现在只能用 `curl` 调 API 或 `selvedge admin`。
- 界面有简体中文和英文两种语言，可随时切换。
- 不引入新的运行组件，不让集群多一个要做高可用的东西。
- 所有资源操作走现有的 `/client/v4` 处理逻辑，权限判断只有一处。

**非目标**

- **部署代码**：上传和构建仍用 wrangler，控制台不提供在线编辑器，也不接 Git 仓库。`wrangler login`（OAuth 登录 Cloudflare）不在范围内；wrangler 继续用控制台里创建的 API token。
- **按 Worker 的流量统计**：Cloudflare 控制台的请求数、错误率、CPU 时间图表依赖按脚本的分析数据。Selvedge 的 Prometheus 指标刻意不带账号 id（序列数不随租户增长，见 [metrics.md](../metrics.md)），v1 没有数据可画。运维看的集群指标另外提供 Grafana 仪表盘（§10）。
- **自建用户体系**：Selvedge 不存密码、不做注册和找回密码，身份全部来自 OIDC 提供方。
- **SCIM 用户同步、计费**。
- **复制 Cloudflare 控制台**：交互习惯接近，方便 Cloudflare 用户上手，但名称、标志和配色都是 Selvedge 自己的（§3.3）。

## 2. 现状：API 已覆盖什么

控制台需要的资源读写，绝大多数 wrangler 已经在用，所以 API 里都有：

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

API 现在只认 `Authorization: Bearer <token>`。token 分两种：

- **管理员 token**：`api_tokens.account_id` 为空，`Principal::Admin`；
- **账号 token**：`Principal::Account(id)`，只能访问一个账号。

API 进程已经有多个监听端口：公开端口，以及启用 mTLS 时的内部端口。控制台端口沿用这个做法（§5）。

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
- **组件里的文字**：Kumo 组件不自带翻译。按钮、空状态、分页上的文字都由调用方传入，所以控制台的翻译（§8）能覆盖全部界面。
- **版本跟进**：Kumo 还在 2.x，迭代快。锁定精确版本，和 workerd 一样由机器人定期提升级 PR，测试通过再合并。

### 3.3 品牌

Kumo 自带一个 `cloudflare-logo` 组件，默认配色也是 Cloudflare 的。控制台**不用这个组件**，也不用 Cloudflare 的名称和标志。标题是“Selvedge”，主色通过覆盖 Kumo 的语义 token 换成 Selvedge 自己的颜色，页脚注明“Selvedge 是独立项目，与 Cloudflare 无关”（中英文各一份）。组件库本身是 MIT 许可，可以使用；要避免的是让人误以为这是 Cloudflare 的官方产品。

### 3.4 备选

- **shadcn/ui、Mantine 等通用组件库**：同样可行，但做出来不像 Cloudflare，迁移用户的熟悉感会少一些。Kumo 本身也是 Base UI 加 Tailwind，和 shadcn 一路，维护难度相近。
- **服务端渲染**（在 Rust 里用模板加 htmx）：不需要 Node 构建，但实时日志、SQL 控制台、对象浏览这类交互要写大量手工脚本，也用不上现成的组件库。不选。

## 4. 认证与权限

### 4.1 登录方式

- **OIDC**（主要方式）：授权码流程，加 PKCE（`S256`）。Selvedge 作为机密客户端，`client_secret` 只在服务端，浏览器拿不到任何 OIDC token。
  - **配置**：只需要提供方的 `issuer`，其余从 `{issuer}/.well-known/openid-configuration` 自动发现，包括授权、token、JWKS、注销等端点。
  - **登录过程**：
    1. 浏览器访问 `/auth/login`，服务端生成 `state`、`nonce` 和 PKCE 校验码，放进一个短期加密 cookie，然后跳转到提供方。
    2. 提供方跳回 `/auth/callback`。服务端校验 `state`，用授权码换 token。回调用默认的 `response_mode=query`（顶层 GET 跳转），这样 `SameSite=Lax` 的 cookie 能带上；`form_post` 是跨站 POST，带不上，所以不用。
    3. 按标准校验 ID token：签名（JWKS，缓存并在遇到未知 `kid` 时刷新）、`iss`、`aud`、`azp`、`exp`、`iat`、`nonce`。只接受非对称签名算法，`alg` 必须是提供方声明的，拒绝 `none`。
    4. 取出 `sub`、`email`、`email_verified`、`name` 和组声明，建立会话（§4.3）。
  - **组声明**：组可以来自 ID token，也可以来自 userinfo 端点，声明名可配置（默认 `groups`）。
  - **注销**：提供方给了 `end_session_endpoint` 时，退出时一并做 RP-Initiated Logout。
- **API token**（后备方式）：没有身份提供方的小规模部署，或者 OIDC 不可用时的应急入口。用户粘贴 token，服务端验证后建立会话，token 本身不留在浏览器里。设置 `dash.tokenLogin: false` 可以关掉；配置了 OIDC 时建议只给管理员保留。

### 4.2 OIDC 身份怎样对应到账号

现在的 `Principal` 只有两种：`Admin`，以及 `Account(id)`（只能访问一个账号）。一个人可能属于多个账号，所以新增一种：

```rust
pub enum Principal {
    Admin,
    Account(AccountId),
    User { id: UserId, accounts: Vec<AccountId> },  // 新增：OIDC 用户，及其登录时可访问的账号
}
```

身份按 `(iss, sub)` 识别：这是 OIDC 规定的稳定标识，邮箱可能会变。新增两张表：

- `users(id, issuer, subject, email, name, created_at, last_login_at)`：第一次登录时创建。
- `account_members(account_id, user_id NULL, email NULL, role, created_at)`：账号成员。
  - 管理员可以按 `user_id` 添加已登录过的人，也可以按邮箱邀请。
  - 按邮箱邀请的，对方第一次登录、并且提供方声明 `email_verified: true` 时，绑定到该用户，之后只认 `user_id`。邮箱未验证的提供方不能用邀请。

另外支持按组映射，适合由身份提供方统一管人：

- `oidc.adminGroups`：属于这些组的人是平台管理员。
- 每个账号可以设一个 `oidc_group`：属于该组的人自动成为这个账号的成员，不用逐个添加。

组成员资格每次登录时重新读取。从提供方移出组，最迟在当前会话过期后失效。

角色 v1 只有两种：

- **`admin`**：平台管理员，等同管理员 token；
- **`member`**：账号成员，对所属账号的权限等同账号 token。

只读成员等细分角色放到以后（§12）。

### 4.3 会话

- 会话存在 PostgreSQL：`dash_sessions(id_sha256, user_id 或 token_sha256, principal, created_at, last_seen_at, expires_at)`。
- 浏览器只拿到一个随机会话 id，放在 cookie `__Host-selvedge-session` 里，属性为 `Secure; HttpOnly; SameSite=Lax; Path=/`，没有 `Domain`。脚本读不到它，也不会发给其他子域名。
- 有效期：空闲 8 小时，最长 24 小时。可以配置。
- 退出登录会删除会话。管理员移除成员或吊销 token 时，相关会话立刻失效。
- 每次请求时，从会话还原出 `Principal`。成员资格的变动要立刻生效，所以 `User` 的 `accounts` 每次从数据库重新读取，并做短时间缓存。

### 4.4 CSRF 与浏览器安全

控制台用 cookie 认证，必须防跨站请求：

- 写操作（`POST`、`PUT`、`PATCH`、`DELETE`）必须带 `Origin` 请求头，并且等于控制台的地址（`dash.url`），还要带自定义请求头 `X-Selvedge-Dash: 1`。跨站表单设不了自定义请求头，也伪造不了 `Origin`。
- 读操作也一律要求带 `X-Selvedge-Dash: 1`，避免 `<img>`、`<script>` 这类跨站请求读到内容。日志 WebSocket 例外：浏览器不能给 WebSocket 加请求头，所以改为检查握手请求的 `Origin`。
- CSP：`default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self' <OIDC 提供方>`。不允许内联脚本，所有资源从同源加载，不用任何 CDN。
- 不渲染用户提供的 HTML：KV 值、R2 对象、D1 结果和日志都按文本显示。R2 对象的预览用下载代替内联打开。

### 4.5 审计

控制台上的每个写操作都记一行结构化日志，包括用户（`iss`、`sub`、邮箱）或 token 名、账号、方法、路径和结果状态。写进审计表并在界面上查看放到 §12。

## 5. 架构与部署

```
              ┌─ api.example.com ─► API 公开端口 :8787  /client/v4/*、/tail/v1/*（Bearer，wrangler 用）
负载均衡 ─────┤
              └─ dash.example.com ─► API 控制台端口 :8788
                                       /               静态文件：index.html、JS、CSS
                                       /auth/*         OIDC 登录、回调、注销，token 登录
                                       /client/v4/*    同一套处理逻辑，只认会话 cookie
                                       /tail/v1/*      日志 WebSocket
```

- **单独的域名和端口**：控制台用自己的域名（如 `dash.example.com`），对应 API 进程上的一个独立监听端口（`--dash-listen`）。浏览器只和这一个源通信，API 不需要开 CORS。两个端口的认证方式不同：
  - 公开端口只认 Bearer token，和现在一样，wrangler 不受影响；
  - 控制台端口只认会话 cookie，不接受 Bearer。
- **同一套处理逻辑**：控制台端口挂载同一个 `/client/v4` 路由，只是认证提取器不同：`Caller` 从会话 cookie 得到 `Principal`，并做 §4.4 的检查。资源接口不写第二份。
- **日志**：tail 会话返回的 WebSocket 地址取自请求的 `Host` 或 `X-Forwarded-Host`。从控制台端口创建的会话，地址自然指向控制台的域名，不用改。
- **代码位置**：仓库新增 `dash/` 目录，用 Vite、React、TypeScript 和 Kumo；路由用 TanStack Router，请求缓存用 TanStack Query，翻译用 react-i18next（§8）。服务端的会话和 OIDC 代码放在 `crates/selvedge-server/src/dash/`，用 `openidconnect` crate。workspace 禁止 `unsafe`，这个 crate 没有问题。
- **构建**：Dockerfile 加一个 Node 24 构建阶段，产出 `dash/dist/`，拷进最终镜像的 `/usr/share/selvedge/dash/`。Rust 的构建不依赖 Node，`cargo build` 和现有 CI 不受影响。
- **托管**：控制台端口用 `tower-http` 的 `ServeDir` 提供静态文件，不认识的路径回退到 `index.html`，交给前端路由。带哈希的静态文件长期缓存，`index.html` 不缓存，并加上 §4.4 的安全头。
- **Helm**：

  ```yaml
  dash:
    enabled: true
    url: https://dash.example.com   # 控制台的公开地址：cookie、Origin 校验、OIDC 回调地址都以它为准
    service: { type: ClusterIP, port: 8788 }
    tokenLogin: true
    session: { idle: 8h, max: 24h }
    oidc:
      issuer: ""                     # 留空则不启用 OIDC
      clientId: ""
      existingSecret: ""             # 存 client-secret 的 Secret
      scopes: [openid, email, profile]
      groupsClaim: groups
      adminGroups: []
  ```

  新增一个 Service `slv-dash`，指向 API Pod 的 8788 端口。chart 现在不带 Ingress，控制台也一样：由运维把 `dash.example.com` 接到这个 Service，并在前面终止 TLS。`__Host-` cookie 要求 HTTPS，所以 `dash.url` 必须是 `https://`；只有本地开发可以用 `http://localhost`。
- **`selvedge all`**：单进程模式同样支持 `--dash-listen` 和 OIDC 参数。
- **开发**：`pnpm dev` 起 Vite 开发服务器，把 `/auth`、`/client/v4`、`/tail` 代理到本地 `selvedge all` 的控制台端口。

## 6. 需要补的 API 与数据

| 缺口 | 补法 | 说明 |
| --- | --- | --- |
| 会话与 OIDC | `GET /auth/login`、`GET /auth/callback`、`POST /auth/token`（token 登录）、`POST /auth/logout`、`GET /auth/me`（当前用户、角色、可访问的账号） | 只在控制台端口上 |
| 用户与成员 | 表 `users`、`account_members`、`dash_sessions`；`Principal::User` | §4.2、§4.3 |
| 成员管理 | `GET/POST /accounts/{id}/selvedge/members`、`DELETE /accounts/{id}/selvedge/members/{member_id}`；`PUT /accounts/{id}/selvedge/oidc-group`（仅管理员） | 管理员管所有账号；成员能看到同账号的成员列表 |
| token 只能创建，不能列出和吊销 | `GET /accounts/{id}/selvedge/tokens` 列出名称、创建者和创建时间；`DELETE /accounts/{id}/selvedge/tokens/{token_id}` 吊销 | 表里只存 token 的 SHA-256，原文创建时只显示一次。需要给 `api_tokens` 加 `id` 列和 `created_by` 列 |
| R2 不能列出对象 | `GET /r2/buckets/{bucket}/objects`，支持 `prefix`、`delimiter`、`cursor`、`per_page` | Cloudflare 有同名接口。数据服务里已有 S3 ListObjectsV2 的逻辑，可以复用 |
| 不能删除账号 | `DELETE /client/v4/accounts/{id}`（仅管理员） | 要先停掉该账号的进程，再清理它的数据：数据库里的记录、S3 里的脚本、D1、R2、DO 存储。删除不可恢复，界面上要输入账号名确认 |
| 查看队列消息会改变消息状态 | v1 不提供“查看消息” | 现有的 `messages/pull` 会租出消息，它们暂时不再投给消费者，所以控制台只提供发送测试消息和清空 |

新增的 `/selvedge/` 接口在公开端口上也可用，方便脚本自动化，比如用管理员 token 批量添加成员。

## 7. 页面

按 Cloudflare 控制台的结构，左侧是边栏，顶部可以切换账号和语言、查看当前用户：

- **登录页**：“使用 <提供方名称> 登录”按钮；打开 token 登录时，下面多一个 token 输入框。
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
- **账号设置**：成员（管理员可增删，可设 OIDC 组），API token（创建、列出、吊销；创建后只显示一次，并附上 wrangler 的配置示例）。
- **平台管理**（仅管理员）：账号列表，建账号和删账号，用户列表（最近登录时间），zone 分配。

危险操作（删除 Worker、存储桶、数据库、账号，清空队列，吊销 token）一律用 `delete-resource` 流程，要求输入资源名确认。

## 8. 国际化

- **语言**：简体中文（`zh-CN`）和英文（`en`）。首次访问按浏览器的 `navigator.languages` 选择，`zh` 开头的选中文，其余选英文。顶部可以切换，选择记在 `localStorage`，只是界面偏好，不涉及权限。
- **实现**：react-i18next。文案放在 `dash/src/locales/en.json` 和 `zh-CN.json`，按页面分命名空间，按需加载。代码里不允许出现写死的界面文字，用 ESLint 规则检查。
- **数字和时间**：数字、字节数、日期和相对时间用浏览器的 `Intl` 格式化，跟随界面语言。时间统一显示本地时区，悬停时显示 UTC。
- **API 的报错**：API 返回的是兼容 Cloudflare 的英文报错，带数字 `code`。控制台按 `code` 翻译常见报错，比如未授权、找不到、名称冲突、超出限制；翻译不了的显示原文，前面加上一句翻译过的概括。
- **页面以外的文字**：OIDC 提供方的登录页由提供方负责。控制台自己的报错页（比如登录失败、会话过期）也提供中英文，由服务端按 `Accept-Language` 选择。
- **检查**：CI 检查两种语言的键完全一致，缺一个就失败；Playwright 用两种语言各跑一遍主要流程（§9）。

## 9. 测试

- **单元测试**：
  - Vitest：API 客户端和数据转换；
  - Rust：ID token 校验，包括篡改签名、错误的 `aud`、`iss` 和 `nonce`、过期 token、`alg: none`、未知 `kid` 时刷新 JWKS；
  - Rust：会话过期，CSRF 检查，以及成员映射（邀请邮箱未验证时拒绝绑定）。
- **端到端**：在 `ci` 工作流的 e2e 任务里，起一个 [Dex](https://dexidp.io/) 作为 OIDC 提供方，配置静态用户和组，再用 Playwright 对着 `selvedge all` 跑：
  1. 用 wrangler 部署一个带 KV、D1、R2 绑定的 Worker；
  2. 管理员通过组映射登录，建账号，邀请成员；
  3. 成员第一次登录后只看到自己的账号；
  4. 在控制台里看到 Worker，开日志并发请求，看到日志行；
  5. 执行 SQL，上传 R2 对象；
  6. 创建 token 并用 wrangler 调用成功，吊销后被拒绝；
  7. 移除成员后，对方的会话立刻失效。

  主要流程用中文和英文各跑一遍。
- **安全**：检查跨源写请求被拒绝，缺 `X-Selvedge-Dash` 的读请求被拒绝，CSP 头存在，控制台在 `<iframe>` 里打不开；公开端口不认会话 cookie，控制台端口不认 Bearer。
- **Kubernetes**：`kind-ha` 里打开控制台，滚动重启 API 时会话不丢，因为会话存在 PostgreSQL 里。
- **包体积**：首屏 JS 压缩后不超过 300 KB，超了 CI 报错。

## 10. 运维仪表盘（不属于控制台）

Prometheus 指标已经很全，但没有现成的面板。在 `deploy/grafana/selvedge.json` 提供一份 Grafana 仪表盘，内容包括：网关请求量和延迟、runner 健康、账号进程的启停与原因、DO 交接、连接复用、各类绑定调用。Helm 可选生成一个带 `grafana_dashboard` 标签的 ConfigMap，供 Grafana sidecar 自动加载。这部分和控制台互不依赖，可以先做。

## 11. 分阶段

| 阶段 | 内容 | 估计 |
| --- | --- | --- |
| P0 | Grafana 仪表盘（§10） | 1–2 天 |
| P1 | 控制台端口与会话、token 登录、CSRF 与安全头、构建与托管（§5）、国际化框架（§8），Workers 列表与详情（只读）、实时日志 | 1.5 周 |
| P2 | OIDC 登录、`users` 和 `account_members`、组映射、`Principal::User`，账号设置与平台管理页面，以及 token 的列出与吊销（§6）；Dex 端到端测试 | 1.5–2 周 |
| P3 | KV、D1 SQL 控制台、R2 对象浏览（含列对象接口），以及 Queues、Workflows、Containers 页面 | 1–1.5 周 |
| P4 | 写操作：secret、回滚和分流、路由和域名、R2 设置、删除账号 | 1 周 |

每个阶段是独立的 PR，合并前通过 CI，包括 Playwright。每个阶段的中英文文案同时提交。

## 12. 待定

- **细分角色**：除了 `admin` 和 `member`，要不要加只读成员，或者按产品限定权限（比如只能看日志）？这要给 API 的每个处理函数标上所需权限，工作量不小，先看需求。
- **审计表**：§4.5 的写操作日志要不要入库，并在控制台里按账号查看？
- **API token 的创建者**：OIDC 用户创建的 token，要不要在成员被移除时一并吊销？倾向于不自动吊销，只在 token 列表里标出创建者已不是成员，避免误伤 CI。
- **Kumo 的变化**：如果 Kumo 出 3.x 并有大的不兼容改动，控制台可能要跟着改。§3.2 的锁版本加机器人升级可以让每次升级都经过测试。
