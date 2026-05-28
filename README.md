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
