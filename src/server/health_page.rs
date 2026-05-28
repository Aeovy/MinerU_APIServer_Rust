use axum::response::Html;

/// Render the embedded health dashboard page.
///
/// The page is intentionally self-contained so production Docker images can serve it without a
/// frontend build step, CDN access, or extra static-file configuration.
pub(crate) async fn health_page() -> Html<&'static str> {
    Html(HEALTH_PAGE_HTML)
}

const HEALTH_PAGE_HTML: &str = r##"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>MinerU Health Dashboard</title>
  <style>
    :root {
      color-scheme: light;
      --bg: #f6f7f9;
      --surface: #ffffff;
      --surface-muted: #eef2f5;
      --text: #17202a;
      --muted: #66727f;
      --border: #d8dee6;
      --green: #168a52;
      --blue: #2563eb;
      --cyan: #0891b2;
      --amber: #b7791f;
      --red: #c2410c;
      --ink: #26313d;
      font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI",
        sans-serif;
    }

    * {
      box-sizing: border-box;
    }

    body {
      min-width: 320px;
      margin: 0;
      background: var(--bg);
      color: var(--text);
    }

    .shell {
      width: min(1420px, 100%);
      margin: 0 auto;
      padding: 24px;
    }

    .topbar {
      display: flex;
      align-items: flex-start;
      justify-content: space-between;
      gap: 16px;
      margin-bottom: 18px;
    }

    h1,
    h2,
    p {
      margin: 0;
    }

    h1 {
      font-size: 28px;
      line-height: 1.15;
      font-weight: 720;
      letter-spacing: 0;
    }

    .subtitle {
      margin-top: 6px;
      color: var(--muted);
      font-size: 14px;
      line-height: 1.5;
    }

    .status-strip {
      display: flex;
      flex-wrap: wrap;
      justify-content: flex-end;
      gap: 8px;
    }

    .pill {
      display: inline-flex;
      align-items: center;
      min-height: 32px;
      padding: 6px 10px;
      border: 1px solid var(--border);
      border-radius: 999px;
      background: var(--surface);
      color: var(--ink);
      font-size: 13px;
      white-space: nowrap;
    }

    .pill strong {
      margin-left: 6px;
      font-weight: 700;
    }

    .status-dot {
      width: 8px;
      height: 8px;
      margin-right: 7px;
      border-radius: 999px;
      background: var(--amber);
    }

    .status-dot.ok {
      background: var(--green);
      box-shadow: 0 0 0 4px rgba(22, 138, 82, 0.12);
    }

    .status-dot.bad {
      background: var(--red);
      box-shadow: 0 0 0 4px rgba(194, 65, 12, 0.12);
    }

    .grid {
      display: grid;
      gap: 14px;
    }

    .grid.summary {
      grid-template-columns: repeat(4, minmax(0, 1fr));
      margin-bottom: 14px;
    }

    .grid.panels {
      grid-template-columns: 1.15fr 1fr;
      align-items: stretch;
    }

    .panel,
    .metric-card {
      border: 1px solid var(--border);
      border-radius: 8px;
      background: var(--surface);
      box-shadow: 0 1px 2px rgba(15, 23, 42, 0.04);
    }

    .metric-card {
      min-height: 118px;
      padding: 16px;
      display: grid;
      grid-template-rows: auto 1fr auto;
      gap: 8px;
    }

    .metric-label,
    .section-label {
      color: var(--muted);
      font-size: 12px;
      line-height: 1.25;
      text-transform: uppercase;
      letter-spacing: 0;
      font-weight: 700;
    }

    .metric-value {
      display: flex;
      align-items: center;
      gap: 8px;
      font-size: 34px;
      line-height: 1;
      font-weight: 760;
      font-variant-numeric: tabular-nums;
    }

    .metric-note {
      min-height: 18px;
      color: var(--muted);
      font-size: 13px;
      line-height: 1.35;
    }

    .panel {
      min-width: 0;
      padding: 18px;
    }

    .panel-header {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 12px;
      margin-bottom: 16px;
    }

    .panel-title {
      font-size: 16px;
      line-height: 1.35;
      font-weight: 740;
    }

    .bar-list {
      display: grid;
      gap: 14px;
    }

    .bar-row {
      display: grid;
      gap: 7px;
    }

    .bar-line {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 12px;
      color: var(--ink);
      font-size: 13px;
      line-height: 1.35;
    }

    .bar-value {
      color: var(--muted);
      font-variant-numeric: tabular-nums;
      white-space: nowrap;
    }

    .bar-track {
      width: 100%;
      height: 10px;
      overflow: hidden;
      border-radius: 999px;
      background: var(--surface-muted);
    }

    .bar-fill {
      width: 0%;
      height: 100%;
      border-radius: inherit;
      background: var(--blue);
      transition: width 180ms ease;
    }

    .bar-fill.green {
      background: var(--green);
    }

    .bar-fill.cyan {
      background: var(--cyan);
    }

    .bar-fill.amber {
      background: var(--amber);
    }

    .bar-fill.red {
      background: var(--red);
    }

    .stack {
      display: flex;
      width: 100%;
      height: 18px;
      overflow: hidden;
      border-radius: 999px;
      background: var(--surface-muted);
    }

    .stack span {
      display: block;
      min-width: 0;
      transition: width 180ms ease;
    }

    .legend {
      display: flex;
      flex-wrap: wrap;
      gap: 10px 14px;
      margin-top: 10px;
      color: var(--muted);
      font-size: 12px;
    }

    .legend-item {
      display: inline-flex;
      align-items: center;
      gap: 6px;
      white-space: nowrap;
    }

    .swatch {
      width: 10px;
      height: 10px;
      border-radius: 2px;
      background: var(--blue);
    }

    .chart-grid {
      display: grid;
      grid-template-columns: repeat(2, minmax(0, 1fr));
      gap: 14px;
    }

    .chart-box {
      min-width: 0;
      padding: 12px;
      border: 1px solid var(--border);
      border-radius: 8px;
      background: #fbfcfd;
    }

    .chart-top {
      display: flex;
      align-items: baseline;
      justify-content: space-between;
      gap: 10px;
      margin-bottom: 8px;
    }

    .chart-label {
      color: var(--muted);
      font-size: 12px;
      font-weight: 700;
      line-height: 1.3;
    }

    .chart-value {
      color: var(--ink);
      font-size: 13px;
      font-weight: 700;
      font-variant-numeric: tabular-nums;
      white-space: nowrap;
    }

    canvas {
      display: block;
      width: 100%;
      height: 72px;
    }

    .detail-grid {
      display: grid;
      grid-template-columns: repeat(3, minmax(0, 1fr));
      gap: 10px;
    }

    .detail {
      min-width: 0;
      padding: 12px;
      border: 1px solid var(--border);
      border-radius: 8px;
      background: #fbfcfd;
    }

    .detail-name {
      margin-bottom: 8px;
      color: var(--muted);
      font-size: 12px;
      line-height: 1.3;
    }

    .detail-value {
      color: var(--ink);
      font-size: 18px;
      font-weight: 740;
      font-variant-numeric: tabular-nums;
      overflow-wrap: anywhere;
    }

    .error {
      display: none;
      margin-bottom: 14px;
      padding: 12px 14px;
      border: 1px solid rgba(194, 65, 12, 0.35);
      border-radius: 8px;
      background: #fff7ed;
      color: #9a3412;
      font-size: 14px;
      line-height: 1.45;
    }

    .error.visible {
      display: block;
    }

    @media (max-width: 980px) {
      .grid.summary,
      .grid.panels,
      .chart-grid {
        grid-template-columns: 1fr;
      }

      .detail-grid {
        grid-template-columns: repeat(2, minmax(0, 1fr));
      }
    }

    @media (max-width: 640px) {
      .shell {
        padding: 16px;
      }

      .topbar {
        display: grid;
      }

      .status-strip {
        justify-content: flex-start;
      }

      h1 {
        font-size: 24px;
      }

      .metric-value {
        font-size: 30px;
      }

      .detail-grid {
        grid-template-columns: 1fr;
      }
    }
  </style>
