# RFC 0002：Containers 支持

- 状态：**草案**，待讨论
- 日期：2026-11-09
- 依赖：[RFC 0001](0001-architecture.md) §7（Durable Objects 集群模式）、§8（多租户隔离）
- 依据：workerd `v1.20261109.1` 源码（`src/workerd/server/container-client.c++`、`docker-api.capnp`、`workerd.capnp`、`src/workerd/api/container.h`），wrangler 4.158.0

RFC 0001 把 Containers 列为 v1 的非目标。本文说明不修改 workerd 怎么支持它，结论是**可行，但相当于一个新子系统**，需要先验证几个关键点（§9）再决定做不做。

## 1. Cloudflare Containers 是什么

- 每个容器都挂在一个 Durable Object 上。DO 代码通过 `ctx.container` 控制它：`start()`、`getTcpPort(port).fetch()` / `connect()`、`monitor()`、`signal()`、`destroy()`、`exec()`、`setInactivityTimeout()`、出站拦截（`interceptOutboundHttp` 等）和快照。`@cloudflare/containers` 的 `Container` 类是封装在这些接口之上的一个库。
- 用 wrangler 部署：配置里写 `containers: [{ class_name, image, max_instances, instance_type }]` 和对应的 DO 绑定。`wrangler deploy` 依次做这些事：
  1. 用本地 Docker 构建镜像；
  2. 调用 `POST /accounts/:id/containers/registries/{domain}/credentials` 取得推送凭证；
  3. 推送到 `registry.cloudflare.com`，可以用环境变量 `CLOUDFLARE_CONTAINER_REGISTRY` 改成别的仓库；
  4. 通过 `/accounts/:id/containers` 下的 `GET /me`、`/applications` 和 `/applications/{id}/rollouts` 创建或更新"应用"，应用和 DO 命名空间关联。
- 语义：容器的磁盘是临时的，DO 换了所在的机器，容器就在新位置重新启动；实例规格（`instance_type`）决定 CPU、内存和磁盘大小。

## 2. workerd 已经提供了什么

workerd 自带一个容器后端，**通过 Docker Engine API 工作**：

- DO 命名空间的配置 `container = (imageName, images, privileges)` 会给该命名空间开启 `ctx.container`。Worker 的配置 `containerEngine = (localDocker = (socketPath, containerEgressInterceptorImage, sessionId))` 指定 Docker 守护进程的 socket。上游注释写明 `localDocker` 是"Only used for local development and testing purposes"。
- 它调用的 Docker API：
  - 镜像：`GET /images/{ref}/json`，`POST /images/create`（拉取）；
  - 容器：`POST /containers/create`，以及 `/containers/{id}/start`、`/stop`、`/kill`、`/wait`、`/json`、`/archive`、`/exec`；
  - exec：`/exec/{id}/start`、`/resize`、`/json`；
  - 快照：`POST /commit`，`/volumes` 的增删查；
  - 网络：`GET /networks/bridge`。
- **网络模型**。每个容器配一个 `proxy-everything` 边车（镜像来自 `containerEgressInterceptorImage`），容器用 `NetworkMode: container:<边车>` 和边车共享网络命名空间。
  - 入站：边车以 `PublishAllPorts` 把入口端口随机映射到宿主机，workerd 对 `宿主机:端口` 发 HTTP CONNECT，由边车转到容器端口。
  - 出站：边车带 `NET_ADMIN` 权限，用 iptables/TPROXY 截获容器的全部出站流量，再经 `host.docker.internal`（`host-gateway`）用 HTTP CONNECT 连回 workerd 在网桥网关地址上开的端口。workerd 按出站拦截规则把流量交给 Worker，或者放行到公网。`--tls-intercept` 时，workerd 会把自己生成的 CA 放进容器。
- **集群模式**：交接一个 DO 时，workerd 会先停掉它的容器（`stopTimeoutMs` 里专门留了时间），新的所属进程再按需启动。这和 Cloudflare"容器跟着 DO 走、磁盘是临时的"语义一致。
- **安全相关**：`privileges` 会原样传给 Docker 的 `CapAdd`/`Devices`/`SecurityOpt`，不做校验；`start()` 可以由 Worker 代码指定任意镜像引用。

## 3. 为什么不能直接把 Docker socket 交给 workerd

