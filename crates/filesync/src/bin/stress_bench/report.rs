use super::types::{DhatSummary, Event, EventKind, IntegrityResult, LogLine, ProcessSample};
use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

pub struct BenchmarkReport {
    pub total_duration: Duration,
    pub events: Vec<Event>,
    pub integrity_results: Vec<(String, IntegrityResult)>,
    pub server_samples: Vec<ProcessSample>,
    pub client_samples: Vec<ProcessSample>,
    pub dhat_server: Option<DhatSummary>,
    pub dhat_client: Option<DhatSummary>,
    pub server_logs: Vec<LogLine>,
    pub client_logs: Vec<LogLine>,
}

/// Format a byte count as a human-readable string (B / KB / MB / GB).
fn fmt_bytes(b: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if b >= GB {
        format!("{:.2} GB", b as f64 / GB as f64)
    } else if b >= MB {
        format!("{:.2} MB", b as f64 / MB as f64)
    } else if b >= KB {
        format!("{:.1} KB", b as f64 / KB as f64)
    } else {
        format!("{} B", b)
    }
}

impl BenchmarkReport {
    pub fn new(
        total_duration: Duration,
        events: Vec<Event>,
        integrity_results: Vec<(String, IntegrityResult)>,
        server_samples: Vec<ProcessSample>,
        client_samples: Vec<ProcessSample>,
        dhat_server: Option<DhatSummary>,
        dhat_client: Option<DhatSummary>,
        server_logs: Vec<LogLine>,
        client_logs: Vec<LogLine>,
    ) -> Self {
        Self {
            total_duration,
            events,
            integrity_results,
            server_samples,
            client_samples,
            dhat_server,
            dhat_client,
            server_logs,
            client_logs,
        }
    }

    /// Construct a `BenchmarkReport` from data loaded from a `data.ndjson` file.
    ///
    /// DHAT fields are always `None` because DHAT output is only available
    /// after a clean valgrind run (not stored in the crash-safe data file).
    pub fn from_loaded_data(data: super::data_store::LoadedData) -> Self {
        Self {
            total_duration: data.total_duration,
            events: data.events,
            integrity_results: data.integrity_results,
            server_samples: data.server_samples,
            client_samples: data.client_samples,
            dhat_server: None,
            dhat_client: None,
            server_logs: data.server_logs,
            client_logs: data.client_logs,
        }
    }