</head>
<body>
  <main class="shell">
    <header class="topbar">
      <div>
        <h1>MinerU Health Dashboard</h1>
        <p class="subtitle">实时轮询 /health，展示任务队列、VLM 调度、内存与运行配置。</p>
      </div>
      <div class="status-strip">
        <span class="pill"><span id="statusDot" class="status-dot"></span>Status <strong id="statusText">Loading</strong></span>
        <span class="pill">Version <strong id="versionText">-</strong></span>
        <span class="pill">Updated <strong id="updatedText">-</strong></span>
      </div>
    </header>

    <section id="errorBox" class="error"></section>

    <section class="grid summary" aria-label="Task summary">
      <article class="metric-card">
        <div class="metric-label">Queued</div>
        <div id="queuedTasks" class="metric-value">0</div>
        <div class="metric-note">等待处理的任务</div>
      </article>
      <article class="metric-card">
        <div class="metric-label">Processing</div>
        <div id="processingTasks" class="metric-value">0</div>
        <div class="metric-note">正在解析的任务</div>
      </article>
      <article class="metric-card">
        <div class="metric-label">Completed</div>
        <div id="completedTasks" class="metric-value">0</div>
        <div class="metric-note">已完成任务</div>
      </article>
      <article class="metric-card">
        <div class="metric-label">Failed</div>
        <div id="failedTasks" class="metric-value">0</div>
        <div class="metric-note">失败任务</div>
      </article>
    </section>

    <section class="grid panels" aria-label="Capacity and VLM panels">
      <article class="panel">
        <div class="panel-header">
          <div>
            <div class="section-label">Capacity</div>
            <h2 class="panel-title">任务与入队容量</h2>
          </div>
          <span class="pill">Protocol <strong id="protocolText">-</strong></span>
        </div>
        <div class="bar-list">
          <div class="bar-row">
            <div class="bar-line"><span>Admission in flight</span><span id="admissionValue" class="bar-value">0 / 0</span></div>
            <div class="bar-track"><div id="admissionBar" class="bar-fill blue"></div></div>
          </div>
          <div class="bar-row">
            <div class="bar-line"><span>Parser workers busy</span><span id="parserValue" class="bar-value">0 / 0</span></div>
            <div class="bar-track"><div id="parserBar" class="bar-fill green"></div></div>
          </div>
          <div class="bar-row">
            <div class="bar-line"><span>Task distribution</span><span id="taskTotalValue" class="bar-value">0 total</span></div>
            <div class="stack" aria-hidden="true">
              <span id="queuedStack" style="background: var(--amber); width: 0%"></span>
              <span id="processingStack" style="background: var(--blue); width: 0%"></span>
              <span id="completedStack" style="background: var(--green); width: 0%"></span>
              <span id="failedStack" style="background: var(--red); width: 0%"></span>
            </div>
            <div class="legend">
              <span class="legend-item"><span class="swatch" style="background: var(--amber)"></span>Queued</span>
              <span class="legend-item"><span class="swatch" style="background: var(--blue)"></span>Processing</span>
              <span class="legend-item"><span class="swatch" style="background: var(--green)"></span>Completed</span>
              <span class="legend-item"><span class="swatch" style="background: var(--red)"></span>Failed</span>
            </div>
          </div>
        </div>
      </article>

      <article class="panel">
        <div class="panel-header">
          <div>
            <div class="section-label">VLM Scheduler</div>
            <h2 class="panel-title">全局 VLM 利用率</h2>
          </div>
          <span class="pill">Per task <strong id="perTaskVlmText">-</strong></span>
        </div>
        <div class="bar-list">
          <div class="bar-row">
            <div class="bar-line"><span>Active VLM requests</span><span id="vlmActiveValue" class="bar-value">0 / 0</span></div>
            <div class="bar-track"><div id="vlmActiveBar" class="bar-fill cyan"></div></div>
          </div>
          <div class="bar-row">
            <div class="bar-line"><span>VLM queue depth</span><span id="vlmQueueValue" class="bar-value">0 / 0</span></div>
            <div class="bar-track"><div id="vlmQueueBar" class="bar-fill amber"></div></div>
          </div>
          <div class="bar-row">
            <div class="bar-line"><span>Available VLM permits</span><span id="vlmPermitValue" class="bar-value">0</span></div>
            <div class="bar-track"><div id="vlmPermitBar" class="bar-fill green"></div></div>
          </div>
        </div>
      </article>
    </section>

    <section class="grid panels" style="margin-top: 14px" aria-label="Trend and details">
      <article class="panel">
        <div class="panel-header">
          <div>
            <div class="section-label">Recent Trend</div>
            <h2 class="panel-title">最近 60 次采样</h2>
          </div>
          <span class="pill">Refresh <strong>2s</strong></span>
        </div>
        <div class="chart-grid">
          <div class="chart-box">
            <div class="chart-top"><span class="chart-label">Active VLM</span><span id="activeVlmChartValue" class="chart-value">0</span></div>
            <canvas id="activeVlmChart" width="360" height="96"></canvas>
          </div>
          <div class="chart-box">
            <div class="chart-top"><span class="chart-label">VLM Queue</span><span id="queueChartValue" class="chart-value">0</span></div>
            <canvas id="queueChart" width="360" height="96"></canvas>
          </div>
          <div class="chart-box">
            <div class="chart-top"><span class="chart-label">Processing Tasks</span><span id="processingChartValue" class="chart-value">0</span></div>
            <canvas id="processingChart" width="360" height="96"></canvas>
          </div>
          <div class="chart-box">
            <div class="chart-top"><span class="chart-label">Resident Memory</span><span id="memoryChartValue" class="chart-value">-</span></div>
            <canvas id="memoryChart" width="360" height="96"></canvas>
          </div>
        </div>
      </article>

      <article class="panel">
        <div class="panel-header">
          <div>
            <div class="section-label">Runtime</div>
            <h2 class="panel-title">配置与内存</h2>
          </div>
        </div>
        <div class="detail-grid">
          <div class="detail"><div class="detail-name">Processing window</div><div id="windowSize" class="detail-value">-</div></div>
          <div class="detail"><div class="detail-name">Max upload</div><div id="maxUpload" class="detail-value">-</div></div>
          <div class="detail"><div class="detail-name">Task retention</div><div id="retention" class="detail-value">-</div></div>
          <div class="detail"><div class="detail-name">Cleanup interval</div><div id="cleanup" class="detail-value">-</div></div>
          <div class="detail"><div class="detail-name">Allocated memory</div><div id="allocatedMemory" class="detail-value">-</div></div>
          <div class="detail"><div class="detail-name">Resident memory</div><div id="residentMemory" class="detail-value">-</div></div>
          <div class="detail"><div class="detail-name">Retained memory</div><div id="retainedMemory" class="detail-value">-</div></div>
          <div class="detail"><div class="detail-name">VLM queue capacity</div><div id="vlmCapacity" class="detail-value">-</div></div>
          <div class="detail"><div class="detail-name">Admission permits</div><div id="admissionPermits" class="detail-value">-</div></div>
        </div>
      </article>
    </section>
  </main>

  <script>
    const HISTORY_LIMIT = 60;
    const REFRESH_MS = 2000;
    const history = {
      activeVlm: [],
      queueDepth: [],
      processing: [],
      residentMemory: []
    };

    const elements = Object.fromEntries(
      Array.from(document.querySelectorAll('[id]')).map((node) => [node.id, node])
    );

    function numberValue(value) {
      return Number.isFinite(Number(value)) ? Number(value) : 0;
    }

    function percent(value, max) {
      const safeMax = Math.max(numberValue(max), 1);
      return Math.max(0, Math.min(100, (numberValue(value) / safeMax) * 100));
    }

    function setText(id, value) {
      elements[id].textContent = value;
    }

    function setBar(id, value, max) {
      elements[id].style.width = `${percent(value, max)}%`;
    }

    function pushHistory(key, value) {
      history[key].push(numberValue(value));
      if (history[key].length > HISTORY_LIMIT) {
        history[key].shift();
      }
    }

    function formatBytes(bytes) {
      if (bytes === null || bytes === undefined) {
        return 'Unavailable';
      }
      const units = ['B', 'KB', 'MB', 'GB', 'TB'];
      let value = numberValue(bytes);
      let unitIndex = 0;
      while (value >= 1024 && unitIndex < units.length - 1) {
        value /= 1024;
        unitIndex += 1;
      }
      return `${value.toFixed(value >= 10 || unitIndex === 0 ? 0 : 1)} ${units[unitIndex]}`;
    }

    function formatSeconds(seconds) {
      const value = numberValue(seconds);
      if (value < 60) {
        return `${value}s`;
      }
      if (value < 3600) {
        return `${Math.round(value / 60)}m`;
      }
      return `${(value / 3600).toFixed(1)}h`;
    }

    // Draw a compact line chart without external charting dependencies.
    function drawChart(canvasId, values, color) {
      const canvas = elements[canvasId];
      const ratio = window.devicePixelRatio || 1;
      const width = canvas.clientWidth;
      const height = canvas.clientHeight;
      canvas.width = Math.max(1, Math.floor(width * ratio));
      canvas.height = Math.max(1, Math.floor(height * ratio));

      const ctx = canvas.getContext('2d');
      ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
      ctx.clearRect(0, 0, width, height);
      ctx.strokeStyle = '#d8dee6';
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.moveTo(0, height - 12);
      ctx.lineTo(width, height - 12);
      ctx.stroke();

      if (values.length === 0) {
        return;
      }

      const max = Math.max(...values, 1);
      const step = values.length > 1 ? width / (values.length - 1) : width;
      ctx.strokeStyle = color;
      ctx.lineWidth = 2;
      ctx.beginPath();
      values.forEach((rawValue, index) => {
        const x = index * step;
        const y = 8 + (1 - rawValue / max) * (height - 20);
        if (index === 0) {
          ctx.moveTo(x, y);
        } else {
          ctx.lineTo(x, y);
        }
      });
      ctx.stroke();
    }

    function setTaskDistribution(data) {
      const values = [
        ['queuedStack', data.queued_tasks],
        ['processingStack', data.processing_tasks],
        ['completedStack', data.completed_tasks],
        ['failedStack', data.failed_tasks]
      ];
      const total = values.reduce((sum, [, value]) => sum + numberValue(value), 0);
      setText('taskTotalValue', `${total} total`);
      values.forEach(([id, value]) => {
        elements[id].style.width = total === 0 ? '0%' : `${(numberValue(value) / total) * 100}%`;
      });
    }

    function renderHealth(data) {
      const inFlight = numberValue(data.max_in_flight_tasks) - numberValue(data.available_admission_permits);
      const parserBusy = numberValue(data.processing_tasks);
      const residentMemory = data.allocator_resident_bytes;

      elements.statusDot.className = `status-dot ${data.status === 'healthy' ? 'ok' : 'bad'}`;
      setText('statusText', data.status || 'unknown');
      setText('versionText', data.version || '-');
      setText('protocolText', data.protocol_version ?? '-');
      setText('updatedText', new Date().toLocaleTimeString());

      setText('queuedTasks', data.queued_tasks ?? 0);
      setText('processingTasks', data.processing_tasks ?? 0);
      setText('completedTasks', data.completed_tasks ?? 0);
      setText('failedTasks', data.failed_tasks ?? 0);

      setText('admissionValue', `${inFlight} / ${data.max_in_flight_tasks ?? 0}`);
      setBar('admissionBar', inFlight, data.max_in_flight_tasks);
      setText('parserValue', `${parserBusy} / ${data.max_concurrent_requests ?? 0}`);
      setBar('parserBar', parserBusy, data.max_concurrent_requests);
      setTaskDistribution(data);

      setText('vlmActiveValue', `${data.active_vlm_requests ?? 0} / ${data.vlm_max_concurrency ?? 0}`);
      setBar('vlmActiveBar', data.active_vlm_requests, data.vlm_max_concurrency);
      setText('vlmQueueValue', `${data.vlm_queue_depth ?? 0} / ${data.vlm_queue_capacity ?? 0}`);
      setBar('vlmQueueBar', data.vlm_queue_depth, data.vlm_queue_capacity);
      setText('vlmPermitValue', data.available_vlm_permits ?? 0);
      setBar('vlmPermitBar', data.available_vlm_permits, data.vlm_max_concurrency);
      setText('perTaskVlmText', data.max_vlm_requests_per_task ?? '-');

      setText('windowSize', data.processing_window_size ?? '-');
      setText('maxUpload', formatBytes(data.max_upload_size_bytes));
      setText('retention', formatSeconds(data.task_retention_seconds));
      setText('cleanup', formatSeconds(data.task_cleanup_interval_seconds));
      setText('allocatedMemory', formatBytes(data.allocator_allocated_bytes));
      setText('residentMemory', formatBytes(residentMemory));
      setText('retainedMemory', formatBytes(data.allocator_retained_bytes));
      setText('vlmCapacity', data.vlm_queue_capacity ?? '-');
      setText('admissionPermits', data.available_admission_permits ?? '-');

      pushHistory('activeVlm', data.active_vlm_requests);
      pushHistory('queueDepth', data.vlm_queue_depth);
      pushHistory('processing', data.processing_tasks);
      pushHistory('residentMemory', residentMemory === null || residentMemory === undefined ? 0 : residentMemory);

      setText('activeVlmChartValue', data.active_vlm_requests ?? 0);
      setText('queueChartValue', data.vlm_queue_depth ?? 0);
      setText('processingChartValue', data.processing_tasks ?? 0);
      setText('memoryChartValue', formatBytes(residentMemory));
      drawChart('activeVlmChart', history.activeVlm, '#0891b2');
      drawChart('queueChart', history.queueDepth, '#b7791f');
      drawChart('processingChart', history.processing, '#2563eb');
      drawChart('memoryChart', history.residentMemory, '#168a52');
    }

    async function refreshHealth() {
      try {
        const response = await fetch('/health', { cache: 'no-store' });
        if (!response.ok) {
          throw new Error(`HTTP ${response.status}`);
        }
        renderHealth(await response.json());
        elements.errorBox.classList.remove('visible');
      } catch (error) {
        elements.statusDot.className = 'status-dot bad';
        setText('statusText', 'unreachable');
        elements.errorBox.textContent = `无法读取 /health: ${error.message}`;
        elements.errorBox.classList.add('visible');
      }
    }

    window.addEventListener('resize', () => {
      drawChart('activeVlmChart', history.activeVlm, '#0891b2');
      drawChart('queueChart', history.queueDepth, '#b7791f');
      drawChart('processingChart', history.processing, '#2563eb');
      drawChart('memoryChart', history.residentMemory, '#168a52');
    });

    refreshHealth();
    setInterval(refreshHealth, REFRESH_MS);
  </script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::{health_page, HEALTH_PAGE_HTML};

    #[tokio::test]
    async fn health_page_serves_embedded_dashboard() {
        let axum::response::Html(body) = health_page().await;

        assert!(body.contains("MinerU Health Dashboard"));
        assert!(body.contains("fetch('/health'"));
        assert!(body.contains("active_vlm_requests"));
        assert!(body.contains("allocator_resident_bytes"));
    }

    #[test]
    fn health_page_has_no_external_runtime_dependencies() {
        assert!(!HEALTH_PAGE_HTML.contains("https://"));
        assert!(!HEALTH_PAGE_HTML.contains("http://"));
        assert!(!HEALTH_PAGE_HTML.contains("<script src="));
        assert!(!HEALTH_PAGE_HTML.contains("<link rel=\"stylesheet\""));
    }
}