- 拿到 Docker socket 就等于拿到节点的 root：可以挂载宿主机目录，或者起特权容器。这违背 RFC 0001 §8 的原则：workerd 以账号自己的 UID 运行，不持有任何平台凭证。
- 容器和 runner 共用节点内核，没有沙箱隔离。不可信租户的任意镜像必须放进 gVisor 或 Kata 这类沙箱。
- 租户代码可以让 `start()` 拉取任意镜像，也可以在配置里设置 `privileges`。

所以容器后端必须放在 workerd 之外，由平台按白名单执行。

## 4. 方案：agent 实现 Docker API 子集，背后用 Kubernetes Pod

```
           runner Pod                                        容器 Pod（RuntimeClass: gvisor 或 kata）
 ┌──────────────────────────────────────┐            ┌──────────────────────────────────────┐
 │ workerd（账号 A，UID a）              │            │ proxy-everything 边车（NET_ADMIN）     │
 │   │ containerEngine.localDocker       │  入站转发   │   ▲ 共享网络命名空间                    │
 │   ▼ unix:<run>/A/docker.sock (0600, a) │──────────▶ │ 应用容器（账号 A 的镜像）               │
 │ agent：Docker API 垫片                 │            └──────────────────────────────────────┘
 │   ├─ 校验镜像、生成 Pod 规格             │                       │ 出站（HTTP CONNECT）
 │   ├─ 入站：127.0.0.1:N → Pod IP:边车端口 │◀──────────────────────┘
 │   └─ 出站中转：只接受本账号容器的连接     │
 └──────────────┬───────────────────────┘
                │ mTLS（新角色：containers）
                ▼
      selvedge-containers（新组件）：持有 Kubernetes 凭证，创建、删除和监视容器 Pod
```

### 4.1 组件划分

- **agent 中的 Docker API 垫片**：给每个账号开一个 Unix socket，权限 0600，属主是该账号的 UID，和现有的 binding socket 一样。渲染配置时写入 `containerEngine = (localDocker = (socketPath = <它>, containerEgressInterceptorImage = <锁定版本的边车镜像>, sessionId = <账号+代次>))`。垫片只实现 §4.2 中的接口，其他一律返回 501，所以 workerd 里对应的功能会明确失败，不会悄悄出错。
- **新组件 `selvedge-containers`**：唯一持有 Kubernetes API 凭证的组件。
  - 为什么不让 agent 直接调用 Kubernetes：runner 运行租户代码，一旦 workerd 沙箱被突破，进程能读到的 ServiceAccount token 就会变成集群级别的权限。
  - 通信方式：agent 通过 mTLS 内部接口调用它，新增 `containers` 角色，沿用 PR #9 的按角色校验。
  - 它是 RFC 0001 §4.5 中设想过的 controller 的第一个职责。
- **容器 Pod**：
  - 放在 release 所在的命名空间，以便用 ownerReference 指向发起它的 runner Pod：runner Pod 被删除时，Kubernetes 会自动回收它的容器 Pod（§6）。
  - 规格固定：两个容器（边车 + 应用），`runtimeClassName` 由 Helm 配置，默认 `gvisor`；不挂载 ServiceAccount token；`hostNetwork: false`；资源上下限按实例规格设定（§5.3）。

### 4.2 Docker API 映射

| workerd 调用 | 垫片的处理 |
| --- | --- |
| `GET /images/{ref}/json`、`POST /images/create` | 只接受 `<仓库>/<账号 id>/...` 下的镜像，否则返回 404。向仓库查询 digest，用于固定版本。拉取在 Pod 启动时由节点完成。 |
| `POST /containers/create`（边车） | 记下边车的参数（端口、DNS、命令行）。只认白名单字段，`CapAdd` 只允许边车的 `NET_ADMIN`。 |
| `POST /containers/create`（应用） | 记下镜像、环境变量、命令和入口端口。丢弃 `privileges` 带来的 `CapAdd`/`Devices`/`SecurityOpt`，Selvedge 也从不渲染 `privileges`。 |
| `POST /containers/{id}/start` | 第一次 start 时请 `selvedge-containers` 创建 Pod，等它进入 Running 状态。 |
| `GET /containers/{id}/json` | 把 Pod 状态翻译成 Docker 的字段。边车的 `NetworkSettings.Ports` 填 agent 为它在 runner Pod 回环地址上开的转发端口（§4.3）。 |
| `POST /containers/{id}/wait`、`stop`、`kill` | `wait` 翻译成 watch Pod；`stop` 是带宽限期删除；`kill` 只支持 SIGKILL（立即删除）和 SIGTERM，其他信号先返回 501（§9）。 |
| `DELETE /containers/{id}` | 删除 Pod。 |
| `GET /networks/bridge` | 返回 runner Pod 的 IP 作为"网关"。workerd 会在这个地址上开出站回连端口。 |
| `/exec/*`、`/archive` | 第二阶段：通过 `selvedge-containers` 代理 Kubernetes 的 exec 子资源。`archive` 用于 TLS 拦截时放入 CA。 |
| `/commit`、`/volumes/*`（快照） | 返回 501，暂不支持（§7）。 |

