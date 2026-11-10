# RFC 0002：Containers 支持

- 状态：**已实现**（P1，2026-11-10）。沙箱实测见 §11，实现与本文初稿的差异见 §12
- 日期：2026-11-09
- 依赖：[RFC 0001](0001-architecture.md) §7（Durable Objects 集群模式）、§8（多租户隔离）
- 依据：workerd `v1.20261109.1` 源码（`src/workerd/server/container-client.c++`、`docker-api.capnp`、`workerd.capnp`、`src/workerd/api/container.h`），wrangler 4.158.0

RFC 0001 把 Containers 列为 v1 的非目标。本文说明不修改 workerd 怎么支持它，结论是**可行，但相当于一个新子系统**。§1–§11 是实现前的设计和实测，§12 记录最终实现与之不同的地方；运维文档见 [containers.md](../containers.md)。

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
           runner Pod                                        容器 Pod（RuntimeClass: kata）
 ┌──────────────────────────────────────┐            ┌──────────────────────────────────────┐
 │ workerd（账号 A，UID a）              │            │ proxy-everything 边车（NET_ADMIN）     │
 │   │ containerEngine.localDocker       │  入站转发   │   ▲ 共享网络命名空间                    │
 │   ▼ unix:<run>/A/docker.sock (0600, a) │──────────▶ │ 应用容器（账号 A 的镜像）               │
 │ agent：Docker API 垫片                 │            └──────────────────────────────────────┘
 │   ├─ 只转交类名、环境变量、命令          │                       │ 出站（HTTP CONNECT）
 │   ├─ 入站：127.0.0.1:N → Pod IP:边车端口 │◀──────────────────────┘
 │   └─ 出站中转：按来源地址找到所属容器     │
 └──────────────┬───────────────────────┘
                │ mTLS（新角色：containers）
                ▼
      selvedge containers（新组件）：持有 Kubernetes 凭证，从数据库查镜像和规格，
      自己生成 Pod 规格，创建、删除和清理容器 Pod
