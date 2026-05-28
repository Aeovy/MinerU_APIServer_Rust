FROM ubuntu:22.04

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --system mineru \
    && useradd --system --gid mineru --home-dir /app --shell /usr/sbin/nologin mineru \
    && mkdir -p /app/output \
    && chown -R mineru:mineru /app

COPY target/release/mineru-rust /usr/local/bin/mineru-rust

ENV MINERU_API_OUTPUT_ROOT=/app/output

WORKDIR /app

EXPOSE 34001

VOLUME ["/app/output"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl --fail --silent --show-error http://127.0.0.1:34001/health > /dev/null || exit 1

USER mineru

CMD ["mineru-rust", "--host", "0.0.0.0", "--port", "34001", "--allow-public-http-client"]