### 4.3 网络与隔离

- **入站**：workerd 连接 `127.0.0.1:N`，agent 把流量转到 `Pod IP:边车入口端口`。runner Pod 里所有账号共享同一个回环地址，所以 agent 收到连接后要通过 `/proc/net/tcp` 找到对端 socket 的 UID，确认它属于这个容器所在的账号；Unix socket 可以用 SO_PEERCRED，TCP 只能这样查。
- **出站**：边车回连 workerd 开在 runner Pod IP 上的端口。这条连接**没有任何认证**：在本地 Docker 网桥上这不是问题，放到集群网络里就是了。措施有两层：
  1. NetworkPolicy：容器 Pod 只能连到"自己所属账号的 runner Pod"的出站端口，以及 DNS；不能直接上公网。所有出站都必须经过 workerd，也就受到 workerd 现有的 `deny private/local` 规则限制。
  2. agent 在 runner Pod 里做一层中转：workerd 实际监听在回环地址上，外部连接先到 agent，agent 核对来源 IP 是否属于该账号的容器 Pod，再转给 workerd。
- **入向限制**：容器 Pod 只接受所属 runner Pod 的连接，彼此之间、跨账号之间都不通。
- **边车权限**：`NET_ADMIN` 只在 Pod 自己的网络命名空间里有效。gVisor 的网络栈对 iptables/TPROXY 的支持需要先验证（§9）；如果不行，就改用 Kata（真实的客户机内核），或者自己写一个不依赖 TPROXY 的边车，但它必须兼容 workerd 期望的 `proxy-everything` 协议，这会增加维护负担。

### 4.4 镜像仓库

- 运营方提供一个 OCI 仓库（Harbor、zot、distribution 均可），使用 Docker Registry v2 的 token 认证，**由 Selvedge API 签发 token**。
- `POST /containers/registries/{domain}/credentials` 返回一个短期 token，权限只覆盖 `repository:<账号 id>/*:push,pull`。用户用 `CLOUDFLARE_CONTAINER_REGISTRY=<仓库域名>` 运行 `wrangler deploy`，其余流程和在 Cloudflare 上相同。
- 节点拉取镜像：每个容器 Pod 带一个只读、只能访问该账号路径的 pull secret，由 `selvedge-containers` 生成。
- 垃圾回收：应用删除后，对应的镜像由仓库自己的 GC 清理，Selvedge 只负责删除 tag。

### 4.5 Containers API 与部署

- 在 `/accounts/:id/containers` 下实现 wrangler 实际调用的接口：`GET /me`（返回限额和一个虚拟的 location）、`/applications` 的增删改查、`POST /applications/{id}/rollouts`（立即生效并记录），以及 `/registries/{domain}/credentials`。部署前用 wrangler 的真实调用顺序核对。
- 应用存在 PostgreSQL 里：名称、关联的 DO 命名空间、镜像 digest、实例规格、`max_instances`。账号 bundle 带上这些信息，渲染器为对应的 DO 命名空间写入 `container = (imageName = "<仓库>/<账号>/<名>@sha256:...")`。
- 上传时的校验：引用了容器的 DO 类必须有对应的应用，否则拒绝上传。这沿用"不支持的就拒绝"的原则。

## 5. 资源、配额与计量

1. **实例数**：`max_instances` 和账号的总容器数，由 `selvedge-containers` 在创建 Pod 时检查。超出时 `start()` 返回错误，和 Cloudflare 一样。
2. **账号总资源**：上限由 Helm 设置，默认值保守。
3. **实例规格**：映射成 Pod 的 requests/limits 和 ephemeral-storage 上限。规格表由运营方配置，默认参照 Cloudflare 公开的 lite、basic、standard 等档位。
4. **监控指标**（沿用 PR #27 的做法，不带账号 id）：运行中的容器数、启动耗时（区分镜像已缓存和需要拉取）、按原因统计的停止次数、垫片收到的 501 次数。

