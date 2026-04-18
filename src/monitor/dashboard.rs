// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

pub const DASHBOARD_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>BilbyCast Edge Monitor</title>
<style>
*{margin:0;padding:0;box-sizing:border-box}
body{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,monospace;background:#0d1117;color:#c9d1d9;min-height:100vh}
a{color:#58a6ff;text-decoration:none}
header{background:#161b22;border-bottom:1px solid #30363d;padding:12px 24px;display:flex;align-items:center;justify-content:space-between}
header h1{font-size:18px;font-weight:600;color:#f0f6fc}
header .meta{font-size:13px;color:#8b949e}
.system-bar{display:flex;gap:24px;padding:16px 24px;background:#161b22;border-bottom:1px solid #30363d;flex-wrap:wrap}
.stat-box{display:flex;flex-direction:column;gap:2px}
.stat-box .label{font-size:11px;color:#8b949e;text-transform:uppercase;letter-spacing:0.5px}
.stat-box .value{font-size:20px;font-weight:600;color:#f0f6fc}
.container{padding:24px;display:grid;grid-template-columns:repeat(auto-fill,minmax(480px,1fr));gap:16px}
.flow-card{background:#161b22;border:1px solid #30363d;border-radius:8px;overflow:hidden}
.flow-header{padding:12px 16px;display:flex;align-items:center;justify-content:space-between;border-bottom:1px solid #21262d}
.flow-name{font-size:15px;font-weight:600;color:#f0f6fc}
.badge{padding:2px 8px;border-radius:12px;font-size:11px;font-weight:600;text-transform:uppercase}
.badge-running{background:#0d419d;color:#58a6ff}
.badge-idle{background:#272c33;color:#8b949e}
.badge-starting{background:#4d2d00;color:#d29922}
.badge-error{background:#5c1a1a;color:#f85149}
.badge-stopped{background:#272c33;color:#8b949e}
.flow-viz{padding:12px 16px;border-bottom:1px solid #21262d}
.flow-viz canvas{display:block;width:100%}
.section{padding:12px 16px}
.section-title{font-size:11px;color:#8b949e;text-transform:uppercase;letter-spacing:0.5px;margin-bottom:8px}
.stats-grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(120px,1fr));gap:8px}
.stat{display:flex;flex-direction:column}
.stat .k{font-size:11px;color:#8b949e}
.stat .v{font-size:14px;color:#c9d1d9;font-weight:500}
table{width:100%;border-collapse:collapse;font-size:13px}
th{text-align:left;color:#8b949e;font-weight:500;padding:4px 8px;font-size:11px;text-transform:uppercase}
td{padding:4px 8px;color:#c9d1d9;border-top:1px solid #21262d}
.srt-details{margin-top:8px;padding:8px 12px;background:#0d1117;border-radius:4px;font-size:12px}
.srt-details .row{display:flex;gap:16px;flex-wrap:wrap}
.srt-details .item{color:#8b949e}
.srt-details .item span{color:#c9d1d9;font-weight:500}
.no-flows{text-align:center;padding:48px 24px;color:#8b949e;font-size:15px}
.error-banner{background:#5c1a1a;color:#f85149;padding:8px 24px;font-size:13px;text-align:center;display:none}
.thumb-wrap{position:relative;width:160px;height:90px;flex-shrink:0;background:#0d1117;border-radius:4px;overflow:hidden}
.thumb-wrap img{width:100%;height:100%;object-fit:cover;display:block}
.thumb-wrap img.hidden{display:none}
.thumb-overlay{position:absolute;inset:0;display:flex;align-items:center;justify-content:center;font-size:11px;font-weight:600;text-transform:uppercase;letter-spacing:0.5px;pointer-events:none}
.thumb-overlay[data-alarm="no-signal"]{background:rgba(100,116,139,0.75);color:#e2e8f0}
.thumb-overlay[data-alarm="frozen"]{background:rgba(59,130,246,0.55);color:#fff}
.thumb-overlay[data-alarm="black"]{background:rgba(0,0,0,0.70);color:#94a3b8}
.thumb-overlay[data-alarm="stopped"]{background:rgba(100,116,139,0.80);color:#cbd5e1}
.tunnel-section{padding:0 24px 16px;display:none}
.tunnel-section.visible{display:block}
.tunnel-grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(320px,1fr));gap:12px}
.tunnel-card{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:12px 16px}
.tunnel-header{display:flex;align-items:center;justify-content:space-between;margin-bottom:8px}
.tunnel-name{font-size:14px;font-weight:600;color:#f0f6fc}
.tunnel-mode{font-size:11px;color:#8b949e;text-transform:uppercase}
.inventory-section{padding:0 24px 16px;display:none}
.inventory-section.visible{display:block}
.inventory-grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(340px,1fr));gap:12px}
.inv-card{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:12px 16px}
.inv-card-header{display:flex;align-items:center;justify-content:space-between;margin-bottom:8px}
.inv-card-name{font-size:14px;font-weight:600;color:#f0f6fc}
.inv-card-type{font-size:11px;color:#8b949e;text-transform:uppercase;letter-spacing:0.5px}
.inv-badge-unassigned{background:rgba(139,148,158,0.15);color:#8b949e;padding:2px 8px;border-radius:12px;font-size:10px;font-weight:600}
.inv-badge-assigned{background:rgba(88,166,255,0.12);color:#58a6ff;padding:2px 8px;border-radius:12px;font-size:10px;font-weight:600;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;max-width:180px}
</style>
</head>
<body>
<header>
  <h1>BilbyCast Edge Monitor</h1>
  <div class="meta" id="version"></div>
</header>
<div class="error-banner" id="error-banner">Connection lost - retrying...</div>
<div class="system-bar" id="system-bar"></div>
<div class="tunnel-section" id="tunnel-section">
  <div style="font-size:11px;color:#8b949e;text-transform:uppercase;letter-spacing:0.5px;margin-bottom:8px;padding:0">IP Tunnels</div>
  <div class="tunnel-grid" id="tunnel-grid"></div>
</div>
<div class="inventory-section" id="inputs-section">
  <div style="font-size:11px;color:#8b949e;text-transform:uppercase;letter-spacing:0.5px;margin-bottom:8px;padding:0">Inputs</div>
  <div class="inventory-grid" id="inputs-grid"></div>
</div>
<div class="inventory-section" id="outputs-section">
  <div style="font-size:11px;color:#8b949e;text-transform:uppercase;letter-spacing:0.5px;margin-bottom:8px;padding:0">Outputs</div>
  <div class="inventory-grid" id="outputs-grid"></div>
</div>
<div class="container" id="flows"></div>
<script>
const REFRESH_MS = 1500;
let errorCount = 0;

// ── Flow visualization state ──
const vizStates = new Map();

const C_GREEN = '#3fb950';
const C_AMBER = '#d29922';
const C_RED = '#f85149';
const C_BLUE = '#58a6ff';
const C_GRAY = '#8b949e';
const C_TEXT = '#c9d1d9';
const C_TEXT_DIM = '#8b949e';
const C_BG = '#0d1117';
const C_NODE_BG = '#161b22';

function healthColor(state, dropped, srtStats) {
  if (!state) return C_GRAY;
  const s = String(state).toLowerCase();
  if (s.includes('error') || s === 'broken') return C_RED;
  if (s === 'connecting' || s === 'starting') return C_BLUE;
  if (s === 'idle' || s === 'stopped') return C_GRAY;
  if ((dropped && dropped > 0) || (srtStats && srtStats.pkt_loss_total > 0)) return C_AMBER;
  return C_GREEN;
}

function inputHealthColor(inp, flowData) {
  if (!inp || !inp.state) return C_GRAY;
  const s = String(inp.state).toLowerCase();
  if (s.includes('error') || s === 'broken') return C_RED;
  if (s === 'connecting' || s === 'starting') return C_BLUE;
  if (s === 'idle' || s === 'stopped' || s === 'waiting') return C_GRAY;
  if (flowData && flowData.bandwidth_blocked) return C_RED;
  if (flowData && flowData.bandwidth_exceeded) return C_AMBER;
  if (inp.packets_lost > 0) return C_AMBER;
  return C_GREEN;
}

function computeLayout(w, h, numOutputs, dualInput) {
  const nodeW = Math.min(w * 0.18, 110);
  const nodeH = 36;
  const hubX = w * 0.38;
  const hubY = h / 2;
  const inX = w * 0.08;
  const outX = w - w * 0.08 - nodeW;
  const outputs = [];
  // Dual input legs for 2022-7 redundancy
  let inputs = [];
  if (dualInput) {
    const gap = nodeH + 12;
    inputs = [
      { x: inX, y: hubY - gap / 2 },
      { x: inX, y: hubY + gap / 2 }
    ];
  } else {
    inputs = [{ x: inX, y: hubY }];
  }
  const inY = dualInput ? hubY : hubY;
  if (numOutputs > 0) {
    const totalH = numOutputs * (nodeH + 12) - 12;
    const startY = (h - totalH) / 2 + nodeH / 2;
    for (let i = 0; i < numOutputs; i++) {
      outputs.push({ x: outX, y: startY + i * (nodeH + 12) });
    }
  }
  return { inX, inY, hubX, hubY, nodeW, nodeH, outputs, inputs };
}

function drawRoundedRect(ctx, x, y, w, h, r, fillColor, strokeColor) {
  ctx.beginPath();
  ctx.moveTo(x + r, y);
  ctx.lineTo(x + w - r, y);
  ctx.quadraticCurveTo(x + w, y, x + w, y + r);
  ctx.lineTo(x + w, y + h - r);
  ctx.quadraticCurveTo(x + w, y + h, x + w - r, y + h);
  ctx.lineTo(x + r, y + h);
  ctx.quadraticCurveTo(x, y + h, x, y + h - r);
  ctx.lineTo(x, y + r);
  ctx.quadraticCurveTo(x, y, x + r, y);
  ctx.closePath();
  if (fillColor) { ctx.fillStyle = fillColor; ctx.fill(); }
  if (strokeColor) { ctx.strokeStyle = strokeColor; ctx.lineWidth = 1.5; ctx.stroke(); }
}

function drawNode(ctx, x, y, w, h, label, sublabel, color) {
  const r = 6;
  // Fill with dark translucent version of color
  const rgb = hexToRgb(color);
  drawRoundedRect(ctx, x, y - h/2, w, h, r, 'rgba('+rgb.r+','+rgb.g+','+rgb.b+',0.12)', color);
  // Status dot
  ctx.beginPath();
  ctx.arc(x + 10, y, 3.5, 0, Math.PI * 2);
  ctx.fillStyle = color;
  ctx.fill();
  // Label
  ctx.fillStyle = C_TEXT;
  ctx.font = '600 12px -apple-system,BlinkMacSystemFont,monospace';
  ctx.textBaseline = 'middle';
  ctx.textAlign = 'left';
  ctx.fillText(label, x + 20, sublabel ? y - 6 : y);
  if (sublabel) {
    ctx.fillStyle = C_TEXT_DIM;
    ctx.font = '10px -apple-system,BlinkMacSystemFont,monospace';
    ctx.fillText(sublabel, x + 20, y + 8);
  }
}

function drawHub(ctx, x, y, color) {
  ctx.beginPath();
  ctx.arc(x, y, 5, 0, Math.PI * 2);
  const rgb = hexToRgb(color);
  ctx.fillStyle = 'rgba('+rgb.r+','+rgb.g+','+rgb.b+',0.3)';
  ctx.fill();
  ctx.strokeStyle = color;
  ctx.lineWidth = 1.5;
  ctx.stroke();
}

function drawConnection(ctx, x1, y1, x2, y2, color) {
  ctx.beginPath();
  ctx.moveTo(x1, y1);
  // Bezier curve for smooth routing
  const cpx = x1 + (x2 - x1) * 0.5;
  ctx.bezierCurveTo(cpx, y1, cpx, y2, x2, y2);
  const rgb = hexToRgb(color);
  ctx.strokeStyle = 'rgba('+rgb.r+','+rgb.g+','+rgb.b+',0.35)';
  ctx.lineWidth = 2;
  ctx.stroke();
}

function bezierPoint(x1, y1, x2, y2, t) {
  const cpx = x1 + (x2 - x1) * 0.5;
  const u = 1 - t;
  const x = u*u*u*x1 + 3*u*u*t*cpx + 3*u*t*t*cpx + t*t*t*x2;
  const y = u*u*u*y1 + 3*u*u*t*y1 + 3*u*t*t*y2 + t*t*t*y2;
  return { x, y };
}

function drawParticle(ctx, x, y, color) {
  const rgb = hexToRgb(color);
  // Glow
  ctx.beginPath();
  ctx.arc(x, y, 6, 0, Math.PI * 2);
  ctx.fillStyle = 'rgba('+rgb.r+','+rgb.g+','+rgb.b+',0.15)';
  ctx.fill();
  // Core
  ctx.beginPath();
  ctx.arc(x, y, 2.5, 0, Math.PI * 2);
  ctx.fillStyle = color;
  ctx.fill();
}

function drawBitrateLabel(ctx, x, y, bps, limitMbps, exceeded) {
  const text = fmt_bitrate(bps);
  var label = text;
  if (limitMbps) {
    label = text + ' / ' + limitMbps.toFixed(1) + ' Mbps';
  }
  ctx.fillStyle = exceeded ? C_AMBER : C_TEXT_DIM;
  ctx.font = '10px -apple-system,BlinkMacSystemFont,monospace';
  ctx.textAlign = 'center';
  ctx.textBaseline = 'bottom';
  ctx.fillText(label, x, y - 4);
}

function hexToRgb(hex) {
  const r = parseInt(hex.slice(1, 3), 16);
  const g = parseInt(hex.slice(3, 5), 16);
  const b = parseInt(hex.slice(5, 7), 16);
  return { r, g, b };
}

function updateVizData(flowId, flowData) {
  let vs = vizStates.get(flowId);
  if (!vs) {
    vs = { canvas: null, ctx: null, particles: new Map(), data: null, lastResize: 0 };
    vizStates.set(flowId, vs);
  }
  vs.data = flowData;
  // Ensure particle arrays match outputs
  const outs = flowData.outputs || [];
  // Input-to-hub particles
  if (!vs.particles.has('_input')) {
    vs.particles.set('_input', []);
  }
  for (const o of outs) {
    const key = o.output_id || o.output_name;
    if (!vs.particles.has(key)) {
      vs.particles.set(key, []);
    }
  }
  // Remove particles for removed outputs
  for (const key of vs.particles.keys()) {
    if (key === '_input') continue;
    if (!outs.find(o => (o.output_id || o.output_name) === key)) {
      vs.particles.delete(key);
    }
  }
}

function initVizCanvas(flowId) {
  const vs = vizStates.get(flowId);
  if (!vs) return;
  const container = document.getElementById('viz-' + flowId);
  if (!container) return;
  const dpr = window.devicePixelRatio || 1;
  const w = container.clientWidth;
  const numOuts = (vs.data && vs.data.outputs) ? vs.data.outputs.length : 1;
  const inp = vs.data ? vs.data.input : null;
  const dualInput = inp && (inp.srt_leg2_stats || inp.redundancy_switches);
  const minNodes = Math.max(numOuts, dualInput ? 2 : 1);
  const h = Math.max(100, Math.min(minNodes * 50 + 40, 350));
  let canvas = container.querySelector('canvas');
  if (!canvas) {
    canvas = document.createElement('canvas');
    container.appendChild(canvas);
  }
  canvas.width = w * dpr;
  canvas.height = h * dpr;
  canvas.style.width = w + 'px';
  canvas.style.height = h + 'px';
  const ctx = canvas.getContext('2d');
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  vs.canvas = canvas;
  vs.ctx = ctx;
  vs.w = w;
  vs.h = h;
}

// Approximate bezier arc length by sampling points
function bezierLength(x1, y1, x2, y2, steps) {
  steps = steps || 16;
  let len = 0;
  let prev = bezierPoint(x1, y1, x2, y2, 0);
  for (let i = 1; i <= steps; i++) {
    const cur = bezierPoint(x1, y1, x2, y2, i / steps);
    const dx = cur.x - prev.x, dy = cur.y - prev.y;
    len += Math.sqrt(dx * dx + dy * dy);
    prev = cur;
  }
  return len;
}

function updateParticles(particles, bps, dt, linePixelLen) {
  const bpsMb = (bps || 0) / 1e6;
  // Only animate when there is actual bitrate
  const hasActivity = bpsMb > 0;
  // 1-6 particles scaling with bitrate (1 at low rates, up to 6 at 100+ Mbps)
  const targetCount = hasActivity ? Math.max(1, Math.min(Math.ceil(bpsMb / 20) + 1, 6)) : 0;
  // Target pixel speed: 40-120 px/sec regardless of line length
  const pxPerSec = hasActivity ? 40 + Math.min(bpsMb / 100, 1.0) * 80 : 0;
  // Convert to t-units/sec using line length
  const safeLen = Math.max(linePixelLen || 200, 50);
  const speed = pxPerSec / safeLen;
  // Add/remove particles to match target
  while (particles.length < targetCount) {
    particles.push({ t: Math.random() });
  }
  while (particles.length > targetCount) {
    particles.pop();
  }
  // Advance
  for (const p of particles) {
    p.t += speed * dt;
    if (p.t > 1) p.t -= 1;
  }
}

let lastFrameTime = 0;

function animateAll(timestamp) {
  const dt = lastFrameTime ? (timestamp - lastFrameTime) / 1000 : 0.016;
  lastFrameTime = timestamp;

  for (const [flowId, vs] of vizStates) {
    if (!vs.canvas || !vs.ctx || !vs.data) continue;
    if (!vs.canvas.isConnected) continue;

    const ctx = vs.ctx;
    const w = vs.w;
    const h = vs.h;
    const inp = vs.data.input || {};
    const outs = vs.data.outputs || [];
    const dualInput = !!(inp.srt_leg2_stats || inp.redundancy_switches);
    const layout = computeLayout(w, h, outs.length, dualInput);

    ctx.clearRect(0, 0, w, h);

    const inpColor = inputHealthColor(inp, vs.data);
    const inpLabel = (inp.input_type || 'INPUT').toUpperCase();

    if (dualInput) {
      // Draw two input leg nodes
      const leg1Color = inp.srt_stats ? healthColor(inp.srt_stats.state, 0, inp.srt_stats) : inpColor;
      const leg2Color = inp.srt_leg2_stats ? healthColor(inp.srt_leg2_stats.state, 0, inp.srt_leg2_stats) : C_GRAY;
      const il1 = layout.inputs[0];
      const il2 = layout.inputs[1];
      drawNode(ctx, il1.x, il1.y, layout.nodeW, layout.nodeH, inpLabel, 'Leg 1', leg1Color);
      drawNode(ctx, il2.x, il2.y, layout.nodeW, layout.nodeH, inpLabel, 'Leg 2', leg2Color);

      if (outs.length === 0) { requestAnimationFrame(animateAll); return; }

      // Draw hub
      drawHub(ctx, layout.hubX, layout.hubY, inpColor);

      // Draw leg1-to-hub and leg2-to-hub lines
      const inRight = il1.x + layout.nodeW;
      drawConnection(ctx, inRight, il1.y, layout.hubX, layout.hubY, leg1Color);
      drawConnection(ctx, inRight, il2.y, layout.hubX, layout.hubY, leg2Color);

      // Leg 1 particles
      const l1Parts = vs.particles.get('_input') || [];
      const l1Len = bezierLength(inRight, il1.y, layout.hubX, layout.hubY);
      updateParticles(l1Parts, inp.bitrate_bps, dt, l1Len);
      for (const p of l1Parts) {
        const pt = bezierPoint(inRight, il1.y, layout.hubX, layout.hubY, p.t);
        drawParticle(ctx, pt.x, pt.y, leg1Color);
      }

      // Leg 2 particles
      if (!vs.particles.has('_input2')) vs.particles.set('_input2', []);
      const l2Parts = vs.particles.get('_input2');
      const l2Len = bezierLength(inRight, il2.y, layout.hubX, layout.hubY);
      updateParticles(l2Parts, inp.bitrate_bps, dt, l2Len);
      for (const p of l2Parts) {
        const pt = bezierPoint(inRight, il2.y, layout.hubX, layout.hubY, p.t);
        drawParticle(ctx, pt.x, pt.y, leg2Color);
      }

      // Bitrate label between legs (centered)
      const midY = (il1.y + il2.y) / 2;
      const inMid = bezierPoint(inRight, midY, layout.hubX, layout.hubY, 0.45);
      drawBitrateLabel(ctx, inMid.x, inMid.y, inp.bitrate_bps, vs.data.bandwidth_limit_mbps, vs.data.bandwidth_exceeded || vs.data.bandwidth_blocked);
    } else {
      // Single input node
      drawNode(ctx, layout.inputs[0].x, layout.inputs[0].y, layout.nodeW, layout.nodeH, inpLabel, inp.state || '', inpColor);

      if (outs.length === 0) { requestAnimationFrame(animateAll); return; }

      // Draw hub
      drawHub(ctx, layout.hubX, layout.hubY, inpColor);

      // Draw input-to-hub line
      const inRight = layout.inputs[0].x + layout.nodeW;
      drawConnection(ctx, inRight, layout.inputs[0].y, layout.hubX, layout.hubY, inpColor);

      // Input particles
      const inpParts = vs.particles.get('_input') || [];
      const inpLineLen = bezierLength(inRight, layout.inputs[0].y, layout.hubX, layout.hubY);
      updateParticles(inpParts, inp.bitrate_bps, dt, inpLineLen);
      for (const p of inpParts) {
        const pt = bezierPoint(inRight, layout.inputs[0].y, layout.hubX, layout.hubY, p.t);
        drawParticle(ctx, pt.x, pt.y, inpColor);
      }

      // Bitrate label on input line
      const inMid = bezierPoint(inRight, layout.inputs[0].y, layout.hubX, layout.hubY, 0.5);
      drawBitrateLabel(ctx, inMid.x, inMid.y, inp.bitrate_bps, vs.data.bandwidth_limit_mbps, vs.data.bandwidth_exceeded || vs.data.bandwidth_blocked);
    }

    // Draw each output
    for (let i = 0; i < outs.length; i++) {
      const o = outs[i];
      const ol = layout.outputs[i];
      const oColor = healthColor(o.state, o.packets_dropped, o.srt_stats);
      const oLabel = (o.output_name || o.output_id || 'OUT').substring(0, 14);
      const oSub = (o.output_type || '').toUpperCase();

      // Hub-to-output line
      drawConnection(ctx, layout.hubX, layout.hubY, ol.x, ol.y, oColor);

      // Output particles
      const key = o.output_id || o.output_name;
      const parts = vs.particles.get(key) || [];
      const oLineLen = bezierLength(layout.hubX, layout.hubY, ol.x, ol.y);
      updateParticles(parts, o.bitrate_bps, dt, oLineLen);
      for (const p of parts) {
        const pt = bezierPoint(layout.hubX, layout.hubY, ol.x, ol.y, p.t);
        drawParticle(ctx, pt.x, pt.y, oColor);
      }

      // Bitrate label
      const oMid = bezierPoint(layout.hubX, layout.hubY, ol.x, ol.y, 0.5);
      drawBitrateLabel(ctx, oMid.x, oMid.y, o.bitrate_bps);

      // Output node
      drawNode(ctx, ol.x, ol.y, layout.nodeW, layout.nodeH, oLabel, oSub, oColor);
    }
  }

  requestAnimationFrame(animateAll);
}

requestAnimationFrame(animateAll);

// ── Formatters ──

function fmt_uptime(s) {
  if (!s && s !== 0) return '-';
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  if (d > 0) return d + 'd ' + h + 'h ' + m + 'm';
  if (h > 0) return h + 'h ' + m + 'm ' + sec + 's';
  return m + 'm ' + sec + 's';
}

function fmt_bitrate(bps) {
  if (!bps) return '0 bps';
  if (bps >= 1e9) return (bps / 1e9).toFixed(2) + ' Gbps';
  if (bps >= 1e6) return (bps / 1e6).toFixed(2) + ' Mbps';
  if (bps >= 1e3) return (bps / 1e3).toFixed(1) + ' Kbps';
  return bps + ' bps';
}

function fmt_bytes(b) {
  if (!b) return '0 B';
  if (b >= 1e12) return (b / 1e12).toFixed(2) + ' TB';
  if (b >= 1e9) return (b / 1e9).toFixed(2) + ' GB';
  if (b >= 1e6) return (b / 1e6).toFixed(1) + ' MB';
  if (b >= 1e3) return (b / 1e3).toFixed(1) + ' KB';
  return b + ' B';
}

function fmt_num(n) {
  if (n === undefined || n === null) return '0';
  return n.toLocaleString();
}

function badge_class(state) {
  if (!state) return 'badge-idle';
  const s = typeof state === 'string' ? state.toLowerCase() : '';
  if (s === 'running') return 'badge-running';
  if (s === 'starting') return 'badge-starting';
  if (s === 'stopped') return 'badge-stopped';
  if (s === 'idle') return 'badge-idle';
  return 'badge-error';
}

function state_label(state) {
  if (!state) return 'Idle';
  if (typeof state === 'object' && state.Error) return 'Error: ' + state.Error;
  return state;
}

function render_srt(srt, label) {
  if (!srt) return '';
  return '<div class="srt-details"><div class="row">' +
    '<div class="item">' + label + ': <span>' + srt.state + '</span></div>' +
    '<div class="item">RTT: <span>' + (srt.rtt_ms || 0).toFixed(1) + ' ms</span></div>' +
    '<div class="item">Loss: <span>' + fmt_num(srt.pkt_loss_total) + '</span></div>' +
    '<div class="item">Retransmit (snd): <span>' + fmt_num(srt.pkt_retransmit_total) + '</span></div>' +
    '<div class="item">Retransmit (rcv): <span>' + fmt_num(srt.pkt_recv_retransmit_total) + '</span></div>' +
    '</div></div>';
}

function renderInputs(inputs) {
  var section = document.getElementById('inputs-section');
  if (!inputs || inputs.length === 0) {
    section.classList.remove('visible');
    return;
  }
  section.classList.add('visible');
  var html = '';
  for (var i = 0; i < inputs.length; i++) {
    var inp = inputs[i];
    var sc = inputHealthColor(inp, null);
    var rgb = hexToRgb(sc);
    html += '<div class="inv-card">';
    html += '<div class="inv-card-header">';
    html += '<span class="inv-card-name">' + esc(inp.input_name || inp.input_id) + '</span>';
    html += '<span style="display:flex;gap:6px;align-items:center">';
    html += '<span class="badge" style="background:rgba(' + rgb.r + ',' + rgb.g + ',' + rgb.b + ',0.15);color:' + sc + '">' + esc(inp.state || 'Unknown') + '</span>';
    html += '<span class="inv-card-type">' + esc(inp.input_type) + '</span>';
    html += '</span></div>';
    if (inp.assigned_flow_name) {
      html += '<div style="margin-bottom:6px"><span class="inv-badge-assigned" title="' + esc(inp.assigned_flow_id) + '">Flow: ' + esc(inp.assigned_flow_name) + '</span></div>';
    } else {
      html += '<div style="margin-bottom:6px"><span class="inv-badge-unassigned">Unassigned</span></div>';
    }
    html += '<div class="stats-grid">';
    html += '<div class="stat"><div class="k">Packets</div><div class="v">' + fmt_num(inp.packets_received) + '</div></div>';
    html += '<div class="stat"><div class="k">Bytes</div><div class="v">' + fmt_bytes(inp.bytes_received) + '</div></div>';
    html += '<div class="stat"><div class="k">Bitrate</div><div class="v">' + fmt_bitrate(inp.bitrate_bps) + '</div></div>';
    html += '<div class="stat"><div class="k">Lost</div><div class="v">' + fmt_num(inp.packets_lost) + '</div></div>';
    if (inp.packets_recovered_fec) {
      html += '<div class="stat"><div class="k">FEC Recovered</div><div class="v">' + fmt_num(inp.packets_recovered_fec) + '</div></div>';
    }
    if (inp.redundancy_switches) {
      html += '<div class="stat"><div class="k">Redundancy Sw.</div><div class="v">' + fmt_num(inp.redundancy_switches) + '</div></div>';
    }
    html += '</div>';
    html += render_srt(inp.srt_stats, 'SRT Leg1');
    html += render_srt(inp.srt_leg2_stats, 'SRT Leg2');
    html += '</div>';
  }
  document.getElementById('inputs-grid').innerHTML = html;
}

function renderOutputs(outputs) {
  var section = document.getElementById('outputs-section');
  if (!outputs || outputs.length === 0) {
    section.classList.remove('visible');
    return;
  }
  section.classList.add('visible');
  var html = '';
  for (var i = 0; i < outputs.length; i++) {
    var o = outputs[i];
    var sc = healthColor(o.state, o.packets_dropped, o.srt_stats);
    var rgb = hexToRgb(sc);
    html += '<div class="inv-card">';
    html += '<div class="inv-card-header">';
    html += '<span class="inv-card-name">' + esc(o.output_name || o.output_id) + '</span>';
    html += '<span style="display:flex;gap:6px;align-items:center">';
    html += '<span class="badge" style="background:rgba(' + rgb.r + ',' + rgb.g + ',' + rgb.b + ',0.15);color:' + sc + '">' + esc(o.state || 'Unknown') + '</span>';
    html += '<span class="inv-card-type">' + esc(o.output_type) + '</span>';
    html += '</span></div>';
    if (o.assigned_flow_name) {
      html += '<div style="margin-bottom:6px"><span class="inv-badge-assigned" title="' + esc(o.assigned_flow_id) + '">Flow: ' + esc(o.assigned_flow_name) + '</span></div>';
    } else {
      html += '<div style="margin-bottom:6px"><span class="inv-badge-unassigned">Unassigned</span></div>';
    }
    html += '<div class="stats-grid">';
    html += '<div class="stat"><div class="k">Packets</div><div class="v">' + fmt_num(o.packets_sent) + '</div></div>';
    html += '<div class="stat"><div class="k">Bytes</div><div class="v">' + fmt_bytes(o.bytes_sent) + '</div></div>';
    html += '<div class="stat"><div class="k">Bitrate</div><div class="v">' + fmt_bitrate(o.bitrate_bps) + '</div></div>';
    html += '<div class="stat"><div class="k">Dropped</div><div class="v">' + fmt_num(o.packets_dropped) + '</div></div>';
    if (o.fec_packets_sent) {
      html += '<div class="stat"><div class="k">FEC Sent</div><div class="v">' + fmt_num(o.fec_packets_sent) + '</div></div>';
    }
    html += '</div>';
    html += render_srt(o.srt_stats, 'SRT Leg1');
    html += render_srt(o.srt_leg2_stats, 'SRT Leg2');
    html += '</div>';
  }
  document.getElementById('outputs-grid').innerHTML = html;
}

function render(data) {
  const sys = data.system;
  document.getElementById('version').textContent = 'v' + sys.version;

  var sysHtml = '<div class="stat-box"><div class="label">Uptime</div><div class="value">' + fmt_uptime(sys.uptime_secs) + '</div></div>' +
    '<div class="stat-box"><div class="label">Active Flows</div><div class="value">' + sys.active_flows + ' / ' + sys.total_flows + '</div></div>';
  if (lastTunnelCount > 0) {
    sysHtml += '<div class="stat-box"><div class="label">Tunnels</div><div class="value">' + lastTunnelCount + '</div></div>';
  }
  sysHtml += '<div class="stat-box"><div class="label">Version</div><div class="value">' + sys.version + '</div></div>';
  document.getElementById('system-bar').innerHTML = sysHtml;

  renderInputs(data.inputs || []);
  renderOutputs(data.outputs || []);

  const flows = data.flows || [];
  const container = document.getElementById('flows');

  if (flows.length === 0) {
    container.innerHTML = '<div class="no-flows">No flows configured</div>';
    vizStates.clear();
    return;
  }

  // Update viz data for all flows before rebuilding HTML
  const activeIds = new Set();
  for (const f of flows) {
    const fid = f.flow_id;
    activeIds.add(fid);
    updateVizData(fid, f);
  }
  // Remove stale viz states
  for (const key of vizStates.keys()) {
    if (!activeIds.has(key)) vizStates.delete(key);
  }

  let html = '';
  for (const f of flows) {
    const st = state_label(f.state);
    const bc = badge_class(f.state);
    const inp = f.input || {};
    const outs = f.outputs || [];

    html += '<div class="flow-card">';
    html += '<div class="flow-header" style="gap:12px">';
    // Thumbnail preview
    var thumbInfo = f.thumbnail || {};
    if (thumbInfo.has_thumbnail || thumbInfo.enabled) {
      var thumbUrl = '/api/thumbnail/' + encodeURIComponent(f.flow_id);
      var thumbAlarm = thumbInfo.alarm || (st === 'Stopped' || st === 'Idle' ? 'stopped' : (!thumbInfo.has_thumbnail ? 'no-signal' : ''));
      html += '<div class="thumb-wrap">';
      html += '<img data-thumb-flow="' + esc(f.flow_id) + '" src="' + thumbUrl + '?t=' + Date.now() + '" onerror="this.classList.add(\'hidden\')" onload="this.classList.remove(\'hidden\')">';
      if (thumbAlarm) {
        html += '<div class="thumb-overlay" data-alarm="' + esc(thumbAlarm) + '">' + esc(thumbAlarm.replace('-', ' ')) + '</div>';
      }
      html += '</div>';
    }
    html += '<div style="flex:1;min-width:0">';
    html += '<div style="display:flex;align-items:center;justify-content:space-between">';
    html += '<span class="flow-name">' + esc(f.flow_name || f.flow_id) + '</span>';
    html += '<span style="display:flex;gap:6px;align-items:center">';
    // Health badge (RP 2129 M6)
    var hc = {'Healthy':C_GREEN,'Warning':C_AMBER,'Error':C_RED,'Critical':C_RED}[f.health] || C_GRAY;
    html += '<span class="badge" style="background:rgba(' + hexToRgb(hc).r + ',' + hexToRgb(hc).g + ',' + hexToRgb(hc).b + ',0.15);color:' + hc + '">' + esc(f.health || 'Unknown') + '</span>';
    // Bandwidth limit badge
    if (f.bandwidth_limit_mbps) {
      if (f.bandwidth_blocked) {
        html += '<span class="badge" style="background:rgba(248,81,73,0.15);color:#f85149">BLOCKED: BW ' + fmt_bitrate(f.input.bitrate_bps) + ' / ' + f.bandwidth_limit_mbps.toFixed(1) + ' Mbps</span>';
      } else if (f.bandwidth_exceeded) {
        html += '<span class="badge" style="background:rgba(210,153,34,0.15);color:#d29922">BW EXCEEDED: ' + fmt_bitrate(f.input.bitrate_bps) + ' / ' + f.bandwidth_limit_mbps.toFixed(1) + ' Mbps</span>';
      } else {
        html += '<span class="badge" style="background:rgba(139,148,158,0.08);color:#8b949e">Limit: ' + f.bandwidth_limit_mbps.toFixed(1) + ' Mbps</span>';
      }
    }
    html += '<span class="badge ' + bc + '">' + esc(st) + '</span>';
    html += '</span></div>';
    html += '</div></div>';

    // Flow visualization canvas
    html += '<div class="flow-viz" id="viz-' + esc(f.flow_id) + '"></div>';

    // Input section
    html += '<div class="section"><div class="section-title">Input (' + esc(inp.input_type || 'unknown') + ')</div>';
    html += '<div class="stats-grid">';
    html += '<div class="stat"><div class="k">State</div><div class="v">' + esc(inp.state || '-') + '</div></div>';
    html += '<div class="stat"><div class="k">Packets</div><div class="v">' + fmt_num(inp.packets_received) + '</div></div>';
    html += '<div class="stat"><div class="k">Bytes</div><div class="v">' + fmt_bytes(inp.bytes_received) + '</div></div>';
    html += '<div class="stat"><div class="k">Bitrate</div><div class="v">' + fmt_bitrate(inp.bitrate_bps) + '</div></div>';
    html += '<div class="stat"><div class="k">Lost</div><div class="v">' + fmt_num(inp.packets_lost) + '</div></div>';
    if (inp.packets_recovered_fec) {
      html += '<div class="stat"><div class="k">FEC Recovered</div><div class="v">' + fmt_num(inp.packets_recovered_fec) + '</div></div>';
    }
    if (inp.redundancy_switches) {
      html += '<div class="stat"><div class="k">Redundancy Switches</div><div class="v">' + fmt_num(inp.redundancy_switches) + '</div></div>';
    }
    html += '</div>';
    html += render_srt(inp.srt_stats, 'SRT Leg1');
    html += render_srt(inp.srt_leg2_stats, 'SRT Leg2');
    html += '</div>';

    // TR-101290 section
    if (f.tr101290) {
      const tr = f.tr101290;
      const p1ok = tr.priority1_ok;
      const p2ok = tr.priority2_ok;
      const p1c = p1ok ? C_GREEN : C_RED;
      const p2c = p2ok ? C_GREEN : C_AMBER;
      const ec = function(v) { return v > 0 ? 'color:' + C_RED : ''; };

      html += '<div class="section"><div class="section-title">TR-101290 Analysis</div>';
      html += '<div class="stats-grid">';
      html += '<div class="stat"><div class="k">TS Packets</div><div class="v">' + fmt_num(tr.ts_packets_analyzed) + '</div></div>';
      html += '<div class="stat"><div class="k">Priority 1</div><div class="v" style="color:' + p1c + '">' + (p1ok ? 'OK' : 'ERRORS') + '</div></div>';
      html += '<div class="stat"><div class="k">Priority 2</div><div class="v" style="color:' + p2c + '">' + (p2ok ? 'OK' : 'ERRORS') + '</div></div>';
      html += '<div class="stat"><div class="k">Sync Errors</div><div class="v" style="' + ec(tr.sync_byte_errors) + '">' + fmt_num(tr.sync_byte_errors) + '</div></div>';
      html += '<div class="stat"><div class="k">Sync Loss</div><div class="v" style="' + ec(tr.sync_loss_count) + '">' + fmt_num(tr.sync_loss_count) + '</div></div>';
      html += '<div class="stat"><div class="k">CC Errors</div><div class="v" style="' + ec(tr.cc_errors) + '">' + fmt_num(tr.cc_errors) + '</div></div>';
      html += '<div class="stat"><div class="k">PAT Errors</div><div class="v" style="' + ec(tr.pat_errors) + '">' + fmt_num(tr.pat_errors) + '</div></div>';
      html += '<div class="stat"><div class="k">PMT Errors</div><div class="v" style="' + ec(tr.pmt_errors) + '">' + fmt_num(tr.pmt_errors) + '</div></div>';
      html += '<div class="stat"><div class="k">PID Errors</div><div class="v" style="' + ec(tr.pid_errors) + '">' + fmt_num(tr.pid_errors) + '</div></div>';
      html += '<div class="stat"><div class="k">TEI Errors</div><div class="v" style="' + ec(tr.tei_errors) + '">' + fmt_num(tr.tei_errors) + '</div></div>';
      html += '<div class="stat"><div class="k">CRC Errors</div><div class="v" style="' + ec(tr.crc_errors) + '">' + fmt_num(tr.crc_errors) + '</div></div>';
      html += '<div class="stat"><div class="k">PCR Discont.</div><div class="v" style="' + ec(tr.pcr_discontinuity_errors) + '">' + fmt_num(tr.pcr_discontinuity_errors) + '</div></div>';
      html += '<div class="stat"><div class="k">PCR Accuracy</div><div class="v" style="' + ec(tr.pcr_accuracy_errors) + '">' + fmt_num(tr.pcr_accuracy_errors) + '</div></div>';
      html += '<div class="stat"><div class="k">PATs</div><div class="v">' + fmt_num(tr.pat_count) + '</div></div>';
      html += '<div class="stat"><div class="k">PMTs</div><div class="v">' + fmt_num(tr.pmt_count) + '</div></div>';
      // VSF TR-07 compliance indicator
      if (tr.tr07_compliant) {
        html += '<div class="stat"><div class="k">VSF TR-07</div><div class="v" style="color:' + C_GREEN + '">COMPLIANT (JPEG XS PID 0x' + (tr.jpeg_xs_pid ? tr.jpeg_xs_pid.toString(16).toUpperCase().padStart(4, '0') : '?') + ')</div></div>';
      } else {
        html += '<div class="stat"><div class="k">VSF TR-07</div><div class="v" style="color:' + C_GRAY + '">No JPEG XS</div></div>';
      }
      html += '</div></div>';
    }

    // Media Analysis section
    if (f.media_analysis) {
      const ma = f.media_analysis;
      html += '<div class="section"><div class="section-title">Media Analysis</div>';
      html += '<div class="stats-grid">';
      html += '<div class="stat"><div class="k">Protocol</div><div class="v">' + esc(ma.protocol || '-').toUpperCase() + '</div></div>';
      html += '<div class="stat"><div class="k">Payload</div><div class="v">' + esc(ma.payload_format || '-') + '</div></div>';
      if (ma.fec) {
        html += '<div class="stat"><div class="k">FEC</div><div class="v" style="color:' + C_GREEN + '">' + esc(ma.fec.standard) + ' (L=' + ma.fec.columns + ', D=' + ma.fec.rows + ')</div></div>';
      }
      if (ma.redundancy) {
        html += '<div class="stat"><div class="k">Redundancy</div><div class="v" style="color:' + C_GREEN + '">' + esc(ma.redundancy.standard) + '</div></div>';
      }
      html += '<div class="stat"><div class="k">Programs</div><div class="v">' + (ma.program_count || 0) + '</div></div>';
      html += '<div class="stat"><div class="k">TS Bitrate</div><div class="v">' + fmt_bitrate(ma.total_bitrate_bps) + '</div></div>';
      html += '</div>';
      if (ma.video_streams && ma.video_streams.length > 0) {
        html += '<div style="margin-top:8px"><div style="font-size:11px;font-weight:600;color:#8b949e;margin-bottom:4px;text-transform:uppercase;letter-spacing:0.5px">Video Streams</div>';
        html += '<table><tr><th>PID</th><th>Codec</th><th>Resolution</th><th>Frame Rate</th><th>Profile</th><th>Level</th><th>Bitrate</th></tr>';
        for (const vs of ma.video_streams) {
          html += '<tr>';
          html += '<td>0x' + vs.pid.toString(16).toUpperCase().padStart(4, '0') + '</td>';
          html += '<td>' + esc(vs.codec) + '</td>';
          html += '<td>' + esc(vs.resolution || 'detecting...') + '</td>';
          html += '<td>' + (vs.frame_rate ? vs.frame_rate.toFixed(2) + ' fps' : 'detecting...') + '</td>';
          html += '<td>' + esc(vs.profile || '-') + '</td>';
          html += '<td>' + esc(vs.level || '-') + '</td>';
          html += '<td>' + fmt_bitrate(vs.bitrate_bps) + '</td>';
          html += '</tr>';
        }
        html += '</table></div>';
      }
      if (ma.audio_streams && ma.audio_streams.length > 0) {
        html += '<div style="margin-top:8px"><div style="font-size:11px;font-weight:600;color:#8b949e;margin-bottom:4px;text-transform:uppercase;letter-spacing:0.5px">Audio Streams</div>';
        html += '<table><tr><th>PID</th><th>Codec</th><th>Sample Rate</th><th>Channels</th><th>Language</th><th>Bitrate</th></tr>';
        for (const as1 of ma.audio_streams) {
          html += '<tr>';
          html += '<td>0x' + as1.pid.toString(16).toUpperCase().padStart(4, '0') + '</td>';
          html += '<td>' + esc(as1.codec) + '</td>';
          html += '<td>' + (as1.sample_rate_hz ? as1.sample_rate_hz + ' Hz' : 'detecting...') + '</td>';
          html += '<td>' + (as1.channels != null ? as1.channels + ' ch' : '-') + '</td>';
          html += '<td>' + esc(as1.language || '-') + '</td>';
          html += '<td>' + fmt_bitrate(as1.bitrate_bps) + '</td>';
          html += '</tr>';
        }
        html += '</table></div>';
      }
      html += '</div>';
    }

    // SMPTE Trust Boundary Metrics section
    {
      html += '<div class="section"><div class="section-title">SMPTE Trust Boundary Metrics</div>';
      html += '<div class="stats-grid">';
      // IAT
      if (f.iat) {
        html += '<div class="stat"><div class="k">IAT Avg</div><div class="v">' + f.iat.avg_us.toFixed(0) + ' \u00B5s</div></div>';
        html += '<div class="stat"><div class="k">IAT Min</div><div class="v">' + f.iat.min_us.toFixed(0) + ' \u00B5s</div></div>';
        html += '<div class="stat"><div class="k">IAT Max</div><div class="v">' + f.iat.max_us.toFixed(0) + ' \u00B5s</div></div>';
      } else {
        html += '<div class="stat"><div class="k">IAT</div><div class="v" style="color:' + C_GRAY + '">waiting</div></div>';
      }
      // PDV / Jitter
      if (f.pdv_jitter_us) {
        html += '<div class="stat"><div class="k">Jitter (PDV)</div><div class="v">' + f.pdv_jitter_us.toFixed(1) + ' \u00B5s</div></div>';
      } else {
        html += '<div class="stat"><div class="k">Jitter (PDV)</div><div class="v" style="color:' + C_GRAY + '">waiting</div></div>';
      }
      // Packets filtered
      html += '<div class="stat"><div class="k">Pkts Filtered</div><div class="v">' + fmt_num(inp.packets_filtered) + '</div></div>';
      // Packet rate (packets/sec derived from bitrate and avg packet size)
      var pps = inp.packets_received && f.uptime_secs ? Math.round(inp.packets_received / f.uptime_secs) : 0;
      html += '<div class="stat"><div class="k">Packet Rate</div><div class="v">' + fmt_num(pps) + ' pkt/s</div></div>';
      // Seq gaps (same as lost but shown here for RP 2129 context)
      html += '<div class="stat"><div class="k">Seq Gaps</div><div class="v">' + fmt_num(inp.packets_lost) + '</div></div>';
      html += '</div></div>';
    }

    // Outputs section
    if (outs.length > 0) {
      html += '<div class="section"><div class="section-title">Outputs</div>';
      html += '<table><tr><th>Name</th><th>Type</th><th>State</th><th>Packets</th><th>Bytes</th><th>Bitrate</th><th>Dropped</th><th>FEC Sent</th></tr>';
      for (const o of outs) {
        html += '<tr>';
        var oName = esc(o.output_name || o.output_id);
        if (o.srt_leg2_stats) oName += ' <span style="color:#58a6ff;font-size:10px;font-weight:600">[2022-7]</span>';
        if (o.output_type === 'webrtc') oName += ' <span style="color:#a78bfa;font-size:10px;font-weight:600">[WebRTC]</span>';
        html += '<td>' + oName + '</td>';
        html += '<td>' + esc(o.output_type || '-') + '</td>';
        html += '<td>' + esc(o.state || '-') + '</td>';
        html += '<td>' + fmt_num(o.packets_sent) + '</td>';
        html += '<td>' + fmt_bytes(o.bytes_sent) + '</td>';
        html += '<td>' + fmt_bitrate(o.bitrate_bps) + '</td>';
        html += '<td>' + fmt_num(o.packets_dropped) + '</td>';
        html += '<td>' + fmt_num(o.fec_packets_sent) + '</td>';
        html += '</tr>';
      }
      html += '</table>';
      for (const o of outs) {
        html += render_srt(o.srt_stats, esc(o.output_name || o.output_id) + ' Leg1');
        html += render_srt(o.srt_leg2_stats, esc(o.output_name || o.output_id) + ' Leg2');
      }
      html += '</div>';
    }

    html += '</div>';
  }
  container.innerHTML = html;

  // Initialize canvases after DOM is updated
  for (const f of flows) {
    initVizCanvas(f.flow_id);
  }
}

function esc(s) {
  if (!s) return '';
  return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
}

function tunnelStateColor(state) {
  if (!state) return C_GRAY;
  var s = state.toLowerCase();
  if (s === 'connected' || s === 'active') return C_GREEN;
  if (s === 'connecting' || s === 'binding') return C_BLUE;
  if (s.includes('error')) return C_RED;
  return C_GRAY;
}

var lastTunnelCount = 0;

function renderTunnels(tunnels) {
  var section = document.getElementById('tunnel-section');
  lastTunnelCount = tunnels.length;
  if (!tunnels || tunnels.length === 0) {
    section.classList.remove('visible');
    return;
  }
  section.classList.add('visible');
  var html = '';
  for (var i = 0; i < tunnels.length; i++) {
    var t = tunnels[i];
    var sc = tunnelStateColor(t.state);
    var rgb = hexToRgb(sc);
    html += '<div class="tunnel-card">';
    html += '<div class="tunnel-header">';
    html += '<span class="tunnel-name">' + esc(t.name || t.id) + '</span>';
    html += '<span style="display:flex;gap:6px;align-items:center">';
    html += '<span class="badge" style="background:rgba(' + rgb.r + ',' + rgb.g + ',' + rgb.b + ',0.15);color:' + sc + '">' + esc(t.state || 'unknown') + '</span>';
    html += '<span class="tunnel-mode">' + esc(t.mode) + ' / ' + esc(t.direction) + '</span>';
    html += '</span></div>';
    html += '<div class="stats-grid">';
    html += '<div class="stat"><div class="k">Protocol</div><div class="v">' + esc(t.protocol || '-').toUpperCase() + '</div></div>';
    html += '<div class="stat"><div class="k">Local Addr</div><div class="v">' + esc(t.local_addr || '-') + '</div></div>';
    if (t.stats) {
      html += '<div class="stat"><div class="k">In Bitrate</div><div class="v">' + fmt_bitrate(t.stats.bitrate_in_bps) + '</div></div>';
      html += '<div class="stat"><div class="k">Out Bitrate</div><div class="v">' + fmt_bitrate(t.stats.bitrate_out_bps) + '</div></div>';
      html += '<div class="stat"><div class="k">Pkts Sent</div><div class="v">' + fmt_num(t.stats.packets_sent) + '</div></div>';
      html += '<div class="stat"><div class="k">Pkts Recv</div><div class="v">' + fmt_num(t.stats.packets_received) + '</div></div>';
      html += '<div class="stat"><div class="k">Bytes Sent</div><div class="v">' + fmt_bytes(t.stats.bytes_sent) + '</div></div>';
      html += '<div class="stat"><div class="k">Bytes Recv</div><div class="v">' + fmt_bytes(t.stats.bytes_received) + '</div></div>';
      if (t.stats.send_errors > 0) {
        html += '<div class="stat"><div class="k">Send Errors</div><div class="v" style="color:' + C_RED + '">' + fmt_num(t.stats.send_errors) + '</div></div>';
      }
      html += '<div class="stat"><div class="k">Connections</div><div class="v">' + t.stats.connections_active + ' / ' + t.stats.connections_total + '</div></div>';
    }
    html += '</div></div>';
  }
  document.getElementById('tunnel-grid').innerHTML = html;
}

// Handle window resize — reinitialize canvases
let resizeTimer = null;
window.addEventListener('resize', function() {
  clearTimeout(resizeTimer);
  resizeTimer = setTimeout(function() {
    for (const [flowId] of vizStates) {
      initVizCanvas(flowId);
    }
  }, 150);
});

// ── Tunnel polling (slower interval, not in WebSocket stream) ──
async function pollTunnels() {
  try {
    const res = await fetch('/api/tunnels');
    if (res.ok) renderTunnels(await res.json());
  } catch (e) { /* ignore */ }
}

// ── Stats: HTTP polling fallback ──
async function pollStats() {
  try {
    const res = await fetch('/api/stats');
    if (!res.ok) throw new Error('HTTP ' + res.status);
    render(await res.json());
    errorCount = 0;
    document.getElementById('error-banner').style.display = 'none';
  } catch (e) {
    errorCount++;
    if (errorCount > 2) {
      document.getElementById('error-banner').style.display = 'block';
    }
  }
}

// ── WebSocket with HTTP fallback ──
let ws = null;
let wsRetries = 0;
let httpInterval = null;

function startWebSocket() {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  ws = new WebSocket(proto + '//' + location.host + '/api/ws');

  ws.onopen = function() {
    wsRetries = 0;
    // Stop HTTP polling when WS is active
    if (httpInterval) { clearInterval(httpInterval); httpInterval = null; }
    document.getElementById('error-banner').style.display = 'none';
    errorCount = 0;
  };

  ws.onmessage = function(event) {
    try {
      const data = JSON.parse(event.data);
      render(data);
      document.getElementById('error-banner').style.display = 'none';
      errorCount = 0;
    } catch (e) { /* ignore parse errors */ }
  };

  ws.onclose = function() {
    ws = null;
    wsRetries++;
    if (wsRetries > 5) {
      // Give up on WebSocket, fall back to HTTP polling
      if (!httpInterval) {
        httpInterval = setInterval(pollStats, REFRESH_MS);
      }
    } else {
      // Retry WebSocket with backoff
      setTimeout(startWebSocket, Math.min(1000 * wsRetries, 5000));
    }
  };

  ws.onerror = function() {
    // onclose will fire after onerror
  };
}

// Initial load via HTTP (get data immediately)
pollStats();
pollTunnels();

// Start WebSocket for real-time stats
startWebSocket();

// Tunnel polling at a slower interval (not in WS stream)
setInterval(pollTunnels, 5000);

// Refresh thumbnails every 12 seconds (independent of stats)
setInterval(function() {
  document.querySelectorAll('img[data-thumb-flow]').forEach(function(img) {
    var src = img.getAttribute('src').split('?')[0];
    img.setAttribute('src', src + '?t=' + Date.now());
  });
}, 12000);
</script>
</body>
</html>"##;
