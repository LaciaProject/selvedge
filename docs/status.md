# 项目进度、功能与局限

截至 2026-11-10。

Selvedge 是一个开源、高可用、兼容 Cloudflare Workers 的平台：用 `wrangler` 部署的 Worker 不用改代码，就能在自己的 Kubernetes 集群上运行。规划中的主要产品都已实现，并通过了单节点、多 runner 集群和 3 可用区 Kubernetes 高可用测试。设计细节见 [RFC 0001](rfcs/0001-architecture.md)，逐项兼容性差异见 [compatibility.md](compatibility.md)。

## 设计约束

- **未修改的 workerd**：直接使用 Cloudflare 开源的运行时，不 fork、不打补丁，能跟随上游升级。
- **Rust 实现**：控制面、网关、runner agent 都用 Rust 编写，workspace 禁止 `unsafe`。
- **单集群**：在一个 Kubernetes 集群内跨可用区做高可用，不做跨地域的全球分发。
- **依赖可替换的开源组件**：元数据存在 PostgreSQL，对象存在任意 S3 兼容服务（测试用 SeaweedFS），用 Helm 部署。
- **API 兼容**：实现 wrangler 实际会调用的那部分 Cloudflare API（`/client/v4/...`），用 API token 认证。

## 当前进度

最新发布是 v0.2.0，带来 Containers：镜像 `ghcr.io/laciaproject/selvedge:0.2.0`，Helm chart `oci://ghcr.io/laciaproject/charts/selvedge`。第一个发布版本是 v0.1.1（`v0.1.0` 标签的发布因镜像路径大小写问题失败，没有产出任何制品）。代码分 30 个 PR 依次合并进 `main`，每个 PR 合并前都通过了 CI。

