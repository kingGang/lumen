# lumen-server

Lumen 云服务端（M5.1）：账户、设备登记、设置/历史同步，及后续远程控制中继。

技术栈：Rust + tokio + axum + tokio-postgres + deadpool + argon2 + JWT。
纯跨平台依赖——**本地测试编 Windows 版、生产编 Linux 版**（详见仓库
`docs/M5远程控制设计.md` §3.3）。

---

## 1. 本地开发：起 Postgres（docker）

本机 5432 端口情况特殊：**原生 PostgreSQL 18 服务占用 IPv4 `0.0.0.0:5432`**，
而 docker 的 `notebook-postgres` 只在 IPv6 `::` 上代理 5432，且其 pg_hba 对外部
来源要求 scram、对 `::1` 是 trust——宿主直连歧义大。**故为 Lumen 起一个独立、
隔离、端口无冲突的专用容器**：

```bash
docker run -d --name lumen-postgres \
  -p 5544:5432 \
  -e POSTGRES_USER=lumen_user \
  -e POSTGRES_PASSWORD=lumen_password \
  -e POSTGRES_DB=lumen \
  postgres:15-alpine
```

- 宿主 `127.0.0.1:5544` → 容器 5432（IPv4，无冲突）。
- 用户/密码/库：`lumen_user` / `lumen_password` / `lumen`。
- 表由服务端启动时自动建（`CREATE TABLE IF NOT EXISTS`，见 `src/db.rs`），无需迁移工具。

停止/重启/查看：

```bash
docker stop lumen-postgres   # 停
docker start lumen-postgres  # 起
docker exec -it lumen-postgres psql -U lumen_user -d lumen   # 进 psql
```

---

## 2. 运行服务端

```bash
cargo run -p lumen-server
# 或编译后直接跑：./target/debug/lumen-server.exe
```

默认监听 `127.0.0.1:8787`，默认连上面的 `lumen-postgres`。

### 环境变量（全部可选，带默认值）

| 变量 | 默认 | 说明 |
|---|---|---|
| `LUMEN_DATABASE_URL` | `postgres://lumen_user:lumen_password@127.0.0.1:5544/lumen?sslmode=disable` | Postgres 连接串（用 `127.0.0.1` 强制 IPv4，避开原生 PG18） |
| `LUMEN_BIND_ADDR` | `127.0.0.1:8787` | 监听地址 |
| `LUMEN_JWT_SECRET` | `dev-insecure-secret-change-me` | JWT 签名密钥（**生产务必覆盖**） |
| `LUMEN_TOKEN_TTL_SECS` | `604800`（7 天） | token 有效期 |
| `LUMEN_ONLINE_WINDOW_SECS` | `120` | 设备在线判定窗口（M5.1 近似，M5.2 换心跳） |
| `LUMEN_LOG` | `info` | 日志级别（tracing EnvFilter） |

客户端侧：`LUMEN_SERVER_URL`（默认 `http://127.0.0.1:8787`）覆盖服务端地址。

---

## 3. REST 端点（v1）

| 方法 | 路径 | 鉴权 | 说明 |
|---|---|---|---|
| GET | `/api/v1/health` | 否 | 存活 + 协议版本 |
| POST | `/api/v1/auth/register` | 否 | 注册（argon2 哈希密码） |
| POST | `/api/v1/auth/login` | 否 | 登录，登记本设备，返回 JWT |
| GET | `/api/v1/devices` | Bearer | 设备列表（在线/本机标记） |
| PATCH | `/api/v1/devices/{id}` | Bearer | 重命名设备 |
| DELETE | `/api/v1/devices/{id}` | Bearer | 删除设备 |
| GET/PUT | `/api/v1/sync/settings` | Bearer | 偏好同步（version 守卫 last-write-wins） |
| GET/POST | `/api/v1/sync/history` | Bearer | 命令历史同步（`text+ts` 去重） |
| GET (升级) | `/api/v1/ws` | Bearer（`Authorization` 头） | **M5.3** 远程控制 WebSocket 中继 |

类型定义见 `crates/lumen-protocol`（客户端/服务端共享）。

### 3.1 WebSocket 远程控制中继（M5.3 part1）

`GET /api/v1/ws` 升级为 WebSocket 长连接，承载终端远程的**控制面**。鉴权走
`Authorization: Bearer <jwt>` 头（同 REST，不走 query，避免反代日志泄漏）。消息
为 JSON **Text 帧**，类型见 `crates/lumen-protocol/src/remote.rs`（`RemoteC2S` /
`RemoteS2C`）。中继状态机在 `src/hub.rs`（纯内存：presence / 9 位配对 / 控制独占 /
会话生命周期 / 数据面盲转），传输层在 `src/ws.rs`。配对码由**服务端生成、仅下发
被控端展示**、控制端人工输入、服务端校验（attempts=5、TTL 120s、后台 GC）。
数据面 `Relay` 载荷为不透明 JSON，服务端原样转发不解析（part2/3 扩展零改服务端）。

---

## 4. 生产部署（Linux）

```bash
# 在 Linux 上只编 server（不触及 Windows-only 客户端 crate）
cargo build -p lumen-server --release
```

- 单二进制 + systemd；`LUMEN_*` 环境变量配置。
- TLS：前置 **Caddy** 反代终止 TLS（Let's Encrypt 自动续期），server 内网明文 HTTP。
- 数据库：生产 Postgres（同主机/内网），`LUMEN_DATABASE_URL` 指向之。
- `LUMEN_JWT_SECRET` 必须改为强随机值。

---

## 5. 注意

- M5.1 登录对**未知邮箱返回 `user_not_found`**，客户端据此自动注册（方便单用户）。
  这会泄露邮箱是否存在；M5.2 若需对外开放，应改为显式注册 UI + 统一错误。
- DB 连接当前用 `NoTls`（本地 docker 不强制 SSL）；生产若 server↔Postgres 需
  TLS，再接 tokio-postgres 的 rustls connector。