    pub fn generate_html(&self, path: &Path) -> io::Result<()> {
        let mut f = std::fs::File::create(path)?;

        let server_metrics_json = self.process_metrics_to_json(&self.server_samples);
        let client_metrics_json = self.process_metrics_to_json(&self.client_samples);
        let server_threads_json = self.thread_breakdown_to_json(&self.server_samples);
        let client_threads_json = self.thread_breakdown_to_json(&self.client_samples);
        let disk_io_json = self.disk_io_to_json();
        let network_json = self.network_to_json();
        let phases_json = self.phases_to_json();
        let events_json = self.events_to_json();
        let events_html = self.events_to_html();
        let integrity_html = self.integrity_to_html();
        let summary_html = self.summary_html();
        let dhat_html = self.dhat_to_html();
        let logs_js = self.logs_to_js();
        let total_log_entries = self.server_logs.len() + self.client_logs.len() + self.events.len();

        write!(
            f,
            r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>ByteHive FileSync Stress Benchmark Report</title>
<script src="https://cdn.jsdelivr.net/npm/chart.js"></script>
<script src="https://cdn.jsdelivr.net/npm/chartjs-plugin-annotation"></script>
<script src="https://cdn.jsdelivr.net/npm/hammerjs@2.0.8/hammer.min.js"></script>
<script src="https://cdn.jsdelivr.net/npm/chartjs-plugin-zoom@2.0.1/dist/chartjs-plugin-zoom.min.js"></script>

<style>
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
         background: #0d1117; color: #c9d1d9; padding: 24px; line-height: 1.6; }}
  .container {{ max-width: 1400px; margin: 0 auto; }}
  h1 {{ color: #58a6ff; margin-bottom: 8px; font-size: 28px; }}
  h2 {{ color: #79c0ff; margin: 32px 0 16px; font-size: 20px; border-bottom: 1px solid #21262d; padding-bottom: 8px; }}
  h3 {{ color: #8b949e; font-size: 15px; margin: 20px 0 8px; font-weight: 600; }}
  .summary {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(200px, 1fr)); gap: 16px; margin: 24px 0; }}
  .stat-card {{ background: #161b22; border: 1px solid #30363d; border-radius: 8px; padding: 16px; }}
  .stat-card .label {{ font-size: 12px; color: #8b949e; text-transform: uppercase; letter-spacing: 0.5px; }}
  .stat-card .value {{ font-size: 24px; font-weight: 600; color: #f0f6fc; margin-top: 4px; }}
  .stat-card .value-sm {{ font-size: 16px; font-weight: 600; color: #f0f6fc; margin-top: 4px; word-break: break-all; }}
  .stat-card.pass .value {{ color: #3fb950; }}
  .stat-card.fail .value {{ color: #f85149; }}
  .chart-container {{ background: #161b22; border: 1px solid #30363d; border-radius: 8px;
                      padding: 20px; margin: 16px 0; position: relative; }}
  canvas {{ width: 100% !important; height: 100% !important; }}
  .zoom-reset {{
    position: absolute; top: 8px; right: 8px; z-index: 10;
    background: #21262d; color: #8b949e; border: 1px solid #30363d;
    border-radius: 4px; padding: 3px 10px; cursor: pointer; font-size: 11px;
    transition: background 0.15s, color 0.15s;
  }}
  .zoom-reset:hover {{ background: #30363d; color: #c9d1d9; }}
  .zoom-hint {{
    position: absolute; bottom: 6px; right: 10px;
    font-size: 10px; color: #484f58; pointer-events: none;
  }}
  table {{ width: 100%; border-collapse: collapse; background: #161b22; border-radius: 8px; overflow: hidden; border: 1px solid #30363d; }}
  th {{ background: #21262d; color: #8b949e; padding: 12px 16px; text-align: left; font-size: 12px;
       text-transform: uppercase; letter-spacing: 0.5px; }}
  td {{ padding: 10px 16px; border-top: 1px solid #21262d; font-size: 14px; }}
  tr:hover td {{ background: #1c2128; }}
  .badge {{ display: inline-block; padding: 2px 8px; border-radius: 12px; font-size: 12px; font-weight: 600; }}
  .badge-pass {{ background: #238636; color: #fff; }}
  .badge-fail {{ background: #da3633; color: #fff; }}
  .badge-info {{ background: #1f6feb; color: #fff; }}
  .badge-phase {{ background: #8957e5; color: #fff; }}
  .subtitle {{ color: #8b949e; font-size: 14px; margin-bottom: 24px; }}
  /* ── global external tooltip ────────────────────────────── */
  #globalTooltip {{
    position: fixed;
    display: none;
    width: 280px;
    background: rgba(13,17,23,0.97);
    border: 1px solid #30363d;
    border-radius: 8px;
    padding: 12px 14px;
    z-index: 9999;
    pointer-events: none;
    box-shadow: 0 4px 24px rgba(0,0,0,0.6);
    font-size: 12px;
    color: #c9d1d9;
  }}
  .gtt-title {{
    font-size: 12px; font-weight: 600; color: #f0f6fc;
    border-bottom: 1px solid #21262d; padding-bottom: 8px; margin-bottom: 8px;
    line-height: 1.5;
  }}
  .gtt-rows {{ display: grid; gap: 4px; margin-bottom: 2px; }}
  .gtt-row  {{ display: flex; align-items: center; gap: 6px; }}
  .gtt-dot  {{ width: 8px; height: 8px; border-radius: 50%; flex-shrink: 0; }}
  .gtt-lbl  {{ color: #8b949e; flex: 1; min-width: 0; overflow: hidden;
               text-overflow: ellipsis; white-space: nowrap; }}
  .gtt-val  {{ font-weight: 600; color: #f0f6fc; text-align: right; flex-shrink: 0; }}
  .gtt-events {{
    margin-top: 8px; padding-top: 8px; border-top: 1px solid #21262d;
    font-size: 11px; color: #8b949e;
  }}
  .gtt-ev {{ line-height: 1.6; word-break: break-word; }}
  /* ── DHAT sections ──────────────────────────────────────── */
  .dhat-section {{ background: #161b22; border: 1px solid #30363d; border-radius: 8px; padding: 20px; margin: 8px 0 16px; }}
  .dhat-filepath {{
    font-size: 12px; color: #8b949e; margin-bottom: 16px;
    font-family: 'SFMono-Regular', Consolas, monospace;
  }}
  .dhat-filepath code {{ color: #79c0ff; }}
  .dhat-filepath a {{ color: #58a6ff; text-decoration: none; }}
  .dhat-filepath a:hover {{ text-decoration: underline; }}
  .dhat-hint {{ font-size: 12px; color: #8b949e; margin: 8px 0 12px; font-style: italic; }}
  .dhat-error {{ color: #f85149; font-size: 13px; margin: 8px 0; }}
  .dhat-stack {{ font-family: 'SFMono-Regular', Consolas, monospace; font-size: 11px; color: #8b949e; }}
  .dhat-stack .frame-0 {{ color: #f0f6fc; font-weight: 600; }}
  .dhat-stack .frame-1 {{ color: #c9d1d9; }}
  .dhat-stack details summary {{ cursor: pointer; color: #58a6ff; font-size: 11px; }}
  .dhat-peak {{ color: #f85149; font-weight: 600; }}
  .dhat-total {{ color: #79c0ff; }}
  /* ── Log Stream ──────────────────────────────────────────── */
  .log-toolbar {{ display: flex; flex-wrap: wrap; gap: 10px; align-items: center; margin-bottom: 12px; }}
  .log-toolbar select, .log-toolbar input {{
    background: #21262d; color: #c9d1d9; border: 1px solid #30363d;
    border-radius: 6px; padding: 6px 10px; font-size: 13px;
  }}
  .log-toolbar input {{ flex: 1; min-width: 200px; }}
  .log-toolbar button {{
    background: #238636; color: #fff; border: none; border-radius: 6px;
    padding: 6px 14px; cursor: pointer; font-size: 13px;
  }}
  .log-toolbar button:hover {{ background: #2ea043; }}
  .log-count {{ color: #8b949e; font-size: 12px; }}
  #logTableWrap {{ max-height: 600px; overflow-y: auto; border-radius: 8px; border: 1px solid #30363d; }}
  .log-tbl {{ width: 100%; border-collapse: collapse; font-family: 'SFMono-Regular', Consolas, monospace; font-size: 12px; }}
  .log-tbl th {{ background: #21262d; color: #8b949e; padding: 8px 12px; text-align: left; position: sticky; top: 0; z-index: 1; }}
  .log-tbl td {{ padding: 4px 12px; border-top: 1px solid #161b22; white-space: pre-wrap; word-break: break-all; }}
  .log-tbl tr:hover td {{ background: #1c2128; }}
  .lvl-ERROR {{ color: #f85149; }}
  .lvl-WARN  {{ color: #d29922; }}
  .lvl-INFO  {{ color: #c9d1d9; }}
  .lvl-DEBUG {{ color: #484f58; }}
  .lvl-TRACE {{ color: #30363d; }}
  .src-server {{ color: #58a6ff; font-size: 11px; }}
  .src-client {{ color: #3fb950; font-size: 11px; }}
  .src-bench  {{ color: #8957e5; font-size: 11px; }}
</style>
</head>
<body>
<div id="globalTooltip"></div>
<div class="container">
  <h1>ByteHive FileSync — Stress Benchmark Report</h1>
  <p class="subtitle">Duration: {duration:.1}s | Generated at benchmark completion</p>

  {summary}

  <h2>Server CPU Usage</h2>
  <div class="chart-container" style="height:350px">
    <button class="zoom-reset" onclick="resetChartZoom('serverCpuChart')">⟲ Reset zoom</button>
    <span class="zoom-hint">scroll to zoom · drag to pan</span>
    <canvas id="serverCpuChart"></canvas>
  </div>

  <h2>Client CPU Usage</h2>
  <div class="chart-container" style="height:350px">
    <button class="zoom-reset" onclick="resetChartZoom('clientCpuChart')">⟲ Reset zoom</button>
    <span class="zoom-hint">scroll to zoom · drag to pan</span>
    <canvas id="clientCpuChart"></canvas>
  </div>

  <h2>Server Memory</h2>
  <div class="chart-container" style="height:350px">
    <button class="zoom-reset" onclick="resetChartZoom('serverMemChart')">⟲ Reset zoom</button>
    <span class="zoom-hint">scroll to zoom · drag to pan</span>
    <canvas id="serverMemChart"></canvas>
  </div>

  <h2>Client Memory</h2>
  <div class="chart-container" style="height:350px">
    <button class="zoom-reset" onclick="resetChartZoom('clientMemChart')">⟲ Reset zoom</button>
    <span class="zoom-hint">scroll to zoom · drag to pan</span>
    <canvas id="clientMemChart"></canvas>
  </div>

  <h2>Server Thread CPU Breakdown</h2>
  <div class="chart-container" style="height:350px">
    <button class="zoom-reset" onclick="resetChartZoom('serverThreadChart')">⟲ Reset zoom</button>
    <span class="zoom-hint">scroll to zoom · drag to pan</span>
    <canvas id="serverThreadChart"></canvas>
  </div>

  <h2>Client Thread CPU Breakdown</h2>
  <div class="chart-container" style="height:350px">
    <button class="zoom-reset" onclick="resetChartZoom('clientThreadChart')">⟲ Reset zoom</button>
    <span class="zoom-hint">scroll to zoom · drag to pan</span>
    <canvas id="clientThreadChart"></canvas>
  </div>

  <h2>Disk I/O</h2>
  <div class="chart-container" style="height:350px">
    <button class="zoom-reset" onclick="resetChartZoom('diskIoChart')">⟲ Reset zoom</button>
    <span class="zoom-hint">scroll to zoom · drag to pan</span>
    <canvas id="diskIoChart"></canvas>
  </div>

  <h2>Network Throughput (MB/s)</h2>
  <div class="chart-container" style="height:350px">
    <button class="zoom-reset" onclick="resetChartZoom('netChart')">⟲ Reset zoom</button>
    <span class="zoom-hint">scroll to zoom · drag to pan</span>
    <canvas id="netChart"></canvas>
  </div>

  {dhat_html}

  <h2>Events Timeline</h2>
  {events_table}

  <h2>Integrity Results</h2>
  {integrity_table}

  <h2>Log Stream ({total_log_entries} entries)</h2>
  <div class="log-toolbar">
    <select id="logSrc" onchange="renderLogs()">
      <option value="all">All sources</option>
      <option value="server">Server</option>
      <option value="client">Client</option>
      <option value="bench">Bench</option>
    </select>
    <select id="logLvl" onchange="renderLogs()">
      <option value="all">All levels</option>
      <option value="ERROR">ERROR only</option>
      <option value="WARN">WARN and above</option>
      <option value="INFO">INFO and above</option>
      <option value="DEBUG">DEBUG and above</option>
    </select>
    <input type="text" id="logSearch" placeholder="Search log messages…" oninput="renderLogs()">
    <span class="log-count" id="logCount"></span>
    <button onclick="downloadLogs()">⬇ Download logs</button>
  </div>
  <div id="logTableWrap">
    <table class="log-tbl">
      <thead><tr><th>Time (s)</th><th>Source</th><th>Level</th><th>Message</th></tr></thead>
      <tbody id="logTbody"></tbody>
    </table>
  </div>
</div>

<script>
const serverMetrics = {server_metrics_json};
const clientMetrics = {client_metrics_json};
const serverThreads = {server_threads_json};
const clientThreads = {client_threads_json};
const diskIoData = {disk_io_json};
const networkData = {network_json};
const phasesData = {phases_json};
const eventsData = {events_json};

/* ── chart registry + zoom reset ───────────────────────────── */
const chartRegistry = {{}};
function resetChartZoom(id) {{
  if (chartRegistry[id]) chartRegistry[id].resetZoom();
}}

/* ── external tooltip handler ───────────────────────────────── */
function escapeHtml(s) {{
  return String(s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}}

function externalTooltipHandler(context) {{
  const {{chart, tooltip}} = context;
  const el = document.getElementById('globalTooltip');

  if (tooltip.opacity === 0) {{
    el.style.display = 'none';
    return;
  }}

  // ── build inner HTML ─────────────────────────────────────
  const titleText = (tooltip.title || []).join(' ');
  let html = '<div class="gtt-title">' + escapeHtml(titleText) + '</div>';

  // data-point rows
  if (tooltip.dataPoints && tooltip.dataPoints.length) {{
    html += '<div class="gtt-rows">';
    tooltip.dataPoints.forEach(function(dp, i) {{
      const colors = tooltip.labelColors && tooltip.labelColors[i];
      const dot    = (colors && colors.borderColor) ? colors.borderColor : '#8b949e';
      const label  = dp.dataset ? escapeHtml(dp.dataset.label || '') : '';
      const val    = escapeHtml(dp.formattedValue || '');
      html += '<div class="gtt-row">'
            + '<span class="gtt-dot" style="background:' + dot + '"></span>'
            + '<span class="gtt-lbl">' + label + '</span>'
            + '<span class="gtt-val">' + val + '</span>'
            + '</div>';
    }});
    html += '</div>';
  }}

  // after-body lines (events / extra context)
  const afterBody = tooltip.afterBody || [];
  if (afterBody.length) {{
    html += '<div class="gtt-events">';
    afterBody.forEach(function(line) {{
      html += '<div class="gtt-ev">' + escapeHtml(line) + '</div>';
    }});
    html += '</div>';
  }}

  el.innerHTML = html;

  // ── position ─────────────────────────────────────────────
  const rect = chart.canvas.getBoundingClientRect();
  const cx   = rect.left + window.scrollX + tooltip.caretX;
  const cy   = rect.top  + window.scrollY + tooltip.caretY;
  const tipW = el.offsetWidth  || 280;
  const tipH = el.offsetHeight || 120;
  const vw   = window.innerWidth;
  const vh   = window.innerHeight;

  let left = cx + 14;
  let top  = cy - 20;
  if (left + tipW > vw - 8) left = cx - tipW - 14;
  if (top  + tipH > vh - 8) top  = vh - tipH - 8;
  if (top  < 8)             top  = 8;

  el.style.left    = left + 'px';
  el.style.top     = top  + 'px';
  el.style.display = 'block';
}}

/* ── colours for phase overlays ────────────────────────────── */
const phaseColors = [
  'rgba(88, 166, 255, 0.12)',
  'rgba(63, 185, 80, 0.12)',
  'rgba(210, 153, 34, 0.12)',
  'rgba(248, 81, 73, 0.12)',
  'rgba(137, 87, 229, 0.12)',
  'rgba(219, 171, 9, 0.12)',
  'rgba(56, 211, 159, 0.12)',
];
const phaseTextColors = [
  '#58a6ff','#3fb950','#d29922','#f85149','#8957e5','#dbab09','#38d39f',
];

/* ── colour palette for thread breakdown ───────────────────── */
const threadPalette = [
  '#58a6ff','#3fb950','#f85149','#d29922','#8957e5',
  '#38d39f','#f778ba','#79c0ff','#dbab09','#ff7b72',
];

/* ── build annotation boxes for every phase ────────────────── */
function buildAnnotations() {{
  const a = {{}};
  phasesData.forEach((p, i) => {{
    a['phase_' + i] = {{
      type: 'box',
      xMin: p.start,
      xMax: p.end,
      backgroundColor: phaseColors[i % phaseColors.length],
      borderWidth: 0,
      label: {{
        display: true,
        content: p.name,
        position: 'start',
        color: phaseTextColors[i % phaseTextColors.length],
        font: {{ size: 12, weight: 'bold' }},
        padding: 6,
      }}
    }};
  }});
  return a;
}}

/* ── look-up helpers used by the tooltip ───────────────────── */
function activePhaseAt(t) {{
  for (const p of phasesData) {{
    if (t >= p.start && t <= p.end) return p.name;
  }}
  return null;
}}

function nearbyEvents(t, windowSec) {{
  return eventsData.filter(e => Math.abs(e.t - t) <= windowSec);
}}

/* ── shared Chart.js options ───────────────────────────────── */
const commonScales = {{
  x: {{
    title: {{ display: true, text: 'Time (s)', color: '#8b949e' }},
    ticks: {{ color: '#484f58', maxTicksLimit: 30 }},
    grid:  {{ color: '#21262d' }},
  }},
}};

function makeTooltip(extraLines) {{
  return {{
    enabled: false,
    external: externalTooltipHandler,
    mode: 'index',
    intersect: false,
    callbacks: {{
      title: function(items) {{
        if (!items.length) return '';
        const t = parseFloat(items[0].label);
        const phase = activePhaseAt(t);
        let title = 'Time: ' + t.toFixed(1) + 's';
        if (phase) title += '  \u2502  Phase: ' + phase;
        return title;
      }},
      afterBody: function(items) {{
        if (!items.length) return [];
        const t = parseFloat(items[0].label);
        const nearby = nearbyEvents(t, 1.5);
        if (nearby.length === 0) return [];
        const lines = ['\u2500\u2500 Events nearby \u2500\u2500'];
        nearby.forEach(ev => {{
          lines.push('\u25B8 ' + ev.text);
        }});
        if (typeof extraLines === 'function') {{
          const ex = extraLines(items);
          if (ex && ex.length) lines.push('', ...ex);
        }}
        return lines;
      }},
    }},
  }};
}}

function makeOptions(yLabel, extraTooltipLines) {{
  return {{
    responsive: true,
    maintainAspectRatio: false,
    animation: false,
    interaction: {{
      mode: 'index',
      intersect: false,
    }},
    hover: {{
      mode: 'index',
      intersect: false,
    }},
    plugins: {{
      legend: {{ labels: {{ color: '#c9d1d9', padding: 16, usePointStyle: true, pointStyleWidth: 14 }} }},
      annotation: {{ annotations: buildAnnotations() }},
      tooltip: makeTooltip(extraTooltipLines),
      zoom: {{
        zoom: {{
          wheel: {{ enabled: true }},
          pinch: {{ enabled: true }},
          mode: 'x',
        }},
        pan: {{
          enabled: true,
          mode: 'x',
        }},
        limits: {{
          x: {{ min: 'original', max: 'original' }},
        }},
      }},
    }},
    scales: {{
      ...commonScales,
      y: {{
        title: {{ display: true, text: yLabel, color: '#8b949e' }},
        ticks: {{ color: '#484f58' }},
        grid:  {{ color: '#21262d' }},
        min: 0,
      }},
    }},
  }};
}}

function makeLineDataset(label, data, color, fill) {{
  return {{
    label: label,
    data: data,
    borderColor: color,
    backgroundColor: fill ? color.replace(')', ',0.15)').replace('rgb(', 'rgba(') : 'transparent',
    fill: fill,
    pointRadius: 1,
    pointHoverRadius: 5,
    pointBackgroundColor: color,
    pointHoverBackgroundColor: '#fff',
    pointBorderWidth: 0,
    borderWidth: 2,
    tension: 0.25,
  }};
}}

/* ── 1. Server CPU Chart ───────────────────────────────────── */
chartRegistry['serverCpuChart'] = new Chart(document.getElementById('serverCpuChart'), {{
  type: 'line',
  data: {{
    labels: serverMetrics.time,
    datasets: [
      makeLineDataset('Total CPU %', serverMetrics.cpu, '#58a6ff', false),
      makeLineDataset('User CPU %', serverMetrics.user_cpu, '#3fb950', false),
      makeLineDataset('System CPU %', serverMetrics.sys_cpu, '#f85149', false),
    ]
  }},
  options: makeOptions('CPU %', null),
}});

/* ── 2. Client CPU Chart ───────────────────────────────────── */
chartRegistry['clientCpuChart'] = new Chart(document.getElementById('clientCpuChart'), {{
  type: 'line',
  data: {{
    labels: clientMetrics.time,
    datasets: [
      makeLineDataset('Total CPU %', clientMetrics.cpu, '#79c0ff', false),
      makeLineDataset('User CPU %', clientMetrics.user_cpu, '#56d364', false),
      makeLineDataset('System CPU %', clientMetrics.sys_cpu, '#ff7b72', false),
    ]
  }},
  options: makeOptions('CPU %', null),
}});

/* ── 3. Server Memory Chart ────────────────────────────────── */
chartRegistry['serverMemChart'] = new Chart(document.getElementById('serverMemChart'), {{
  type: 'line',
  data: {{
    labels: serverMetrics.time,
    datasets: [
      makeLineDataset('RSS', serverMetrics.rss, '#3fb950', false),
      makeLineDataset('Private', serverMetrics.private_mem, '#58a6ff', false),
      makeLineDataset('Shared', serverMetrics.shared_mem, '#8957e5', false),
    ]
  }},
  options: makeOptions('Memory (MB)', null),
}});

/* ── 4. Client Memory Chart ────────────────────────────────── */
chartRegistry['clientMemChart'] = new Chart(document.getElementById('clientMemChart'), {{
  type: 'line',
  data: {{
    labels: clientMetrics.time,
    datasets: [
      makeLineDataset('RSS', clientMetrics.rss, '#3fb950', false),
      makeLineDataset('Private', clientMetrics.private_mem, '#58a6ff', false),
      makeLineDataset('Shared', clientMetrics.shared_mem, '#8957e5', false),
    ]
  }},
  options: makeOptions('Memory (MB)', null),
}});

/* ── 5. Server Thread CPU Breakdown ────────────────────────── */
(function() {{
  const datasets = [];
  serverThreads.names.forEach((name, i) => {{
    datasets.push(makeLineDataset(name, serverThreads.series[name], threadPalette[i % threadPalette.length], false));
  }});
  chartRegistry['serverThreadChart'] = new Chart(document.getElementById('serverThreadChart'), {{
    type: 'line',
    data: {{ labels: serverThreads.time, datasets: datasets }},
    options: makeOptions('Thread CPU %', null),
  }});
}})();

/* ── 6. Client Thread CPU Breakdown ────────────────────────── */
(function() {{
  const datasets = [];
  clientThreads.names.forEach((name, i) => {{
    datasets.push(makeLineDataset(name, clientThreads.series[name], threadPalette[i % threadPalette.length], false));
  }});
  chartRegistry['clientThreadChart'] = new Chart(document.getElementById('clientThreadChart'), {{
    type: 'line',
    data: {{ labels: clientThreads.time, datasets: datasets }},
    options: makeOptions('Thread CPU %', null),
  }});
}})();

/* ── 7. Disk I/O Chart ─────────────────────────────────────── */
chartRegistry['diskIoChart'] = new Chart(document.getElementById('diskIoChart'), {{
  type: 'line',
  data: {{
    labels: diskIoData.time,
    datasets: [
      makeLineDataset('Server Read (MB/s)', diskIoData.server_read_rate, '#58a6ff', false),
      makeLineDataset('Server Write (MB/s)', diskIoData.server_write_rate, '#3fb950', false),
      makeLineDataset('Client Read (MB/s)', diskIoData.client_read_rate, '#79c0ff', false),
      makeLineDataset('Client Write (MB/s)', diskIoData.client_write_rate, '#56d364', false),
    ]
  }},
  options: makeOptions('MB/s', null),
}});

/* ── 8. Network Throughput Chart ───────────────────────────── */
chartRegistry['netChart'] = new Chart(document.getElementById('netChart'), {{
  type: 'line',
  data: {{
    labels: networkData.time,
    datasets: [
      makeLineDataset('TX (MB/s)', networkData.tx_rate, '#d29922', false),
      makeLineDataset('RX (MB/s)', networkData.rx_rate, '#f85149', false),
    ]
  }},
  options: makeOptions('MB/s', null),
}});


/* ── Log Stream ──────────────────────────────────────────────── */
const allLogs = {logs_js};
const LOG_LEVEL_ORDER = {{"TRACE":0,"DEBUG":1,"INFO":2,"WARN":3,"ERROR":4}};
const MAX_ROWS = 2000;

let _filteredLogs = allLogs;

function renderLogs() {{
  const src   = document.getElementById('logSrc').value;
  const lvl   = document.getElementById('logLvl').value;
  const text  = document.getElementById('logSearch').value.toLowerCase();
  const minOrd = lvl === 'all' ? -1 : (LOG_LEVEL_ORDER[lvl] ?? 2);

  _filteredLogs = allLogs.filter(function(e) {{
    if (src !== 'all' && e.src !== src) return false;
    if (lvl !== 'all' && (LOG_LEVEL_ORDER[e.lvl] ?? 2) < minOrd) return false;
    if (text && e.msg.toLowerCase().indexOf(text) === -1) return false;
    return true;
  }});

  const showing = Math.min(_filteredLogs.length, MAX_ROWS);
  document.getElementById('logCount').textContent =
    'Showing ' + showing + ' of ' + _filteredLogs.length + ' matching (' + allLogs.length + ' total)';

  const lvlColors = {{ERROR:'#f85149',WARN:'#d29922',INFO:'#c9d1d9',DEBUG:'#484f58',TRACE:'#30363d'}};
  const srcColors = {{server:'#58a6ff',client:'#3fb950',bench:'#8957e5'}};

  const rows = _filteredLogs.slice(0, MAX_ROWS).map(function(e) {{
    const lc = lvlColors[e.lvl] || '#c9d1d9';
    const sc = srcColors[e.src] || '#8b949e';
    const msg = e.msg.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
    return '<tr>'
      + '<td style="color:#484f58;white-space:nowrap">' + e.t.toFixed(1) + 's</td>'
      + '<td style="color:' + sc + ';white-space:nowrap">' + e.src + '</td>'
      + '<td style="color:' + lc + ';white-space:nowrap;font-weight:600">' + e.lvl + '</td>'
      + '<td style="color:' + lc + '">' + msg + '</td>'
      + '</tr>';
  }}).join('');

  document.getElementById('logTbody').innerHTML = rows;
}}

function downloadLogs() {{
  const lines = _filteredLogs.map(function(e) {{
    return e.t.toFixed(1) + 's [' + e.src.toUpperCase() + '] [' + e.lvl + '] ' + e.msg;
  }});
  const blob = new Blob([lines.join('\n')], {{type:'text/plain'}});
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = 'bench-logs.txt';
  a.click();
}}

// Initial render
renderLogs();

</script>
</body>
</html>
"##,
            duration = self.total_duration.as_secs_f64(),
            summary = summary_html,
            dhat_html = dhat_html,
            events_table = events_html,
            integrity_table = integrity_html,
            server_metrics_json = server_metrics_json,
            client_metrics_json = client_metrics_json,
            server_threads_json = server_threads_json,
            client_threads_json = client_threads_json,
            disk_io_json = disk_io_json,
            network_json = network_json,
            phases_json = phases_json,
            events_json = events_json,
            logs_js = logs_js,
            total_log_entries = total_log_entries,
        )?;

        Ok(())
    }

    fn logs_to_js(&self) -> String {
        // Helper: escape a string for safe JSON embedding
        fn esc(s: &str) -> String {
            s.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
        }

        // Build (timestamp, json-string) pairs then sort numerically by time
        let mut entries: Vec<(f64, String)> = Vec::new();
        for ev in &self.events {
            let (level, msg) = match &ev.kind {
                EventKind::PhaseStart(n) => ("INFO", format!("Phase started: {n}")),
                EventKind::PhaseEnd(n) => ("INFO", format!("Phase ended: {n}")),
                EventKind::FilesCreated { count, total_bytes } => (
                    "INFO",
                    format!(
                        "Created {count} files ({:.1} MB)",
                        *total_bytes as f64 / 1_048_576.0
                    ),
                ),
                EventKind::FilesModified { count, total_bytes } => (
                    "INFO",
                    format!(
                        "Modified {count} files ({:.1} MB)",
                        *total_bytes as f64 / 1_048_576.0
                    ),
                ),
                EventKind::FilesDeleted { count } => ("INFO", format!("Deleted {count} files")),
                EventKind::SyncWaitComplete { duration_secs } => {
                    ("INFO", format!("Sync wait complete ({duration_secs:.1}s)"))
                }
                EventKind::IntegrityCheck {
                    phase,
                    passed,
                    matched,
                    mismatched,
                    missing,
                    extra,
                } => {
                    let lvl = if *passed { "INFO" } else { "WARN" };
                    let tag = if *passed { "PASS" } else { "FAIL" };
                    (lvl, format!("Integrity [{tag}] {phase}: {matched} ok, {mismatched} mismatch, {missing} missing, {extra} extra"))
                }
                EventKind::Info(m) => ("INFO", m.clone()),
            };
            entries.push((
                ev.elapsed_secs,
                format!(
                    r#"{{"t":{:.1},"src":"bench","lvl":"{}","msg":"{}"}}"#,
                    ev.elapsed_secs,
                    level,
                    esc(&msg)
                ),
            ));
        }
        for ll in &self.server_logs {
            entries.push((
                ll.elapsed_secs,
                format!(
                    r#"{{"t":{:.1},"src":"server","lvl":"{}","msg":"{}"}}"#,
                    ll.elapsed_secs,
                    ll.level,
                    esc(&ll.message)
                ),
            ));
        }
        for ll in &self.client_logs {
            entries.push((
                ll.elapsed_secs,
                format!(
                    r#"{{"t":{:.1},"src":"client","lvl":"{}","msg":"{}"}}"#,
                    ll.elapsed_secs,
                    ll.level,
                    esc(&ll.message)
                ),
            ));
        }
        entries.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let json_items: Vec<String> = entries.into_iter().map(|(_, s)| s).collect();
        format!("[{}]", json_items.join(","))
    }

    // ── JSON serialisation helpers ───────────────────────────────────────

    /// Generate a JSON object with time-series arrays for a process's basic metrics.
    fn process_metrics_to_json(&self, samples: &[ProcessSample]) -> String {
        let times: Vec<String> = samples
            .iter()
            .map(|s| format!("{:.1}", s.elapsed_secs))
            .collect();
        let cpus: Vec<String> = samples
            .iter()
            .map(|s| format!("{:.1}", s.cpu_percent))
            .collect();
        let user_cpus: Vec<String> = samples
            .iter()
            .map(|s| format!("{:.1}", s.user_cpu_percent))
            .collect();
        let sys_cpus: Vec<String> = samples
            .iter()
            .map(|s| format!("{:.1}", s.sys_cpu_percent))
            .collect();
        let rss: Vec<String> = samples
            .iter()
            .map(|s| format!("{:.1}", s.rss_bytes as f64 / (1024.0 * 1024.0)))
            .collect();
        let private_mem: Vec<String> = samples
            .iter()
            .map(|s| format!("{:.1}", s.private_bytes as f64 / (1024.0 * 1024.0)))
            .collect();
        let shared_mem: Vec<String> = samples
            .iter()
            .map(|s| format!("{:.1}", s.shared_bytes as f64 / (1024.0 * 1024.0)))
            .collect();

        format!(
            r#"{{"time":[{times}],"cpu":[{cpus}],"user_cpu":[{user_cpus}],"sys_cpu":[{sys_cpus}],"rss":[{rss}],"private_mem":[{private_mem}],"shared_mem":[{shared_mem}]}}"#,
            times = times.join(","),
            cpus = cpus.join(","),
            user_cpus = user_cpus.join(","),
            sys_cpus = sys_cpus.join(","),
            rss = rss.join(","),
            private_mem = private_mem.join(","),
            shared_mem = shared_mem.join(","),
        )
    }

    /// Generate a JSON object with per-thread CPU time-series for a process.
    fn thread_breakdown_to_json(&self, samples: &[ProcessSample]) -> String {
        // Collect all unique thread names across every sample.
        let mut all_names = BTreeSet::new();
        for sample in samples {
            for thread in &sample.threads {
                all_names.insert(thread.name.clone());
            }
        }
        let names: Vec<String> = all_names.into_iter().collect();

        let times: Vec<String> = samples
            .iter()
            .map(|s| format!("{:.1}", s.elapsed_secs))
            .collect();

        // Build per-thread series: for each thread name, for each time point,
        // look up the cpu_percent (0 if the thread wasn't present in that sample).
        let mut series_parts: Vec<String> = Vec::new();
        for name in &names {
            let values: Vec<String> = samples
                .iter()
                .map(|s| {
                    let cpu = s
                        .threads
                        .iter()
                        .find(|t| &t.name == name)
                        .map(|t| t.cpu_percent)
                        .unwrap_or(0.0);
                    format!("{:.1}", cpu)
                })
                .collect();
            let safe_name = name.replace('\\', "\\\\").replace('"', "\\\"");
            series_parts.push(format!(
                r#""{}": [{values}]"#,
                safe_name,
                values = values.join(",")
            ));
        }

        let names_json: Vec<String> = names
            .iter()
            .map(|n| {
                let safe = n.replace('\\', "\\\\").replace('"', "\\\"");
                format!(r#""{}""#, safe)
            })
            .collect();

        format!(
            r#"{{"time":[{times}],"names":[{names}],"series":{{{series}}}}}"#,
            times = times.join(","),
            names = names_json.join(","),
            series = series_parts.join(","),
        )
    }

    /// Generate JSON with disk I/O rates for both server and client.
    fn disk_io_to_json(&self) -> String {
        // We merge server and client onto a common time axis.
        // Use the longer sample set's timestamps as the axis, padding the shorter
        // one with zeros past its end.
        let max_len = self.server_samples.len().max(self.client_samples.len());
        let mut times = Vec::with_capacity(max_len);
        let mut server_read_rate = Vec::with_capacity(max_len);
        let mut server_write_rate = Vec::with_capacity(max_len);
        let mut client_read_rate = Vec::with_capacity(max_len);
        let mut client_write_rate = Vec::with_capacity(max_len);

        for i in 0..max_len {
            // Pick time from whichever set has this index, preferring server.
            let t = if i < self.server_samples.len() {
                self.server_samples[i].elapsed_secs
            } else {
                self.client_samples[i].elapsed_secs
            };
            times.push(format!("{:.1}", t));

            // Server rates
            if i < self.server_samples.len() && i > 0 {
                let dt =
                    self.server_samples[i].elapsed_secs - self.server_samples[i - 1].elapsed_secs;
                if dt > 0.0 {
                    let rd = self.server_samples[i]
                        .io_read_bytes
                        .saturating_sub(self.server_samples[i - 1].io_read_bytes);
                    let wd = self.server_samples[i]
                        .io_write_bytes
                        .saturating_sub(self.server_samples[i - 1].io_write_bytes);
                    server_read_rate.push(format!("{:.2}", rd as f64 / dt / (1024.0 * 1024.0)));
                    server_write_rate.push(format!("{:.2}", wd as f64 / dt / (1024.0 * 1024.0)));
                } else {
                    server_read_rate.push("0.0".to_string());
                    server_write_rate.push("0.0".to_string());
                }
            } else if i < self.server_samples.len() {
                server_read_rate.push("0.0".to_string());
                server_write_rate.push("0.0".to_string());
            } else {
                server_read_rate.push("0.0".to_string());
                server_write_rate.push("0.0".to_string());
            }

            // Client rates
            if i < self.client_samples.len() && i > 0 {
                let dt =
                    self.client_samples[i].elapsed_secs - self.client_samples[i - 1].elapsed_secs;
                if dt > 0.0 {
                    let rd = self.client_samples[i]
                        .io_read_bytes
                        .saturating_sub(self.client_samples[i - 1].io_read_bytes);
                    let wd = self.client_samples[i]
                        .io_write_bytes
                        .saturating_sub(self.client_samples[i - 1].io_write_bytes);
                    client_read_rate.push(format!("{:.2}", rd as f64 / dt / (1024.0 * 1024.0)));
                    client_write_rate.push(format!("{:.2}", wd as f64 / dt / (1024.0 * 1024.0)));
                } else {
                    client_read_rate.push("0.0".to_string());
                    client_write_rate.push("0.0".to_string());
                }
            } else if i < self.client_samples.len() {
                client_read_rate.push("0.0".to_string());
                client_write_rate.push("0.0".to_string());
            } else {
                client_read_rate.push("0.0".to_string());
                client_write_rate.push("0.0".to_string());
            }
        }

        format!(
            r#"{{"time":[{times}],"server_read_rate":[{sr}],"server_write_rate":[{sw}],"client_read_rate":[{cr}],"client_write_rate":[{cw}]}}"#,
            times = times.join(","),
            sr = server_read_rate.join(","),
            sw = server_write_rate.join(","),
            cr = client_read_rate.join(","),
            cw = client_write_rate.join(","),
        )
    }

    /// Generate JSON with network throughput rates (from server samples).
    fn network_to_json(&self) -> String {
        let samples = &self.server_samples;
        let times: Vec<String> = samples
            .iter()
            .map(|s| format!("{:.1}", s.elapsed_secs))
            .collect();

        let mut tx_rates = Vec::with_capacity(samples.len());
        let mut rx_rates = Vec::with_capacity(samples.len());

        for i in 0..samples.len() {
            if i == 0 {
                tx_rates.push("0.0".to_string());
                rx_rates.push("0.0".to_string());
            } else {
                let dt = samples[i].elapsed_secs - samples[i - 1].elapsed_secs;
                if dt > 0.0 {
                    let tx_delta = samples[i]
                        .net_tx_bytes
                        .saturating_sub(samples[i - 1].net_tx_bytes);
                    let rx_delta = samples[i]
                        .net_rx_bytes
                        .saturating_sub(samples[i - 1].net_rx_bytes);
                    tx_rates.push(format!("{:.2}", tx_delta as f64 / dt / (1024.0 * 1024.0)));
                    rx_rates.push(format!("{:.2}", rx_delta as f64 / dt / (1024.0 * 1024.0)));
                } else {
                    tx_rates.push("0.0".to_string());
                    rx_rates.push("0.0".to_string());
                }
            }
        }

        format!(
            r#"{{"time":[{times}],"tx_rate":[{tx}],"rx_rate":[{rx}]}}"#,
            times = times.join(","),
            tx = tx_rates.join(","),
            rx = rx_rates.join(","),
        )
    }

    fn phases_to_json(&self) -> String {
        let mut phases = Vec::new();
        let mut open: Option<(String, f64)> = None;

        for event in &self.events {
            match &event.kind {
                EventKind::PhaseStart(name) => {
                    open = Some((name.clone(), event.elapsed_secs));
                }
                EventKind::PhaseEnd(name) => {
                    if let Some((ref open_name, start)) = open {
                        if open_name == name {
                            phases.push(format!(
                                r#"{{"name":"{}","start":{:.1},"end":{:.1}}}"#,
                                name, start, event.elapsed_secs
                            ));
                            open = None;
                        }
                    }
                }
                _ => {}
            }
        }

        format!("[{}]", phases.join(","))
    }

    /// Serialize events into a JSON array so the tooltip JS can look them up.
    fn events_to_json(&self) -> String {
        let mut items = Vec::new();
        for event in &self.events {
            let text = match &event.kind {
                EventKind::PhaseStart(name) => format!("Phase started: {name}"),
                EventKind::PhaseEnd(name) => format!("Phase ended: {name}"),
                EventKind::FilesCreated { count, total_bytes } => {
                    format!(
                        "Created {} files ({:.1} MB)",
                        count,
                        *total_bytes as f64 / (1024.0 * 1024.0)
                    )
                }
                EventKind::FilesModified { count, total_bytes } => {
                    format!(
                        "Modified {} files ({:.1} MB)",
                        count,
                        *total_bytes as f64 / (1024.0 * 1024.0)
                    )
                }
                EventKind::FilesDeleted { count } => format!("Deleted {count} files"),
                EventKind::IntegrityCheck {
                    phase,
                    passed,
                    matched,
                    mismatched,
                    missing,
                    ..
                } => {
                    let tag = if *passed { "PASS" } else { "FAIL" };
                    format!(
                        "Integrity [{tag}] {phase}: {matched} ok, {mismatched} bad, {missing} missing"
                    )
                }
                EventKind::SyncWaitComplete { duration_secs } => {
                    format!("Sync wait done ({duration_secs:.1}s)")
                }
                EventKind::Info(msg) => msg.clone(),
            };
            // Escape double-quotes and backslashes for safe JSON embedding.
            let safe = text.replace('\\', "\\\\").replace('"', "\\\"");
            items.push(format!(
                r#"{{"t":{:.1},"text":"{}"}}"#,
                event.elapsed_secs, safe
            ));
        }
        format!("[{}]", items.join(","))
    }

    // ── HTML fragment helpers ────────────────────────────────────────────

    fn events_to_html(&self) -> String {
        let mut rows = String::new();
        for event in &self.events {
            let (badge, description) = match &event.kind {
                EventKind::PhaseStart(name) => (
                    r#"<span class="badge badge-phase">PHASE</span>"#.to_string(),
                    format!("Started: <strong>{name}</strong>"),
                ),
                EventKind::PhaseEnd(name) => (
                    r#"<span class="badge badge-phase">PHASE</span>"#.to_string(),
                    format!("Ended: <strong>{name}</strong>"),
                ),
                EventKind::FilesCreated { count, total_bytes } => (
                    r#"<span class="badge badge-info">FILES</span>"#.to_string(),
                    format!(
                        "Created {count} files ({:.1} MB)",
                        *total_bytes as f64 / (1024.0 * 1024.0)
                    ),
                ),
                EventKind::FilesModified { count, total_bytes } => (
                    r#"<span class="badge badge-info">MODIFY</span>"#.to_string(),
                    format!(
                        "Modified {count} files ({:.1} MB)",
                        *total_bytes as f64 / (1024.0 * 1024.0)
                    ),
                ),
                EventKind::FilesDeleted { count } => (
                    r#"<span class="badge badge-info">DELETE</span>"#.to_string(),
                    format!("Deleted {count} files"),
                ),
                EventKind::IntegrityCheck {
                    phase,
                    passed,
                    matched,
                    mismatched,
                    missing,
                    extra,
                } => {
                    let badge_class = if *passed { "badge-pass" } else { "badge-fail" };
                    let status = if *passed { "PASS" } else { "FAIL" };
                    (
                        format!(r#"<span class="badge {badge_class}">{status}</span>"#),
                        format!(
                            "<strong>{phase}</strong>: {matched} matched, {mismatched} mismatched, {missing} missing, {extra} extra"
                        ),
                    )
                }
                EventKind::SyncWaitComplete { duration_secs } => (
                    r#"<span class="badge badge-info">SYNC</span>"#.to_string(),
                    format!("Sync completed in {duration_secs:.1}s"),
                ),
                EventKind::Info(msg) => (
                    r#"<span class="badge badge-info">INFO</span>"#.to_string(),
                    msg.clone(),
                ),
            };

            rows.push_str(&format!(
                "<tr><td>{:.1}s</td><td>{badge}</td><td>{description}</td></tr>\n",
                event.elapsed_secs
            ));
        }

        format!(
            "<table><thead><tr><th>Time</th><th>Type</th><th>Details</th></tr></thead><tbody>{rows}</tbody></table>"
        )
    }

    fn integrity_to_html(&self) -> String {
        if self.integrity_results.is_empty() {
            return "<p>No integrity checks performed.</p>".to_string();
        }

        let mut rows = String::new();
        for (phase, result) in &self.integrity_results {
            let status = if result.passed() {
                r#"<span class="badge badge-pass">PASS</span>"#
            } else {
                r#"<span class="badge badge-fail">FAIL</span>"#
            };
            rows.push_str(&format!(
                "<tr><td>{phase}</td><td>{status}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                result.matched,
                result.mismatched.len(),
                result.missing_from_dest.len(),
                result.extra_in_dest.len(),
            ));
        }

        format!(
            "<table><thead><tr><th>Phase</th><th>Status</th><th>Matched</th><th>Mismatched</th><th>Missing</th><th>Extra</th></tr></thead><tbody>{rows}</tbody></table>"
        )
    }

    fn summary_html(&self) -> String {
        let total_files_created: usize = self
            .events
            .iter()
            .map(|e| match &e.kind {
                EventKind::FilesCreated { count, .. } => *count,
                _ => 0,
            })
            .sum();
        let total_bytes: u64 = self
            .events
            .iter()
            .map(|e| match &e.kind {
                EventKind::FilesCreated { total_bytes, .. } => *total_bytes,
                EventKind::FilesModified { total_bytes, .. } => *total_bytes,
                _ => 0,
            })
            .sum();
        let all_passed = self.integrity_results.iter().all(|(_, r)| r.passed());

        let pass_class = if all_passed { "pass" } else { "fail" };
        let pass_text = if all_passed {
            "ALL PASSED"
        } else {
            "FAILURES DETECTED"
        };

        // Server peaks
        let server_peak_cpu = self
            .server_samples
            .iter()
            .map(|s| s.cpu_percent)
            .fold(0.0_f64, f64::max);
        let server_peak_rss = self
            .server_samples
            .iter()
            .map(|s| s.rss_bytes)
            .max()
            .unwrap_or(0);
        let server_peak_threads = self
            .server_samples
            .iter()
            .map(|s| s.thread_count)
            .max()
            .unwrap_or(0);
        let server_peak_io_rate = self.peak_io_rate(&self.server_samples);

        // Client peaks
        let client_peak_cpu = self
            .client_samples
            .iter()
            .map(|s| s.cpu_percent)
            .fold(0.0_f64, f64::max);
        let client_peak_rss = self
            .client_samples
            .iter()
            .map(|s| s.rss_bytes)
            .max()
            .unwrap_or(0);
        let client_peak_threads = self
            .client_samples
            .iter()
            .map(|s| s.thread_count)
            .max()
            .unwrap_or(0);
        let client_peak_io_rate = self.peak_io_rate(&self.client_samples);

        format!(
            r##"<div class="summary">
  <div class="stat-card"><div class="label">Duration</div><div class="value">{duration:.0}s</div></div>
  <div class="stat-card"><div class="label">Files Created</div><div class="value">{files_created}</div></div>
  <div class="stat-card"><div class="label">Data Written</div><div class="value">{data_written:.1} GB</div></div>
  <div class="stat-card {pass_class}"><div class="label">Integrity Result</div><div class="value">{pass_text}</div></div>
</div>
<div class="summary">
  <div class="stat-card"><div class="label">Server Peak CPU</div><div class="value">{server_cpu:.0}%</div></div>
  <div class="stat-card"><div class="label">Client Peak CPU</div><div class="value">{client_cpu:.0}%</div></div>
  <div class="stat-card"><div class="label">Server Peak Memory</div><div class="value">{server_mem:.0} MB</div></div>
  <div class="stat-card"><div class="label">Client Peak Memory</div><div class="value">{client_mem:.0} MB</div></div>
  <div class="stat-card"><div class="label">Server Peak Threads</div><div class="value">{server_threads}</div></div>
  <div class="stat-card"><div class="label">Client Peak Threads</div><div class="value">{client_threads}</div></div>
  <div class="stat-card"><div class="label">Server Peak I/O</div><div class="value">{server_io:.1} MB/s</div></div>
  <div class="stat-card"><div class="label">Client Peak I/O</div><div class="value">{client_io:.1} MB/s</div></div>
</div>"##,
            duration = self.total_duration.as_secs_f64(),
            files_created = total_files_created,
            data_written = total_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
            pass_class = pass_class,
            pass_text = pass_text,
            server_cpu = server_peak_cpu,
            client_cpu = client_peak_cpu,
            server_mem = server_peak_rss as f64 / (1024.0 * 1024.0),
            client_mem = client_peak_rss as f64 / (1024.0 * 1024.0),
            server_threads = server_peak_threads,
            client_threads = client_peak_threads,
            server_io = server_peak_io_rate,
            client_io = client_peak_io_rate,
        )
    }

    /// Compute the peak combined (read + write) I/O rate in MB/s for a process.
    fn peak_io_rate(&self, samples: &[ProcessSample]) -> f64 {
        let mut peak = 0.0_f64;
        for i in 1..samples.len() {
            let dt = samples[i].elapsed_secs - samples[i - 1].elapsed_secs;
            if dt > 0.0 {
                let read_delta = samples[i]
                    .io_read_bytes
                    .saturating_sub(samples[i - 1].io_read_bytes);
                let write_delta = samples[i]
                    .io_write_bytes
                    .saturating_sub(samples[i - 1].io_write_bytes);
                let rate = (read_delta + write_delta) as f64 / dt / (1024.0 * 1024.0);
                if rate > peak {
                    peak = rate;
                }
            }
        }
        peak
    }

    // ── DHAT report sections ─────────────────────────────────────────────

    /// Generates the DHAT sections for the HTML report.
    /// Returns an empty string when neither server nor client was profiled.
    fn dhat_to_html(&self) -> String {
        let mut out = String::new();
        if let Some(ref s) = self.dhat_server {
            out.push_str(&Self::dhat_section_html("Server", s));
        }
        if let Some(ref c) = self.dhat_client {
            out.push_str(&Self::dhat_section_html("Client", c));
        }
        out
    }

    fn dhat_section_html(label: &str, s: &DhatSummary) -> String {
        let path_str = s.output_path.display().to_string();

        // Summary stat cards
        let mut cards = String::new();
        let mut push_card = |lbl: &str, val: &str| {
            cards.push_str(&format!(
                r#"<div class="stat-card"><div class="label">{lbl}</div><div class="value-sm">{val}</div></div>"#,
            ));
        };
        push_card("Total Allocated", &fmt_bytes(s.total_bytes));
        push_card("Total Alloc Calls", &format!("{}", s.total_blocks));
        push_card("Max Site Peak", &fmt_bytes(s.max_site_peak_bytes));

        // Error notice (if parsing failed)
        let error_html = if let Some(ref e) = s.parse_error {
            format!(r#"<p class="dhat-error">⚠ Parse error: {e}</p>"#)
        } else {
            String::new()
        };

        // Build the allocation-sites table
        let mut rows = String::new();
        for (i, site) in s.top_sites.iter().enumerate() {
            let stack_html = Self::render_stack(&site.frames);
            let peak_pct = if s.total_bytes > 0 {
                site.peak_bytes as f64 / s.total_bytes as f64 * 100.0
            } else {
                0.0
            };
            rows.push_str(&format!(
                r#"<tr>
  <td style="color:#8b949e;text-align:center">{rank}</td>
  <td><span class="dhat-peak">{peak}</span><br><small style="color:#8b949e">{pct:.1}% of total</small></td>
  <td class="dhat-total">{total}</td>
  <td style="font-size:12px">{read} / {written}</td>
  <td style="font-size:12px;color:{never_color}">{never}</td>
  <td>{stack}</td>
</tr>"#,
                rank = i + 1,
                peak = fmt_bytes(site.peak_bytes),
                pct = peak_pct,
                total = fmt_bytes(site.total_bytes),
                read = fmt_bytes(site.bytes_read),
                written = fmt_bytes(site.bytes_written),
                never_color = if site.bytes_never_accessed > 0 { "#f85149" } else { "#8b949e" },
                never = fmt_bytes(site.bytes_never_accessed),
                stack = stack_html,
            ));
        }

        let table_html = if s.top_sites.is_empty() {
            r#"<p style="color:#8b949e;font-size:13px;padding:12px">No allocation sites recorded.</p>"#.to_string()
        } else {
            format!(
                r#"<table>
<thead><tr>
  <th>#</th>
  <th>Peak Live Bytes</th>
  <th>Total Allocated</th>
  <th>Read / Written</th>
  <th>Never Accessed</th>
  <th>Call Stack (innermost → outermost)</th>
</tr></thead>
<tbody>{rows}</tbody>
</table>"#
            )
        };

        format!(
            r##"<h2>{label} Heap Profile (DHAT)</h2>
<div class="dhat-section">
  <p class="dhat-filepath">
    Output: <code>{path_str}</code>
    &nbsp;—&nbsp; open in
    <a href="https://nnethercote.github.io/dh_view/dh_view.html" target="_blank" rel="noopener">DHAT Viewer ↗</a>
    (load the JSON file from disk)
  </p>
  {error_html}
  <div class="summary">{cards}</div>
  <h3>Top Allocation Sites by Peak Live Bytes</h3>
  <p class="dhat-hint">
    Sorted by the maximum number of bytes live at any one time from each unique call site.
    These are the allocations most responsible for peak heap usage.
    The "never accessed" column highlights wasteful allocations (allocated but never read or written).
  </p>
  {table_html}
</div>
"##
        )
    }

    /// Renders a DHAT call-stack as HTML.
    /// Shows the two most specific frames inline and the rest in a collapsible.
    fn render_stack(frames: &[String]) -> String {
        if frames.is_empty() {
            return r#"<span style="color:#484f58">—</span>"#.to_string();
        }

        // Strip the leading hex address from Valgrind frame strings like
        // "0x4C3086B: malloc (vg_replace_malloc.c:381)" → "malloc (vg_replace_malloc.c:381)"
        let clean: Vec<String> = frames
            .iter()
            .map(|f| {
                if let Some(colon_pos) = f.find(": ") {
                    // Only strip if the part before the colon looks like a hex address
                    let prefix = &f[..colon_pos];
                    if prefix.starts_with("0x")
                        && prefix[2..].chars().all(|c| c.is_ascii_hexdigit())
                    {
                        return f[colon_pos + 2..].to_string();
                    }
                }
                f.clone()
            })
            .collect();

        let mut html = String::from(r#"<div class="dhat-stack">"#);

        // First two frames always visible
        let visible = clean.len().min(2);
        for (i, frame) in clean[..visible].iter().enumerate() {
            let escaped = frame
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            html.push_str(&format!(r#"<div class="frame-{i}">{escaped}</div>"#));
        }

        // Remaining frames in a <details>
        if clean.len() > 2 {
            html.push_str(&format!(
                r#"<details><summary>+{} more frames</summary>"#,
                clean.len() - 2
            ));
            for (i, frame) in clean[2..].iter().enumerate() {
                let escaped = frame
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;");
                html.push_str(&format!(r#"<div class="frame-{}">{escaped}</div>"#, i + 2));
            }
            html.push_str("</details>");
        }

        html.push_str("</div>");
        html
    }
}