| PR | 内容 |
| --- | --- |
| [#1](https://github.com/LaciaProject/selvedge/pull/1) | v0.1：Workers、KV、R2、Durable Objects、D1、Cron、Queues、Helm、高可用测试 |
| [#2](https://github.com/LaciaProject/selvedge/pull/2) | Static Assets |
| [#3](https://github.com/LaciaProject/selvedge/pull/3) | `wrangler tail` 实时日志 |
| [#4](https://github.com/LaciaProject/selvedge/pull/4) | 租户隔离：每个账号用独立的 Linux 用户运行，DO 存储经 S3 代理访问，禁用 experimental 标志 |
| [#5](https://github.com/LaciaProject/selvedge/pull/5) | Workflows |
| [#6](https://github.com/LaciaProject/selvedge/pull/6) | R2 的 S3 兼容 API |
| [#7](https://github.com/LaciaProject/selvedge/pull/7) | 组件间 mTLS、按账号资源限制、灰度发布、zone 路由、静态资源回收、修复连接池死锁 |
| [#8](https://github.com/LaciaProject/selvedge/pull/8) | 本文档 |
| [#9](https://github.com/LaciaProject/selvedge/pull/9) | mTLS 按组件角色签发证书，两端都核对角色 |
| [#10](https://github.com/LaciaProject/selvedge/pull/10) | 超内存重启先注销租约，DO 不再停 20 秒 |
| [#11](https://github.com/LaciaProject/selvedge/pull/11) | R2 公开访问 URL 和自定义域名 |
| [#12](https://github.com/LaciaProject/selvedge/pull/12) | R2 事件通知；支持 `inherit` 绑定 |
| [#13](https://github.com/LaciaProject/selvedge/pull/13) | 发布工作流：打标签后发布镜像和 Helm chart |
| [#14](https://github.com/LaciaProject/selvedge/pull/14) | R2 事件通知与写入同一事务入队 |
| [#15](https://github.com/LaciaProject/selvedge/pull/15) | `request.cf` 从负载均衡的请求头映射地理和网络字段 |
| [#16](https://github.com/LaciaProject/selvedge/pull/16) | Queues HTTP pull 消费者和 HTTP 推送 |
| [#17](https://github.com/LaciaProject/selvedge/pull/17) | Queues 暂停投递、默认延迟、保留期、清空、`max_batch_timeout` |
| [#18](https://github.com/LaciaProject/selvedge/pull/18) | D1 导出和文件导入 |
| [#19](https://github.com/LaciaProject/selvedge/pull/19) | R2 生命周期规则 |
| [#20](https://github.com/LaciaProject/selvedge/pull/20) | R2 S3 接口的虚拟主机寻址 |
| [#21](https://github.com/LaciaProject/selvedge/pull/21)–[#23](https://github.com/LaciaProject/selvedge/pull/23) | 本文档更新到 #20；发布工作流的镜像路径改小写；v0.1.1 |
| [#24](https://github.com/LaciaProject/selvedge/pull/24) | 卡死检测：账号进程 30 秒不响应就杀掉重启 |
| [#25](https://github.com/LaciaProject/selvedge/pull/25) | workerd 1.20261109.1：进程停止前先交接 Durable Object |
| [#26](https://github.com/LaciaProject/selvedge/pull/26) | 组件之间复用连接，加请求路径基准测试 |
| [#27](https://github.com/LaciaProject/selvedge/pull/27) | 每个组件的 Prometheus 指标 |
| [#28](https://github.com/LaciaProject/selvedge/pull/28) | 每天自动检查 workerd 新版本 |
| [#29](https://github.com/LaciaProject/selvedge/pull/29) | RFC 0002：Containers（沙箱选用 Kata） |
| [#30](https://github.com/LaciaProject/selvedge/pull/30) | Containers：实现 RFC 0002，含 `exec()`、Helm 和 Kata 集群测试；v0.2.0 |

CI 包含以下几项，测试都由真实的 `wrangler` 和 AWS SDK 发起请求：

- `rust`：单元测试；
- `e2e`：单节点、双 runner 集群、root 模式（每个账号切换到独立用户运行）、开启 mTLS 的集群；
- `kind-ha`：3 可用区 Kubernetes 高可用测试；
- `kind-containers`：容器跑在 Kata 虚拟机里的 Kubernetes 测试，覆盖镜像推送和拉取、出站隔离、NetworkPolicy、随 runner 回收。单节点的 Docker 版容器测试在 `e2e` 里。

## 已实现功能

Cloudflare Workers 的核心产品都已可用。把 `CLOUDFLARE_API_BASE_URL` 指向 Selvedge 的 API，对应的 `wrangler` 命令就能直接使用。

| 产品 | 支持的内容 | 实现方式 |
| --- | --- | --- |
| Workers 运行时 | ES module、多模块（ES、CommonJS、Wasm、JSON、Python 等）、兼容日期与标志、出站 `fetch()` 和 `connect()` | workerd `1.20261109.1`，每个账号一个进程 |
| 部署与版本 | `deploy`、`versions upload/deploy`、`rollback`、`delete`、`secret`，约 2 秒内生效 | agent 跟随变更流，先启动新版本再退掉旧版本 |
| 灰度发布 | 两个版本按百分比分流，支持 `Cloudflare-Workers-Version-Key` 和 `-Overrides` 头 | 每个版本是单独的 workerd 服务 |
| 路由 | workers.dev 式子域名、自定义域名、zone 路由（如 `*.example.com/*`），最具体的规则优先 | 由网关匹配；zone 由管理员分配给账号 |
| KV | 全部绑定操作、TTL、元数据、`wrangler kv` | PostgreSQL，强一致 |
| R2 | 全部绑定操作、条件请求、分片上传、`wrangler r2`；公开访问 URL（`pub-<id>.<zone>`，相当于 r2.dev）和自定义域名；对象变更通知到队列；生命周期规则 | 元数据存 PostgreSQL，数据存 S3；公开读取由 data 服务的 S3 端口提供 |
| R2 S3 API | 常用的桶、对象、分片操作，SigV4（含预签名 URL），`aws-chunked` 请求体 | 与绑定共享同一批对象，已用 AWS SDK 测试 |
| Durable Objects | fetch、RPC、WebSocket 休眠、SQLite 与 KV 存储、alarms | workerd 集群模式，存储写入 S3 后才确认 |
| D1 | `prepare/bind/first/all/run/raw/batch/exec`，以及 `wrangler d1` 的常用命令、迁移、导出和文件导入 | 每个数据库是一个 SQLite Durable Object |
| Queues | 生产者、Worker 消费者和 HTTP pull 消费者、HTTP 推送、重试、延迟、死信队列、暂停投递、清空、保留期，至少投递一次 | 消息存在 PostgreSQL，投递时加租约 |
| Cron Triggers | 每个脚本最多 5 个，支持 `L`、`W`、`#` | 数据服务的多个副本抢同一次触发，每次只执行一次 |
| Static Assets | `html_handling`、SPA 和 404 页、`run_worker_first`、`_headers`、`_redirects`，只上传有变化的文件，自动回收不再用的文件 | 未修改的 Cloudflare asset worker 和 router worker |
| Workflows | `step.do`、`sleep`、`sleepUntil`、`waitForEvent`，实例管理，`wrangler workflows` 全部子命令 | 未修改的 Cloudflare Workflows 引擎，每个实例一个 Durable Object |
| `wrangler tail` | pretty 和 json 两种格式及全部过滤器，包括 console 日志与异常 | tail worker 加 PostgreSQL `LISTEN/NOTIFY` |
| Containers | DO 通过 `ctx.container` 控制容器：`start`、`getTcpPort().fetch()`、`monitor`、`destroy`、出站拦截、`enableInternet`、`exec()`；`wrangler deploy` 构建、推送镜像并创建应用；`max_instances` 和实例规格 | 运营方开启后可用。每个实例是一个跑在 Kata 虚拟机里的 Pod，由唯一持有集群凭证的控制组件创建；镜像仓库用 API 签发的 token 认证。见 [containers.md](containers.md) |
| 服务绑定、环境变量、密钥 | 支持命名入口点；密钥用 AES-256-GCM 加密存储 | 只能在同一账号内使用 |

## 高可用与安全

在 3 可用区的 kind 集群上开启 mTLS 测试：

- 滚动重启期间发出 57 个请求，没有一个失败；
- 断掉一个可用区时，17 个无状态请求失败 1 个，是被杀节点上正在处理的那个。

**高可用**

- API、网关、数据服务和 runner 都是多副本，分散在不同可用区。状态只存在 PostgreSQL 和 S3 里。
- runner 在本地缓存配置和静态资源，数据服务不可用时，已部署的 Worker 照常服务。
- 每个 Durable Object 同一时间只有一个所属进程。正常关闭（如滚动升级、超内存重启）时，workerd 逐个交接对象：先让对象上正在处理的请求完成（最多 4 秒），再交给下一个请求所在的进程；进程崩溃或被杀而 agent 还在时，由 agent 代它放弃租约，对象约 1 秒内转移；整个 runner 宕机时要等租约过期，最多约 20 秒。
- Cron 和 Queues 靠数据库里的抢占和租约避免重复执行。副本崩溃后租约过期，消息会重新投递。

**可观测性**

- 每个组件在单独的端口上提供 Prometheus 指标，Helm 默认开启：请求数和延迟、runner 健康、连接复用、账号进程的启动和停止原因（含内存超限和卡死）、进程崩溃、CPU 降权、各类绑定调用。指标不带账号 id，序列数不随租户增长。详见 [metrics.md](metrics.md)。

**安全与隔离**

- **组件间 mTLS**：Helm 默认开启，自动生成 CA，并为 API、data、gateway、runner 各签发一张证书。每个内部端口只接受调用它的角色，所以 runner 的证书连不上其他 runner，也不能把请求路由进账号进程。内部接口在单独的端口上监听，公共端口拒绝内部请求，NetworkPolicy 限制可访问的端口。
- **按账号隔离进程**：每个账号一个 workerd 进程，用独立的 Linux 用户运行，账号之间互相访问不到对方的文件和 socket。
- **不给 workerd 存储凭证**：workerd 只能通过 agent 的 S3 代理访问 DO 存储，每个进程一把自己的密钥，只能访问自己账号的那部分数据，进程结束后密钥即失效。
- **资源限制**：按账号限制内存（默认 1024 MB）、CPU 和进程数。
- **卡死检测**：workerd 不限制单个请求的 CPU 时间，死循环会卡住整个账号进程（连 SIGTERM 都不响应）。agent 每 2 秒探测一次每个进程里的健康检查 Worker，超过 30 秒（`--hang-timeout`）没有响应就直接杀掉，下一个请求会启动新进程。
- **出站限制**：拒绝访问私有地址、回环地址和链路本地地址。
- **禁用 experimental 标志**：上传时拒绝 workerd 的 experimental 兼容标志，与 Cloudflare 一致。
- **`request.cf` 不可伪造**：由网关设置，客户端无法自己指定。
- **容器**：runner 不持有集群凭证，只能请求启动本账号已部署的应用镜像，镜像、规格、沙箱和拉取凭证都由控制组件决定。实例默认跑在 Kata 虚拟机里，不挂 ServiceAccount token，NetworkPolicy 只允许它连 runner 的出站中转和公共 DNS；中转按来源地址把流量交给所属 DO 的 workerd。

## 已知局限

下表中，对多数 Worker 影响最大的是前三项。

| 方面 | 局限 |
| --- | --- |
| DO 故障转移 | 整个 runner 宕机（节点故障、Pod 被杀）后，对它所持对象的调用最多失败约 20 秒。这是 workerd 内部固定的租约时长：agent 也不在了，没人能确认进程已死，只能等租约过期。只是 workerd 进程崩溃、被 OOM 杀掉或因卡死被杀时，agent 代它放弃租约，对象约 1 秒内转移，只有正在处理的请求会失败。滚动升级和超内存重启会先交接对象，不用等，正在处理的请求也能完成。存储写入的延迟取决于 S3。S3 必须支持条件写（已测 SeaweedFS 4.50） |
| CPU 和内存限制 | 限制针对整个账号进程，不针对单个请求。一个请求失控会让进程重启，同账号其他正在处理的请求也会失败。cgroup 不可写时（K8s 里常见）由 agent 自己监控：内存超限约 2 秒内重启进程（有 DO 的进程先给 10 秒交接，期间新请求已由新进程处理；没有 DO 的给 3 秒），CPU 超限只降低优先级，不会被硬性封顶。陷入死循环的请求要等卡死检测超时（默认 30 秒）才会被终止，这期间同账号的所有请求都会挂起；被杀进程上的 DO 随即转移 |
| `request.cf` | 地理和网络字段来自前面负载均衡注入的请求头（`--cf-header` 映射），Selvedge 自己没有 GeoIP 数据库；没有 `botManagement` 等 Cloudflare 专有字段 |
| 灰度发布 | 定义了 DO 或带静态资源的 Worker 不能分流。服务绑定、Cron 和队列只发往占比大的版本 |
| mTLS | runner 的证书仍能读取 API 内部接口提供的所有账号配置，因为任何账号都可能被调度到任何 runner 上。`selvedge all` 单进程模式不支持 TLS |
| 路由 | zone 只能由管理员分配，Selvedge 不验证域名所有权 |
| R2 | 生命周期规则不做存储类别转换（只有 `Standard`）。公开 URL 没有限流和缓存，TLS 由前面的负载均衡终止。S3 API 没有 tagging、ACL、版本控制和 CORS（虚拟主机寻址需要运营方配置域名） |
| D1 | 没有读副本和 Time Travel；导入文件最大 64 MB，导入导出都在 Durable Object 内存中完成 |
| Queues | 不支持 `v8` 内容类型；`queue.metrics()` 只有积压数 |
| Workflows | 不跟踪版本；`schedules`、`concurrency` 和保留期配置会被忽略 |
| `wrangler tail` | 尽力投递，超过约 7.9 KB 的事件会被截断 |
| 运行时与 API | 不支持 service-worker 语法和 Global API key |
| Containers | 冷启动是秒级（Pod 调度、首次拉取镜像、虚拟机启动），比 Cloudflare 慢；沙箱要求节点支持 KVM。不支持快照和 TERM/INT/QUIT/KILL 以外的信号，Kubernetes 上 `exec()` 不能指定用户；rollout 立即生效；删除应用后镜像层要等仓库 GC 回收 |

**不在范围内**：Workers AI、Vectorize、Hyperdrive 和 Analytics Engine 是 Cloudflare 自己托管的服务，不属于基础设施，使用这些绑定的上传会被拒绝。跨地域多集群也不在当前范围内。

## 下一步

按上面局限表的优先级继续。Durable Object 故障转移只剩整个 runner 宕机这一种情况要等约 20 秒的租约；要缩短它，需要 workerd 把租约时长做成可配置项。