## 6. 生命周期与高可用

- 容器跟随 DO 的所属进程：
  - **正常交接**：workerd 自己调用 stop/delete。
  - **进程崩溃或被杀**（包括 #24 的卡死检测）：垫片知道这个进程会话下的所有容器，进程退出后由 agent 删除。
  - **agent 或节点宕机**：runner Pod 被删除时，ownerReference 让 Kubernetes 自动回收它的容器 Pod。节点失联但 Pod 对象还在时，由 `selvedge-containers` 定期清理"所属 runner 已不健康"的容器。
- DO 在别的 runner 上重新被接手后，会在那里启动新的容器。原来容器里磁盘上的数据会丢失，这和 Cloudflare 的语义相同（磁盘是临时的），要在 compatibility.md 里写清楚。
- 冷启动：要经过 Pod 调度、可能的镜像拉取和 gVisor 启动，预计是秒级，比 Cloudflare 慢。可以考虑可选的预拉取（DaemonSet 按账号的热门镜像预热），放到后续阶段。

## 7. 不在范围内

GPU；持久磁盘；选择地域或位置；SSH 到实例（`/instances/{id}/ssh`）；不挂在 DO 上的独立部署（Cloudchamber 的 deployments）；容器快照（`snapshotContainer` / `snapshotDirectory`，先返回 501）；按实例规格计费。

## 8. 备选方案

| 方案 | 不选的原因 |
| --- | --- |
| 把节点的 Docker socket 交给 workerd | 等于给租户节点 root（§3） |
| 每个 runner Pod 里跑 DinD 或 rootless Podman | 需要特权或用户命名空间加 FUSE；容器和 runner 共享资源和节点，没有沙箱；扩容受单个 runner 限制 |
| 在 runner Pod 里直接调用 Kubernetes | runner 运行租户代码，不应该持有集群凭证（§4.1） |
| 自己实现 `ctx.container` | 它是 workerd 运行时内置的接口，只能 fork workerd，违反"不 fork"的约束 |
| 向上游提议一个"远程容器引擎"接口 | 长期看最干净，可以和本方案并行提出；但上游目前把容器后端定位为本地开发用，短期不能指望 |

## 9. 需要先验证的点（spike，在 kind 上通过 GitHub Actions 跑）

1. **gVisor 下的 `proxy-everything`**：在 `runsc` 中，iptables/TPROXY 能否截获全部出站流量？不行的话，Kata 在 GitHub 托管机器上能不能跑（需要嵌套虚拟化）？
2. **最小闭环**：workerd + 一个只实现 create/start/json/stop/delete 的垫片 + 一个 Pod，`getTcpPort(8080).fetch()` 能否打通，并量出冷启动时间。
3. **出站回连**：用 NetworkPolicy 加上 agent 中转之后，`interceptOutboundHttp` 和直连公网是否都正常，跨账号的连接是否被拒绝。
4. **workerd 升级时的接口变化**：用一个契约测试记录 workerd 实际调用的 Docker API 端点和字段。每天的 workerd 升级工作流（PR #28）跑这个测试，接口一变就能发现。
5. **wrangler 端到端**：用真实的 wrangler 推送到自建仓库，再创建应用，核对每一个被调用的接口和字段。

## 10. 分阶段与工作量估计

| 阶段 | 内容 | 粗估 |
| --- | --- | --- |
| P0 spike | §9 中的 1–3 项，结论写回本 RFC | 1–2 周 |
| P1 MVP | 仓库 token 签发、Containers API、垫片（生命周期、入站、出站）、`selvedge-containers`、清理、配额、监控指标、Helm、e2e 和 kind 测试 | 4–6 周 |
| P2 | `exec`、信号、TLS 拦截（`archive`）、`inspect`、预拉取 | 2–3 周 |
| P3 | 快照，或者正式宣布不支持 | 待定 |

决策点：P0 的结论决定是否进入 P1。如果 gVisor 跑不了 `proxy-everything`，而 Kata 又跑不了，就需要自己写边车，P1 的工作量和长期维护成本都会明显增加。到那时应当重新评估要不要做。