```

### 4.1 组件划分

- **agent 中的 Docker API 垫片**：给每个账号开一个 Unix socket，权限 0600，属主是该账号的 UID，和现有的 binding socket 一样。渲染配置时写入 `containerEngine = (localDocker = (socketPath = <它>, containerEgressInterceptorImage = <锁定版本的边车镜像>, sessionId = <账号+代次>))`。垫片只实现 §4.2 中的接口，其他一律返回 501，所以 workerd 里对应的功能会明确失败，不会悄悄出错。
- **新组件 `selvedge-containers`**：唯一持有 Kubernetes API 凭证的组件。
  - 为什么不让 agent 直接调用 Kubernetes：runner 运行租户代码，一旦 workerd 沙箱被突破，进程能读到的 ServiceAccount token 就会变成集群级别的权限。
  - 通信方式：agent 通过 mTLS 内部接口调用它，新增 `containers` 角色，沿用 PR #9 的按角色校验。
  - 它是 RFC 0001 §4.5 中设想过的 controller 的第一个职责。
- **校验放在控制组件，不放在 agent**：runner 可能被攻破，所以 agent 里的任何校验都不可信。agent 只转交 workerd 给它的东西：账号、DO 类、环境变量、命令、出站配置；镜像 digest、实例规格、`max_instances`、沙箱、Pod 形状和拉取凭证都由控制组件从 PostgreSQL 和自身配置决定，Worker 代码在 `start()` 里指定的镜像一律拒绝。控制组件还核对调用者：请求里的 runner 名必须是一个 runner Pod，且其 IP 等于 mTLS 连接的对端地址。
- **容器 Pod**：
  - 沙箱默认用 Kata（`runtimeClassName` 由 Helm 配置，默认 `kata-qemu`），所以节点需要 KVM，也就是裸金属机器或开启了嵌套虚拟化的云主机。沙箱是可选的：设成空值就用集群默认的 runc，容器和节点共享内核，只适合运行可信镜像（例如只有自己一个租户的部署），启动时会打警告。
  - 放在 release 所在的命名空间，以便用 ownerReference 指向发起它的 runner Pod：runner Pod 被删除时，Kubernetes 会自动回收它的容器 Pod（§6）。
  - 规格固定：边车是 native sidecar（`restartPolicy: Always` 的 init 容器），之后一个 setup 步骤写入出站配置并取回边车的 CA，最后是应用；`restartPolicy: Never`，应用退出即实例结束；不挂载 ServiceAccount token、不注入 Service 环境变量；`hostNetwork: false`；DNS 只用配置的公共解析器；资源上下限按实例规格设定（§5.3）。运营方的 `podTemplate`（nodeSelector、tolerations 等）先合并，控制组件决定的字段覆盖它。
- **不存状态**：账号、应用、会话和 runner 都写在 Pod 的 label 上，控制组件的每个回答都从 Pod 列表推出来。控制组件或 agent 重启都不会丢失对实例的跟踪，清理只需按 label 找孤儿。
- **单节点**：`selvedge all --containers-docker-socket` 用 Docker 作为后端，同一套控制逻辑，容器不进沙箱，用于开发和 e2e 测试。

### 4.2 Docker API 映射

| workerd 调用 | 垫片的处理 |
| --- | --- |
| `GET /volumes` | 返回空列表（不支持快照）。 |
| `GET /networks/bridge` | 网关报成 `127.0.0.1`：workerd 把每个容器的出站监听开在回环地址上，只有 agent 的出站中转能连到。 |
| `POST /containers/create`（边车） | 不创建任何东西，只在回环地址上为这个容器开一个入站端口。Pod 的容器在创建时就固定了，所以真正的边车随应用一起启动。 |
| `POST /containers/create`（应用） | 记下环境变量、命令和出站配置。镜像、`CapAdd`/`Devices`/`SecurityOpt`、资源限制都忽略，由控制组件决定。 |
| `POST /containers/{id}/start` | 请控制组件创建实例，等应用进入运行状态或失败（超时可配置，默认 300 秒）。失败原因写进 agent 日志，workerd 只告诉 Worker 启动失败。 |
| `GET /containers/{id}/json` | 把实例状态翻译成 Docker 的字段；边车的入口端口填 agent 开的回环端口。 |
| `POST /containers/{id}/wait`、`stop`、`kill` | `wait` 等实例结束并返回退出码；`stop` 是带 10 秒宽限期的删除；`kill`：SIGKILL 删除实例，其他信号通过 exec 在容器里执行 `kill -<n> 1` 送给 PID 1，和 Docker 一样只有该进程处理的信号才会送达。镜像里没有 `/bin/sh` 时，TERM、INT、QUIT 退回为停止实例。 |
| `DELETE /containers/{id}` | 删除实例。 |
| `PUT /containers/{id}/archive?path=/etc` | 接受并丢弃：workerd 想写入边车的 CA，实例启动时 setup 步骤会取回真正的 CA。 |
| `POST /containers/{id}/exec`、`/exec/{id}/start`、`/json`、`/resize` | 记下命令；start 时向控制组件开一条升级后的 mTLS 连接，控制组件用 Docker 的 exec 或 Pod 的 `exec` 子资源运行命令。stdin、stdout/stderr、终端尺寸和退出码在这条连接上用 Docker 多路复用流的帧格式传递。Kubernetes 上不支持指定用户。 |
| 其他 `archive` | 501。 |
| `/commit`、`/volumes/*`（快照）、`/images/*` | 501。 |

每个 501 都按操作计入 `selvedge_agent_container_unsupported_total`。

### 4.3 网络与隔离

- **入站**：workerd 连接 `127.0.0.1:N`，agent 把流量转到 `Pod IP:边车入口端口`。runner Pod 里所有账号共享同一个回环地址，所以 agent 收到连接后要通过 `/proc/net/tcp` 找到对端 socket 的 UID，确认它属于这个容器所在的账号；Unix socket 可以用 SO_PEERCRED，TCP 只能这样查。
- **出站（实现）**：边车把容器的全部出站流量发到 runner 的出站中转端口（默认 8791，`host.docker.internal` 指向 runner Pod IP）。中转按连接的来源地址找到是哪个实例，再转给 workerd 为该容器开在回环地址上的出站监听；来源地址不属于本 runner 的任何实例时直接断开，并计入 `selvedge_agent_container_egress_refused_total`。于是一个容器只能到达它自己那个 DO 的出站处理，由 workerd 按 `enableInternet`、出站拦截和私有地址规则决定去向。
- **出站**：边车回连 workerd 开在 runner Pod IP 上的端口。这条连接**没有任何认证**：在本地 Docker 网桥上这不是问题，放到集群网络里就是了。措施有两层：
  1. NetworkPolicy：容器 Pod 只能连到 runner Pod 的出站端口，以及配置的公共 DNS 解析器；不能直接上公网，也连不到集群里的其他服务。NetworkPolicy 无法按账号区分 runner，账号之间的隔离由第 2 点的中转按来源地址保证。所有出站都必须经过 workerd，也就受到 workerd 现有的 `deny private/local` 规则限制。
  2. agent 在 runner Pod 里做一层中转：workerd 实际监听在回环地址上，外部连接先到 agent，agent 核对来源 IP 是否属于该账号的容器 Pod，再转给 workerd。
- **入向限制**：容器 Pod 只接受所属 runner Pod 的连接，彼此之间、跨账号之间都不通。
- **边车权限**：`NET_ADMIN` 只在 Pod 自己的网络命名空间里有效。**实测（§11）表明 `proxy-everything` 不能在 gVisor 下运行**：它依赖 gVisor 不支持的策略路由和 TPROXY。可选的做法有两种：
  - **改用 Kata（已在 kind 上验证可行，采用这个方案）**：它是真实的客户机内核，同一个 Pod 的容器共享一台虚拟机，现成的 `proxy-everything` 可以照常运行；
  - 自己写一个基于 nat `REDIRECT` 的边车，配合 `runsc --net-raw`，并兼容 workerd 期望的 `proxy-everything` 协议。这会带来长期维护负担，只作为没有 KVM 节点时的备选。

### 4.4 镜像仓库

- 运营方提供一个 OCI 仓库（Harbor、zot、distribution 均可），使用 Docker Registry v2 的 token 认证，**由 Selvedge API 签发 token**。
- `POST /containers/registries/{domain}/credentials` 返回一个短期 token，权限只覆盖 `repository:<账号 id>/*:push,pull`。用户用 `CLOUDFLARE_CONTAINER_REGISTRY=<仓库域名>` 运行 `wrangler deploy`，其余流程和在 Cloudflare 上相同。
- 节点拉取镜像：每个容器 Pod 带一个只读、只能访问该账号路径的 pull secret，由控制组件生成并归 Pod 所有，Pod 删除时一起回收。凭证是 API 的 token 服务能验证的短期密码，不是仓库账号。
- Helm 可以捆绑一个 CNCF distribution 仓库（只信任 API 签发的 token），也可以接运营方自己的仓库；token 的签名密钥和证书由 chart 生成并保留。
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
- 冷启动：要经过 Pod 调度、可能的镜像拉取和 Kata 虚拟机启动，是秒级，比 Cloudflare 慢（实测见 §12）。可以考虑可选的预拉取（DaemonSet 按账号的热门镜像预热），放到后续阶段。

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

（下表是实测前的估计；§11 的结果会改变 P1 的范围，见该节的"下一步"。）

| 阶段 | 内容 | 粗估 |
| --- | --- | --- |
| P0 spike | §9 中的 1–3 项，结论写回本 RFC | 1–2 周 |
| P1 MVP（已完成，§12） | 仓库 token 签发、Containers API、垫片（生命周期、入站、出站）、`selvedge containers`、清理、配额、监控指标、Helm、e2e 和 kind 测试 | 4–6 周 |
| P2 | `exec`、信号、TLS 拦截（`archive`）、`inspect`、预拉取 | 2–3 周 |
| P3 | 快照，或者正式宣布不支持 | 待定 |

决策点：P0 的结论决定是否进入 P1。如果 gVisor 跑不了 `proxy-everything`，而 Kata 又跑不了，就需要自己写边车，P1 的工作量和长期维护成本都会明显增加。到那时应当重新评估要不要做。

## 11. 实测结果（2026-11-09）

在 GitHub Actions 的 Ubuntu 机器上，用 workerd `1.20261109.1` 自带的容器后端（`containerEngine.localDocker`）对接 Docker，分别以 runc、gVisor（`release-20261102.0`）以及开启 `--net-raw` 的 gVisor 作为 Docker 的默认运行时。代码在 `spike/containers` 分支的 `test/spike/containers/` 下，由 `containers spike` 工作流运行。

**runc（不带沙箱，作为基线）**：功能全部正常。

| 步骤 | 结果 | 耗时 |
| --- | --- | --- |
| 冷启动（镜像已在本地，含边车） | `getTcpPort(8080).fetch()` 成功 | 0.77–1.12 s |
| 热请求 | 成功 | 11 ms |
| 销毁后重新启动 | 成功 | 0.41 s |
| 默认出站（未开启 `enableInternet`） | DNS 解析失败，不能上公网 | |
| `enableInternet: true` 时出站 HTTP/HTTPS | 200 | 55–61 ms |
| 出站访问 `169.254.169.254` | 被 workerd 拒绝（400） | |
| `interceptOutboundHttp` | 容器发出的请求交给了 Worker | 12 ms |

**gVisor：`proxy-everything` 启动即退出**，报错 `RTNETLINK answers: Not supported`，开启 `--net-raw` 也一样。workerd 随后报 `Container ingress proxy is not running`。逐条探测透明代理需要的网络配置：

| 操作 | runc | gVisor | gVisor `--net-raw` |
| --- | --- | --- | --- |
| 回环地址别名（`ip addr add 127.0.0.3`） | 支持 | 支持 | 支持 |
| 策略路由（`ip rule fwmark`、`local` 路由表），TPROXY 依赖它 | 支持 | **不支持** | **不支持** |
| nftables 后端的 iptables | 支持 | 不支持 | 不支持 |
| 传统 iptables 的 nat `REDIRECT` | 支持 | nat 表不存在 | **支持，端到端验证有效** |
| 传统 iptables 的 owner 匹配 | 支持 | 不支持 | 支持 |
| `TPROXY` 目标 | 支持 | 不支持 | **不支持** |

**Docker API 契约**（这次实测的场景中 workerd 实际发出的调用，不包括 exec 和快照）：`GET /volumes?filters`、`GET /networks/bridge`、`POST /containers/create`（边车和应用各一次，HostConfig 中带资源限制、`CapAdd`、`Dns`、`ExtraHosts`、`NetworkMode`、`AutoRemove` 等字段）、`POST /containers/{id}/start`、`GET /containers/{id}/json`、`POST /containers/{id}/wait`（带和不带 `condition`）、`DELETE /containers/{id}?force`。`docker-recorder.py` 可以复用为 §9 第 4 点的契约测试。

**结论**

1. 在不带沙箱的 runc 下，workerd 的容器后端完全可用，冷启动约 1 秒。§4.2 的接口映射可以照着录到的调用清单来做。
2. **现成的 `proxy-everything` 不能在 gVisor 下运行，这是确定的。** 原因是 gVisor 的网络栈不支持策略路由和 TPROXY，与配置无关。
3. gVisor 加 `--net-raw` 后，传统 iptables 的 `REDIRECT` 可用，所以一个基于 `REDIRECT` 的边车理论上可行。代价有两个：
   - 要自己维护一个兼容 workerd 协议的边车，包括入站 CONNECT、出站 CONNECT 加主机名提示、DNS 和 TLS 拦截用的 CA；
   - 沙箱里会开放原始套接字，需要做安全评估。
4. Kata 无法在 Docker 下验证：Docker 中每个 Kata 容器各自是一台虚拟机，不支持 workerd 依赖的 `--net=container:`。到了 Kubernetes 里，同一个 Pod 的容器共享一台虚拟机，这个问题就不存在了，所以改在 kind 上验证（下一小节）。

### 11.1 Kata（kind + kata-deploy 4.3.1）

workerd 的容器布局放进一个 Kubernetes Pod：`proxy-everything` 边车用 workerd 传给它的同一组参数，带 `NET_ADMIN`，和应用容器共享 Pod 的网络命名空间；RuntimeClass 用 `kata-qemu`。

| 检查项 | 结果 |
| --- | --- |
| Pod 就绪耗时（镜像已在节点上） | 6 s |
| 容器内核和宿主机内核 | `6.18.56` 和 `7.0.0-1019-azure`，确实运行在独立的虚拟机里 |
| `proxy-everything` | 正常运行，没有重启，入口端口开始接受 CONNECT |
| 按 workerd 的方式发入站请求（向边车 `CONNECT 127.0.0.1:8080`，带 `X-Dst-Addr`） | 拿到应用的响应 |
| 策略路由、TPROXY、nat REDIRECT、owner 匹配 | 全部支持 |
| 出站（边车回连 workerd） | 未测：需要 workerd 在场，留给最小闭环（§9 第 2 点） |

**要让 Kata 在 kind 里跑起来，踩过这些坑**（只和 kind 有关，真实节点上不需要）：

- 默认的 `kata-qemu-runtime-rs` 要通过 systemd 的 D-Bus 设置 cgroup，而 kind 节点里没有 D-Bus socket，所以改用 Go 版运行时 `kata-qemu`；
- 宿主机要加载 `vhost_vsock` 和 `vhost_net`，并把 `/dev/kvm`、`/dev/vhost-vsock`、`/dev/vhost-net` 挂进节点；
- 客户机内存以文件形式放在 `/dev/shm`（virtio-fs 需要共享内存），kind 节点自己的 `/dev/shm` 只有 64 MiB，QEMU 会报 `kvm run failed Bad address`。要把宿主机的 `/dev/shm` 挂进去。

GitHub 托管的 Ubuntu 机器提供 `/dev/kvm`（嵌套虚拟化），所以这些检查可以在 CI 里持续运行。

**决定**：沙箱用 Kata，现成的 `proxy-everything` 可以直接用，不需要自己写边车，gVisor 不作为选项。代价是容器所在的节点必须支持 KVM，而且每个容器 Pod 都多一台轻量虚拟机的开销。这个要求要写进 Helm 文档和 compatibility.md。

**下一步**（P0 剩下的部分）

1. 最小闭环（§9 第 2 点）：垫片只实现 create/start/json/wait/delete，背后创建 Kata Pod，让 workerd 的 `getTcpPort().fetch()`、出站和 `interceptOutboundHttp` 都打通，量出冷启动时间（包括镜像拉取和虚拟机启动）。
2. 出站回连的隔离（§9 第 3 点）：NetworkPolicy 加 agent 中转，验证跨账号的连接会被拒绝。
3. 把 `docker-recorder.py` 做成契约测试，接进每天的 workerd 升级工作流。

## 12. 实现（2026-11-10）

P1 已经实现，代码在 `crates/selvedge-server/src/{containers,agent/dockershim.rs,api/containers.rs,registry.rs}`。和 §4 的初稿相比有这些变化，都是实测或评审后做的调整：

1. **校验在控制组件里**（§4.1）。初稿让 agent 校验镜像、生成 Pod 规格；但 runner 本来就被假定可能被攻破，在它里面做的校验不能当真。现在 agent 只转交 workerd 给的类名、环境变量、命令和出站配置，其余全部由控制组件从数据库和自身配置决定。`start()` 指定的其他镜像一律拒绝。
2. **控制组件不存状态**。实例的账号、应用、会话、runner 都在 Pod 的 label 上，回答一律从 Pod 列表推出；控制组件可以多副本运行，配额检查在 PostgreSQL 按账号的 advisory lock 下进行。
3. **沙箱可选**。`runtimeClassName` 为空时用集群默认运行时，只适合可信镜像，启动时打警告。单节点的 `selvedge all` 用 Docker 后端，同一套控制逻辑。
4. **边车的形态**。Pod 的容器在创建时就固定了，而 workerd 先建边车、后建应用，所以垫片对 workerd 扮演边车（只开一个回环入站端口），真正的 `proxy-everything` 作为 native sidecar 和应用一起启动，中间的 setup 步骤写入出站配置并取回 CA。workerd 的 `archive` 写 CA 因此被接受并丢弃。
5. **出站中转按来源地址识别实例**。`GET /networks/bridge` 把网关报成 `127.0.0.1`，workerd 的出站监听只开在回环地址上；边车连 runner 的 8791 端口，中转按来源 IP 找到实例，只交给该实例自己的出站监听。NetworkPolicy 只能做到"只连 runner"，跨账号隔离靠这一层。
6. **拉取凭证**。控制组件为每个实例生成只读、只覆盖该账号仓库的短期凭证，放进归 Pod 所有的 pull secret。仓库可以用 chart 捆绑的 CNCF distribution，也可以接运营方自己的。
7. **契约测试**（§9 第 4 点）。没有单独做录制回放：`test/e2e/containers.sh` 用真实的 wrangler 和 workerd 跑完整流程，每天的 workerd 升级工作流会跑它；垫片不认识的调用一律 501 并计入 `selvedge_agent_container_unsupported_total`，workerd 换了用法会直接表现为测试失败。
8. **删除镜像**：删除应用时，API 给自己签一个只覆盖该仓库的 `delete` token，删掉仓库里所有带 tag 的 manifest（另一个应用还在用同一个仓库时不删）；层文件由仓库的 GC 回收。
9. **`exec()`**（初稿的 P2）：经控制组件代理，runner 依然碰不到实例的运行时。Kubernetes 用 `v5.channel.k8s.io`，可以关闭 stdin；`user` 选项只有 Docker 后端支持。
10. **信号**：有了 exec 之后，SIGKILL 以外的信号都在容器里送给 PID 1（容器内不能给 init 发 SIGKILL，所以它仍是删除实例）。
11. **没有做的**：快照返回 501；没有镜像预拉取。

**测试**

- 单节点（CI `e2e`）：wrangler 构建、推送、部署，token 服务的权限范围，启动、入站、环境变量、CA，出站（默认不通、HTTP 和 HTTPS 拦截、`enableInternet`、拒绝链路本地地址），`max_instances` 和拒绝其他镜像，`exec()`（输出、退出码、stdin、环境变量和工作目录、kill、终端），退出码，销毁和重启，换镜像后的 rollout，删除应用及其镜像，监控指标。
- Kubernetes（CI `kind-containers`，kind 加 kata-deploy 4.3.1，GitHub 托管机器的嵌套虚拟化）：节点用实例自己的凭证从捆绑仓库拉取镜像；实例跑在 `kata-qemu` 里、有自己的内核；不挂 token、归 runner Pod 所有；入站、HTTP 和 HTTPS 出站拦截、`enableInternet`，workerd 拒绝集群地址；`exec()`；NetworkPolicy；runner Pod 删除后实例随之回收，DO 在新 runner 上重新启动容器；删除应用后实例、pull secret 和仓库里的镜像都被清理。

| 冷启动（kind + Kata，嵌套虚拟化，四次运行） | 耗时 |
| --- | --- |
| 首次，需要拉取镜像（18 MB）并启动虚拟机 | 10.1–12.0 s |
| 镜像已在节点上 | 7.7–9.0 s |
| runner 被删除后，DO 在新 runner 上重新启动容器 | 7.2–7.9 s |
| `wrangler deploy`（构建并推送） | 4.4–6.1 s |

比 §11.1 量到的 6 秒 Pod 就绪多出的部分，是 native sidecar 和 setup 步骤依次启动、以及 workerd 轮询端口的时间。直接用 KVM 的节点应当更快。
