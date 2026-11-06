# RFC 0001：Selvedge 总体架构

- 状态：**已采纳**，按此实现中（实现进度见 §15）
- 日期：2026-11-06
- 范围：v1（单集群、跨可用区）

## 1. 定位

Selvedge 是一个开源、运行在 Kubernetes 上、高可用的 Workers 平台，兼容 Cloudflare Workers 的编程模型和 `/client/v4` API。
用户用现有的 `cf` / Vite 插件工作流部署，代码不用改。

**目标**

- 单个 Kubernetes 集群内跨可用区高可用：任意一个节点或一个可用区故障，已部署的应用继续服务。
- 能安全地运行来自不同租户的不可信代码。
- 运行时使用官方 workerd，不维护 fork。
- 同一套代码路径支持两种部署档位：`minimal`（单节点、几分钟装好）和 `ha`（多副本、跨可用区）。

**非目标（v1）**

- 多区域、多集群、全球放置。数据模型不为此预留结构。
- Containers、Workers AI 推理、Browser Rendering、Hyperdrive、Analytics Engine。
- 一致性比 Cloudflare 公开的语义更强。只对齐它公开承诺的语义。

## 2. 已定的约束

| 约束 | 决定 | 影响 |
| --- | --- | --- |
| 租户模型 | 支持不可信多租户 | 默认安全配置按最坏情况设计；隔离边界是进程，不是 V8 isolate（见 §8） |
| 部署范围 | 只做单集群跨 AZ | DO 放置、KV 复制都只在一个集群内解决 |
| 与 open-compute 的关系 | 另起项目，复用部分资产 | 见 §11 |
| 语言 | Rust | 控制面、数据面、节点 agent 都用 Rust |
| 运行时 | 尽量用官方 workerd 原版 | 不打补丁。缺的能力在 workerd 外面补，补不了的写进兼容性差异（见 §10） |
| Durable Objects | 使用官方 workerd 的 cluster 模式 | DO 的权威数据在 S3 兼容存储里，不需要块存储（§7） |
| 对象存储 | 必须支持条件写入（`If-None-Match: *`、`If-Match`） | workerd 启动时会检查，不支持就拒绝启动。候选后端见 S3 |
| 许可证 | Apache-2.0 | 和 workerd 一致 |
| 大规模测试 | 在 GitHub Actions 上跑，用专门的分支触发 | 故障注入、多节点、升级测试都写成 CI workflow，本地只跑单元测试和小规模集成测试（见 §12.1） |

> 修订记录：初稿曾计划用"跨 AZ 同步复制的块存储"承载 DO（已获同意）。之后发现上游 workerd 已经提供 cluster 模式，
> 它把 DO 的状态放进 S3 兼容存储，故障切换更快，也不需要 StatefulSet 和节点隔离流程，所以改用 cluster 模式，块存储不再是前提。

## 3. 总体结构

```
                  ┌──────────────── Kubernetes 集群（跨 AZ）────────────────┐
  客户端 ──▶ LB ──▶ selvedge-gateway (Deployment, ≥3)                        │
                  │      │ 按 host/route 查本地路由表                        │
                  │      ▼                                                    │
                  │  selvedge-runner (Deployment + HPA)                       │
                  │   ├─ agent：拉代码包、生成 workerd 配置、管进程、转发 binding│
                  │   └─ workerd × N（每个账户一个进程，官方二进制）          │
                  │        同一账户的各个进程组成一个 workerd cluster，         │
                  │        DO 由 cluster 分布到这些进程上                      │
                  │      │                                                    │
                  │      ├─ binding 调用（经 agent，带租户身份）─▶ selvedge-data│
                  │      └─ DO 状态、租约、alarm ─────────────▶ 对象存储       │
                  │                                                           │
                  │  selvedge-data (Deployment, ≥3)   KV / R2 / Queues 服务   │
                  │  selvedge-api (Deployment, ≥2)    /client/v4、Dashboard   │
                  │  selvedge-controller (P2 引入, 选主)  Cron、Queue 调度     │
                  │                                                           │
                  │  依赖：PostgreSQL (CNPG)   S3 兼容对象存储（支持条件写入） │
                  └───────────────────────────────────────────────────────────┘
```

