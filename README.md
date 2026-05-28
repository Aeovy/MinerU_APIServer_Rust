# MinerU Rust API

MinerU-compatible Rust API service for `vlm-http-client`.

## Docker 运行

镜像不会把 `.env` 打包进去。请在运行容器时使用 Docker 的 `--env-file` 注入配置，这样同一个镜像可以复用在不同环境，也避免把 `MINERU_VL_API_KEY` 等敏感信息写进镜像层。

### 1. 准备配置

```bash
cp .env.example .env
```

按实际环境编辑 `.env`。如果 VLM 服务运行在宿主机：

- Docker Desktop: `MINERU_VL_SERVER=http://host.docker.internal:30000`
- Linux bridge 网络: 使用宿主机可被容器访问的 IP，例如 `http://172.17.0.1:30000`
- Linux host 网络: 可继续使用 `http://127.0.0.1:30000`，运行容器时加 `--network host`

容器默认工作目录是 `/app`，因此 `.env` 中的 `MINERU_API_OUTPUT_ROOT=./output` 会落到容器内 `/app/output`。

### 2. 构建二进制和镜像

```bash
cargo build --release --bin mineru-rust
docker build -t mineru-rust:latest .
```

### 3. 启动容器

普通 bridge 网络：

```bash
docker run --rm \
  --name mineru-rust \
  --env-file .env \
  -p 34001:34001 \
  -v "$PWD/output:/app/output" \
  mineru-rust:latest
```

Linux host 网络：

```bash
docker run --rm \
  --name mineru-rust \
  --env-file .env \
  --network host \
  -v "$PWD/output:/app/output" \
  mineru-rust:latest
```

服务启动后访问：

```bash
curl http://127.0.0.1:34001/health
```

### 配置优先级

Dockerfile 只提供容器内默认输出目录：

```text
MINERU_API_OUTPUT_ROOT=/app/output
```

运行时 `--env-file .env` 会覆盖镜像内默认环境变量；也可以继续用 `-e KEY=value` 覆盖 `.env` 中的单项配置。

程序启动时也会尝试读取当前工作目录下的 `.env`，但 Docker 推荐使用 `--env-file` 传入环境变量，而不是把 `.env` 复制进镜像。

## VLM 并发调优

服务端使用全局 VLM worker 队列调度所有解析任务的 VLM 请求：

- `MINERU_VLM_MAX_CONCURRENCY`: 全局同时发送到 VLM 服务的请求数。
- `MINERU_VLM_QUEUE_CAPACITY`: 等待 VLM worker 的有界队列容量。
- `MINERU_VLM_MAX_REQUESTS_PER_TASK`: 单个任务同时排队/发送的 VLM 请求上限，防止大文档独占队列。
- `MINERU_API_MAX_CONCURRENT_REQUESTS`: 同时进入解析执行态的文档数。排队文件很多但 VLM 不满载时，可以在内存允许的前提下提高到 8-16。

可通过 `/health` 观察 `active_vlm_requests`、`vlm_queue_depth`、`vlm_queue_capacity` 和 `available_vlm_permits` 判断瓶颈。如果 `active_vlm_requests` 长期低于 `MINERU_VLM_MAX_CONCURRENCY` 且队列为空，通常说明 PDF 渲染、图片裁剪或解析任务数不足；如果队列长期接近满，说明 VLM 服务本身是瓶颈。