所有 Selvedge 组件编进同一个 Rust 二进制 `selvedge`，用子命令区分角色（`selvedge gateway`、`selvedge agent` 等），
这样版本对齐和发布都简单。面向用户的 CLI 叫 `selv`。

整个系统只有两个有状态依赖：Postgres 和对象存储。Selvedge 自己的所有组件都是无状态的 Deployment。

**可用性原则：控制面挂了，数据面继续跑。** gateway 和 agent 都在本地缓存路由表、代码包和配置；
`selvedge-api`、`selvedge-controller` 甚至 Postgres 短暂不可用时，已部署的 Worker 继续处理请求，
只是不能部署、Cron 会延迟。DO 只依赖对象存储，不依赖 Postgres。

## 4. 组件

### 4.1 selvedge-gateway

- v1 直接基于 hyper 实现（HTTP/1.1，支持 WebSocket 升级）。[Pingora](https://github.com/cloudflare/pingora) 留作以后的性能优化选项。
- 路由表（hostname → 账户 → script）由 gateway 每 2 秒从 api 拉取（带 ETag），本地全量缓存；api 不可用时继续用旧表。
- 把请求转给持有该账户进程的 runner Pod，按一致性哈希加有界负载选择，至少分布在两个 AZ。
- TLS 证书用 cert-manager 签发。对外暴露用标准 Service / Gateway API，不绑定云厂商 LB。

### 4.2 selvedge-runner：执行层

每个 runner Pod 里有一个 agent 和若干 workerd 进程。

- **每个账户一个 workerd 进程**。同账户的 script 在同一进程里，所以同账户内的 Service Binding 直接走 workerd 原生调用。
- **同账户的进程组成一个 workerd cluster**（§7）。每个账户的 cluster 有自己的对象存储前缀、存储凭据和 cluster 密钥。
- **代码包按内容哈希存储**在对象存储里，agent 按需拉取并缓存到本地。回滚就是换一个哈希。
- **放置**：不需要集中调度。gateway 对健康的 runner 做 rendezvous 哈希，每个账户固定落在排名前 2 的 runner 上
  （每个请求随机选其一，两边都保持热），其余 runner 按排名依次作为后备。所有 gateway 算出的结果一致。
  冷账户在第一个请求到来时才启动进程，空闲一段时间后回收。
- **部署是滚动替换进程，不是热更新。** workerd 的配置是静态的，cluster 模式也只在启动时读取配置。
  agent 先启动新配置的进程，让它加入 cluster，健康检查通过后切流量，再给旧进程发 SIGTERM。
  旧进程停止认领新对象并注销自己，它持有的 DO 在下一个请求到来时由新进程接管，不用等租约过期。
  不依赖 Worker Loader 做动态加载，因为官方版本里动态加载的 Worker 拿不到原生的 KV/R2 等绑定对象。
- Pod 只需要 `emptyDir` 作为 workerd 的 `localDisk`。它只是缓存，权威数据在对象存储里。

### 4.3 selvedge-data：binding 后端

官方 workerd 的 `kvNamespace`、`r2Bucket`、`queue` 绑定本来就是把调用发给一个服务。Miniflare 就是这样做的。
Selvedge 实现这些服务端协议：

- workerd 进程只能连到 agent 为它单独开的 Unix socket。agent 在转发时加上账户身份，租户代码无法伪造。
- agent 用 mTLS 访问 selvedge-data。数据库凭据和全局对象存储凭据只在 selvedge-data 里，不进 runner Pod。

### 4.4 selvedge-api

- 实现 `/client/v4` 的子集，让 `cf` 和官方 SDK 能直接用。
- 元数据全部存在 Postgres。租户数据（Worker、KV namespace 等）不存成 CRD。
- Dashboard 也由它提供。

### 4.5 selvedge-controller（P2 再引入）

v1 不需要它：路由表由 gateway 直接从 api 拉取，放置用 rendezvous 哈希，每账户的 cluster 密钥由各 agent 从共享密钥派生。
做 Cron 和 Queue consumer 调度时（§6）再引入，用 Kubernetes Lease 选主，同一时间只有一个实例工作；
它挂掉或正在切主时，已有流量不受影响，只是 Cron 会延迟。

## 5. 请求路径

1. 客户端 → LB → gateway。gateway 查本地路由表，确定账户和 version。
2. gateway → 某个 runner Pod 的 agent → 该账户的 workerd 进程（Unix socket）。
3. Worker 调用 KV/R2/Queue → workerd → agent → selvedge-data。
4. Worker 调用 DO → workerd 原生 DO 绑定。如果对象归本进程，直接调用；
   否则 workerd 通过 cluster 端口转给持有它的进程。HTTP、WebSocket 和 JS RPC 都原样转发。

## 6. 各产品的状态方案

| 产品 | 一致性（对齐 Cloudflare） | 方案 | 阶段 |
| --- | --- | --- | --- |
| Workers | 无状态 | runner，每账户一个进程，跨 AZ 至少两份 | P1 |
| Static Assets | 不可变 | 按内容哈希存对象存储，runner 本地缓存 | P1 |
| KV | 最终一致，约 60 秒 | 以 Postgres 为准（大值放对象存储），runner 本地缓存加 TTL | P1 |
| R2 | 强一致 | selvedge-data 转到后端 S3，每个 R2 bucket 对应后端的一个前缀；同时提供 S3 兼容端点 | P1 |
| Secrets | — | 信封加密。主密钥 `ha` 档放 KMS/Vault（经 External Secrets 注入），`minimal` 档放 K8s Secret | P1 |
| 日志、Live Tail | 流式 | workerd 的输出由 agent 带上账户标签写到自己的 stdout（可选 JSON），由集群的日志采集器送到 Loki 等；Live Tail 由每个账户进程里的 tail worker 推给 agent，只转发有人在 tail 的脚本，API 经 PostgreSQL `LISTEN/NOTIFY` 送到持有 WebSocket 的副本 | P1 |
| Durable Objects | 每个对象全局唯一、强一致 | workerd cluster 模式（§7） | P2 |
| Alarms、WebSocket Hibernation | 同 DO | workerd 原生实现；cluster 模式下 alarm 存在对象存储里，节点故障后由其他节点接管 | P2 |
| D1 | 单写 | 每个数据库是一个 Selvedge 提供的系统 DO 类（Miniflare 也这样实现），自动获得 cluster 模式的高可用 | P2 |
| Queues | 至少投递一次 | Postgres 表加 `FOR UPDATE SKIP LOCKED`，controller 负责把消息分发给 consumer | P2 |
| Cron | 允许偶尔重复 | Postgres 中每个触发时刻一行，认领时带 fencing token，然后分发到 runner | P2 |
| Workflows | 持久执行 | workerd 内置的 `WorkflowsEngine`，跑在 DO 上，跟随 cluster 模式（需验证，见 S1） | P3 |

**为什么 Queues 不用 NATS**：开源项目每多一个有状态依赖，安装和运维成本都会上升。
v1 用 Postgres 足够，存储层做成接口，以后有需要再加 NATS JetStream 后端。

## 7. Durable Objects：使用 workerd cluster 模式

上游 workerd（本文核对的是 2026-11-06 的 `main`，提交 `3189460a`）已经有实验性的 cluster 模式
（`Config.cluster`，需要 `--experimental`，代码在 `src/workerd/server/cluster/`）。
它解决了 DO 高可用里最难的几件事，Selvedge 直接使用，不自己实现。

### 7.1 cluster 模式做了什么

- **单一所有者**：每个 DO 同一时间只在一个进程上运行。认领一个对象就是对 `own.json` 做一次条件写入；
  接管租约已过期的所有者时，所有权代数（epoch）加一。旧所有者迟到的写入落在没人读的位置，不会产生两个写入者。
- **持久性**：一次提交只有在对象存储写入成功、并且 `own.json` 仍然指向本代数之后才确认。
  output gate 由 workerd 原生实现，所以对外的响应一定在数据持久化之后才发出。
  同一对象的并发提交合并上传，热点对象每组提交只上传一次。
- **故障切换**：每个进程在对象存储里持有一个 20 秒的租约，每 3 秒续约。进程崩溃后，最长约 20 秒其他进程就能接管它的对象。
  正常退出（SIGTERM）时进程会立即注销，对象可以马上被接管。
- **路由**：进程之间各保持一条 Cap'n Proto 会话。请求发给非所有者时，workerd 会转给所有者，HTTP、WebSocket 和 JS RPC 都不变。
- **Alarm**：存在对象存储里，节点故障后由其他节点接管。
- **测试**：上游有确定性模拟测试（DST），会在任意存储调用前后注入崩溃。

### 7.2 Selvedge 怎么用

- **每个账户一个 cluster。** cluster 要求所有成员的配置相同，任何成员都可以把请求转给任何其他成员。
  这正好对应"每个账户一个进程"：一个账户的所有 workerd 进程组成一个 cluster，不同账户的 cluster 互不相通。
- **每个账户的存储前缀和凭据独立**：`s3://<bucket>/do/<account_id>/`，凭据只能访问这个前缀。
  这样即使租户代码突破 V8 拿到了进程内的凭据，也只能读写自己账户的 DO 数据。
- **每个账户的 cluster 密钥独立**（由各 agent 从共享密钥按账户派生，32 字节）。持有密钥就能访问进程里的所有服务，所以密钥绝不能共用。
- **不使用 cluster 的 deployment 功能**（`workerd deploy` 和对象存储里的 catalog）。配置由 agent 直接生成并传给进程，
  这样 Selvedge 能完全控制绑定，也不用给租户写入 catalog 的权限。
- **D1 是系统 DO**：agent 在每个账户的配置里加入 Selvedge 的 D1 实现类，D1 数据库因此和用户的 DO 一样分布和切换。

### 7.3 需要注意的限制

- **实验性功能**：接口和存储格式可能变化。Selvedge 固定 workerd 版本，升级 workerd 前跑完整的 DO 兼容和故障注入测试，
  发现问题优先给上游提 issue 和 PR。存储格式在 `v1/` 前缀下，上游已经为格式升级留了位置。
- **提交延迟取决于对象存储**：每组提交要等一次 PUT 和一次条件检查。集群内的 SeaweedFS 大约是毫秒级，公有云 S3 可能是几十毫秒。
  这会直接体现在 DO 写入后的响应延迟上。`ha` 档建议使用集群内或同区域的对象存储，实际数值在 S2 中测量。
- **对象存储本身必须高可用**：它成了 DO 的唯一权威存储。`ha` 档的对象存储要求跨 AZ 复制，可以是外部 S3，也可以是自建的 SeaweedFS 等。
- **cluster 端口不加密**：上游要求只在私有网络中使用。`ha` 档建议开启 CNI 层加密（如 Cilium 或 Calico 的 WireGuard）。
- **滚动部署时短暂存在两个版本**：新旧进程同时在 cluster 中时，DO 运行在哪个版本取决于当前所有者。
  旧进程注销后，下一个请求会让新进程接管。这和 Cloudflare 部署时 DO 重启、WebSocket 断开的行为大致一致，具体在 S1 中验证。
- **已有 `localDisk` 数据不会被导入**：上游拒绝在已有本地 DO 数据的目录上开启 cluster 模式。Selvedge 从第一天就用 cluster 模式，不存在这个问题。

## 8. 多租户隔离

workerd 的文档明确说过，它单独使用时不算加固过的安全沙箱。因此隔离分四层：

1. **进程**：每个账户一个 workerd 进程，使用独立的 UID、独立的 cgroup v2 子树（CPU 和内存上限）、seccomp 白名单，
   不给任何 capability，根文件系统只读。
2. **网络**：租户的 `globalOutbound` 是一个 workerd `network` 服务，只允许 `public` 地址，禁止 `private`、`local` 和 `unix`。
   这样租户代码连不到 cluster 端口和集群内的服务。Pod 的 NetworkPolicy 再加一层：拦截集群网段和云厂商元数据地址，
   cluster 端口只允许同账户的进程访问。
3. **凭据**：runner Pod 里没有数据库凭据和全局对象存储凭据。每个进程只有自己账户的 DO 存储凭据和 cluster 密钥。
4. **可选**：runner Pod 可以使用 gVisor `RuntimeClass` 做纵深防御（要求 S6 验证 workerd 在 gVisor 下的兼容性和性能）。

默认安全配置：以上四层中的前三层默认开启；NetworkPolicy 默认拒绝；管理 API 和 Dashboard 默认不暴露到集群外。

**爆炸半径**：一个 runner Pod 被攻破，影响的是放在这个 Pod 上的账户，不会扩散到其他 Pod，也拿不到数据库凭据和其他账户的 DO 数据。

## 9. 安装与升级

- 提供 Helm Chart，有 `minimal` 和 `ha` 两个 profile。
  - `minimal`：单节点，Postgres 和单节点对象存储都打包在内。每个账户的 cluster 只有一个成员，但仍然走 cluster 模式。
  - `ha`：Postgres 用 CNPG（同步复制、跨 AZ），对象存储用外部 S3 或跨 AZ 部署的自建存储。
- 两个 profile 走同一套代码，只是副本数和后端不同。**不为 `minimal` 写特殊代码路径。**
- 所有组件配置 `topologySpreadConstraints`（跨 AZ）、PDB 和反亲和。
- **升级兼容**：Postgres 迁移只追加，已发布的迁移不修改；支持跨一个小版本直接升级；
  CI 测试"装旧版 → 写数据 → 升级 → 校验数据"。升级 workerd 版本时，同一个测试还要覆盖 DO 数据。
- **workerd 版本**：固定官方 release 版本并校验 checksum，打进 `selvedge-runtime` 镜像。
  上游发布安全补丁后，在约定天数内跟进发版（天数写进 `SECURITY.md`）。

## 10. 官方 workerd 做不到的部分

| 能力 | 官方 workerd 的情况 | Selvedge 的做法 | 和 Cloudflare 的差异 |
| --- | --- | --- | --- |
| 单请求 CPU 时间上限 | standalone 不执行 | cgroup 限制整个进程的 CPU；进程卡死时，agent 的探活失败并重启进程 | 不返回 1102 错误；死循环会让同账户的进行中请求一起失败，其他账户不受影响 |
| isolate 内存上限 | standalone 不执行 | cgroup `memory.max`，OOM 后重启进程 | 同上 |
| DO 跨节点 | cluster 模式支持，但是实验性的 | 直接使用，固定版本，跟进上游 | 节点崩溃时切换最长约 20 秒 |
| 部署不中断 | 配置是静态的 | 滚动替换进程 | 部署时 DO 重启、WebSocket 断开，和 Cloudflare 一致 |

如果以后确实需要补丁（比如 CPU 上限），**优先给上游提 PR**，而不是自己维护 fork。
open-compute 的 W2 补丁就是做这件事的，可以和它的维护者一起推动上游合并。

## 11. 复用什么

| 来源 | 复用内容 | 说明 |
| --- | --- | --- |
| workerd cluster 模式 | DO 的分布、持久化、故障切换和路由 | 官方功能，不 fork |
| open-compute（Apache-2.0） | `/client/v4` 的 API 语义和 OpenAPI 子集、兼容性测试、和真实 Cloudflare 对比的 fixture、API 清单的生成方式 | 它的 `packages/runtime` 依赖 fork 补丁，不能直接用。复用代码时保留版权声明，并在 NOTICE 中注明来源 |
| Miniflare（workers-sdk 仓库） | KV、R2、D1、Queues 在官方 workerd 下的服务端协议实现，作为参考或直接移植 | 许可证以仓库为准，复用前核实 |
| Pingora | gateway | |

## 12. 分阶段

| 阶段 | 内容 | 完成标准 |
| --- | --- | --- |
| P0 | 本 RFC 定稿；完成 S1–S6；搭好仓库骨架、CI、`SECURITY.md`、`CONTRIBUTING.md`（DCO） | 每个 spike 有结论和数据 |
| P1 | gateway、runner、api、KV、R2、Static Assets、Secrets、日志；`minimal` 和 `ha` 两个 profile | `cf deploy` 能部署一个无状态应用；杀掉任意一个 Pod 或一个 AZ 的节点，请求不中断 |
| P2 | cluster 模式接入、Durable Objects、Alarms、WebSocket Hibernation、D1、Queues、Cron | 杀掉 DO 所在的 Pod：已确认的写入不丢，30 秒内恢复，故障注入中不出现两个写入者 |
| P3 | Workflows，以及 P2 中暴露的兼容性差距 | |

和初稿相比，DO 从 P3 提前到 P2。原来 DO 最难的部分（放置、复制、隔离旧所有者）现在由 workerd 负责，
Selvedge 只需要做配置生成、凭据和测试。

### 12.1 测试分层

| 层级 | 在哪跑 | 内容 |
| --- | --- | --- |
| 单元测试、clippy | 本地和每个 PR | Rust 单元测试 |
| 小规模集成测试 | 本地和每个 PR | 单进程 workerd 加 selvedge 组件，不起 Kubernetes |
| 兼容性测试 | GitHub Actions，每个 PR | Kind 单节点集群装 `minimal` profile，跑兼容性测试和 Weft 冒烟测试 |
| 大规模测试 | GitHub Actions，推送 `ci/*` 分支或手动触发 | Kind 多节点集群（用节点标签模拟多个 AZ）、Chaos Mesh 故障注入、升级测试、spike 实验 |

大规模测试不放在本地跑。每个 spike 或故障场景对应一个 workflow，结果作为 artifact 上传，方便在 PR 里引用数据。
如果 GitHub 托管 runner 的资源（CPU、内存、嵌套虚拟化）不够，再考虑自托管 runner。

## 13. 需要先验证的点（spike）

| 编号 | 问题 | 方法 |
| --- | --- | --- |
| S1 | cluster 模式在 Kubernetes 上的正确性：Pod 被杀、网络分区、滚动部署时，DO 有没有丢写入、有没有两个写入者；WebSocket Hibernation、Alarm、`WorkflowsEngine` 在 cluster 模式下是否正常 | 在 GitHub Actions 的 Kind 多节点集群上用 Chaos Mesh 注入故障，配合线性一致性检查；跑 open-compute 的 DO 兼容测试 |
| S2 | DO 写入延迟和吞吐，分别在 SeaweedFS、MinIO 和 AWS S3 上测 | 基准测试，记录 p50/p99 |
| S3 | 哪些自建对象存储支持 workerd 要求的条件写入：SeaweedFS、Garage、MinIO 社区版（同时核实许可现状）、Rook-Ceph RGW | 用 workerd 启动时的检查直接测试 |
| S4 | 官方 workerd 的 KV、R2、D1、Queue 绑定协议是否能完整实现 Cloudflare 的公开行为 | 移植 Miniflare 的实现，跑兼容性测试 |
| S5 | 每账户一个 workerd 进程的冷启动时间、基础内存、单节点能放多少账户；cluster 模式额外的开销（租约续约、对象存储请求数） | 基准测试 |
| S6 | 在 Pod 里由 agent 为每个账户做进程沙箱（UID、seccomp、cgroup 子树）是否可行；在 gVisor 下是否还能用 | 原型加 cgroup v2 delegation 的权限评估 |

已解决：初稿的"workerd `localDisk` 提交时是否 fsync"已不再关键。cluster 模式下本地磁盘只是缓存，持久性以对象存储确认为准。
（顺带确认：workerd 的 SQLite VFS 把 `xSync` 映射到 `datasync()` / `sync()`，见 `src/workerd/util/sqlite.c++`。）

## 14. 已定的问题

2026-11-06 按"选最可行的方案"定下：

1. **接受依赖实验性的 cluster 模式。** 风险缓解：固定 workerd 版本；每次升级前在 CI 跑 DO 兼容和故障注入测试；
   cluster 的存储格式有文档、实现是 Apache-2.0 的 Rust 代码，必要时可以写导出工具，不会被格式锁死。
2. **隔离粒度：每账户一个进程。** 同账户的 Service Binding 走原生调用，进程数随账户数增长而不是随 script 数增长。
3. **管理面鉴权：v1 只做 Bearer API Token**（兼容 `wrangler` 和官方 SDK）。OIDC 等 Dashboard 做出来再加。
4. **KV 后端：v1 用 Postgres，查询都在 `selvedge-store` 的 `kv_*` 函数里**，以后换后端只需替换这一层。

## 15. 实现进度（2026-11-06）

| 部分 | 状态 | 验证方式 |
| --- | --- | --- |
| api：`/client/v4` 子集（脚本、版本、部署、回滚、secret、KV、自定义域名、workers.dev 子域） | 完成 | `wrangler deploy / versions / rollback / delete / secret / kv` 端到端测试 |
| data：KV 绑定协议服务端 | 完成 | 官方 workerd 绑定调用的端到端测试 |
| agent：每账户一个官方 workerd 进程、按需启动、空闲回收、部署时滚动替换、SIGTERM 先摘流量再 drain；请求路径不等控制面，新版本通过变更推送（每秒轮询）在后台切换；访问 data 时建连失败自动重试 | 完成 | 端到端测试；滚动重启压测零失败 |
| gateway：按 hostname 路由、rendezvous 哈希、建连失败自动换 runner | 完成 | 端到端测试 |
| Durable Objects（cluster 模式）：fetch、RPC、SQLite、alarm、WebSocket hibernation | 完成 | 两 runner 测试：单一所有者、runner 崩溃后 20 秒内全部恢复、无丢写 |
| Helm chart（minimal / ha）、镜像（官方 workerd + sha512 校验）、NetworkPolicy | 完成 | `helm lint`；kind 三可用区测试在 GitHub Actions 上跑 |
| R2：绑定协议（含 multipart、条件请求、范围读取）、REST API、旧内容延迟回收 | 完成 | 端到端测试（绑定 + `wrangler r2`） |
| D1：官方 `d1-api` 绑定 + 系统 worker（每个数据库一个 SQLite DO，自动获得 cluster 模式的高可用）、REST API（create / execute / migrations） | 完成 | 绑定和 wrangler 的端到端测试；runner 崩溃后数据仍在 |
| Cron：`triggers.crons`、`L/W/#` 语法，data 副本用 SKIP LOCKED 认领到期触发（每次触发只执行一次，不需要选主），经系统 worker `selvedge-events` 投递 | 完成 | wrangler 部署 + 真实定时触发的端到端测试 |
| Queues：官方 Queue 生产者绑定；消息存 Postgres，data 副本按租约认领批次（至少一次投递），经 `selvedge-events` 以 `MessageBatch` 形状的对象投递；重试、死信队列 | 完成（不支持 `v8` 格式：解码需要实验性标志） | wrangler 部署 + 生产、消费、重试的端到端测试 |
| Static Assets：wrangler 的上传会话协议（只传账户里还没有的文件）、纯静态 Worker、`ASSETS` 绑定；由官方 workers-sdk 的 asset worker 和 router worker（未修改）在 workerd 里提供服务，`_headers`、`_redirects`、`run_worker_first` 在部署时解析；runner 本地磁盘缓存 | 完成 | 官方 workerd 渲染测试；wrangler 部署的端到端测试；两 runner 测试中 data 服务停掉后已缓存的资源照常返回 |
| `wrangler tail`：每个账户进程里有一个 tail worker，只在有人 tail 时把事件转给 agent；API 按 wrangler 的过滤条件筛选，经 PostgreSQL `LISTEN/NOTIFY` 送到持有 WebSocket 的副本 | 完成 | wrangler tail 的端到端测试（日志、异常、结束会话） |
| Workflows：官方 workers-sdk 的 Workflows 引擎（未修改，每个实例一个 DO，跟随 cluster 模式）、workerd 自带的 `workflows-api` 绑定模块；系统 worker `selvedge-workflows` 维护实例索引、对外提供 REST；不需要实验性标志 | 完成 | wrangler 部署和 `wrangler workflows` 的端到端测试；两 runner 测试中 runner 崩溃后等待事件的实例全部继续完成 |
| 每账户 UID：agent 以只带六个 capability 的 root 运行，每个账户的 workerd 用独立 UID、无 capability，目录 0700、绑定 socket 0600；Helm 默认开启，可选 `hostUsers: false`；Pod 使用 `RuntimeDefault` seccomp | 完成 | 以 root 在容器里跑的单节点端到端测试（进程 UID、socket 和目录权限）；kind 测试默认开启 |
| `request.cf` 由 gateway 设置，不再暴露 runner 本地 socket 的 PID/UID | 完成 | 端到端测试（含客户端伪造） |
| 每账户对象存储凭据：DO 存储经 agent 内的 S3 代理，workerd 只拿到按账户派生的密钥；代理校验 SigV4、只放行本账户前缀和 cluster 模式用到的操作，再用真实凭据重新签名 | 完成 | AWS SigV4 测试向量；两 runner 测试：DO 正常读写和故障切换，越权前缀、整桶列举、错误签名均被拒绝，workerd 环境里没有真实密钥 |
| R2 的 S3 兼容接口：data 服务上单独的监听端口，路径式寻址；SigV4（请求头和预签名 URL）、`aws-chunked` 请求体；对象、分段上传、列举、复制、条件读写；凭据同 Cloudflare（token id + token 的 SHA-256） | 完成 | 官方 AWS SDK 的 13 项端到端检查；S3 写入的对象可由 `wrangler r2` 读出 |
| 每账户 cgroup、组件间 mTLS | 未开始 | 见 `SECURITY.md` 的已知缺口 |

Spike 结论：

- **S3（条件写入）**：SeaweedFS 4.50 通过 workerd 的启动检查，已用于 CI 和 Helm 的 minimal profile。
  MinIO 在 Docker Hub 上的官方镜像已经拉不到，未测试。Garage、Rook-Ceph RGW 待测。
- **S1（部分）**：本地两 runner：`kill -9` 一个 runner 后，它持有的对象在约 20 秒（租约时长）后恢复，期间对这些对象的调用失败、
  不会挂起到超时；正常下线（SIGTERM）几乎无中断。Kubernetes 上的同类测试在 `test/k8s/e2e.sh`。
- **S5（部分）**：单账户进程冷启动在本地约 0.1–0.4 秒（2 核虚拟机，含渲染配置）。
- 实现中发现：workerd 子进程默认继承 agent 的环境变量（含内部 token），已改为清空环境、只传对象存储所需的变量。
