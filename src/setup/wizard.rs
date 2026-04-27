// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

pub const SETUP_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>BilbyCast Edge Setup</title>
<style>
*{margin:0;padding:0;box-sizing:border-box}
body{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,monospace;background:#0d1117;color:#c9d1d9;min-height:100vh;display:flex;justify-content:center;padding:40px 16px}
.card{background:#161b22;border:1px solid #30363d;border-radius:12px;max-width:520px;width:100%;padding:32px}
h1{font-size:20px;font-weight:600;color:#f0f6fc;margin-bottom:4px}
.subtitle{font-size:13px;color:#8b949e;margin-bottom:24px}
label{display:block;font-size:13px;color:#c9d1d9;margin-bottom:4px;font-weight:500}
.hint{font-size:11px;color:#8b949e;margin-bottom:12px}
input[type="text"],input[type="number"],input[type="password"]{width:100%;padding:8px 12px;background:#0d1117;border:1px solid #30363d;border-radius:6px;color:#f0f6fc;font-size:14px;font-family:inherit;margin-bottom:4px}
input[type="text"]:focus,input[type="number"]:focus,input[type="password"]:focus{outline:none;border-color:#58a6ff}
.checkbox-row{display:flex;align-items:center;gap:8px;margin-bottom:4px}
.checkbox-row input[type="checkbox"]{accent-color:#58a6ff}
.row{display:flex;gap:12px;margin-bottom:0}
.row .col{flex:1}
.section{margin-bottom:20px}
.section-title{font-size:11px;color:#8b949e;text-transform:uppercase;letter-spacing:0.5px;margin-bottom:12px;padding-bottom:6px;border-bottom:1px solid #21262d}
button{width:100%;padding:10px;background:#238636;border:none;border-radius:6px;color:#fff;font-size:14px;font-weight:600;cursor:pointer;font-family:inherit;margin-top:8px}
button:hover{background:#2ea043}
button:disabled{background:#21262d;color:#484f58;cursor:not-allowed}
.msg{margin-top:16px;padding:12px;border-radius:6px;font-size:13px;display:none}
.msg.ok{display:block;background:#0d2818;border:1px solid #238636;color:#3fb950}
.msg.err{display:block;background:#3d1114;border:1px solid #da3633;color:#f85149}
.loading{text-align:center;color:#8b949e;padding:48px 0}
</style>
</head>
<body>
<div class="card">
  <h1>BilbyCast Edge Setup</h1>
  <p class="subtitle">Configure this edge node for network and manager connectivity.</p>

  <div id="loading" class="loading">Loading current configuration...</div>

  <form id="form" style="display:none" onsubmit="return submitForm(event)">
    <div class="section">
      <div class="section-title">Authentication</div>
      <label for="setup_token">Setup Token</label>
      <div class="hint">
        Required from the LAN or internet. Loopback callers
        (localhost / 127.0.0.1 / ::1) bypass this. Get the token from
        the node's first-boot stdout banner, or run
        <code>bilbycast-edge --print-setup-token</code> on the node.
      </div>
      <input type="password" id="setup_token" maxlength="128" placeholder="Required from LAN/internet">
    </div>

    <div class="section">
      <div class="section-title">Device</div>
      <label for="device_name">Device Name</label>
      <div class="hint">Optional label for this edge node (e.g. "Studio-A Encoder")</div>
      <input type="text" id="device_name" maxlength="256" placeholder="My Edge Node">
    </div>

    <div class="section">
      <div class="section-title">API Server</div>
      <div class="row">
        <div class="col">
          <label for="listen_addr">Listen Address</label>
          <div class="hint">Bind address for the API server</div>
          <input type="text" id="listen_addr" value="0.0.0.0" required>
        </div>
        <div class="col">
          <label for="listen_port">Port</label>
          <div class="hint">API server port</div>
          <input type="number" id="listen_port" min="1" max="65535" value="8080" required>
        </div>
      </div>
    </div>

    <div class="section">
      <div class="section-title">Manager Connection</div>
      <label for="manager_urls">Manager URLs (one per line)</label>
      <div class="hint">
        One or more WebSocket URLs of bilbycast-manager instances
        (each must start with wss://). The edge tries them in order
        and rotates on connection close — use 2–4 entries for an
        active/active cluster, a single entry for a solo manager.
      </div>
      <textarea id="manager_urls" rows="4" placeholder="wss://manager-a.example.com:8443/ws/node&#10;wss://manager-b.example.com:8443/ws/node" required style="width:100%;box-sizing:border-box;font-family:inherit;color:inherit;background:#0d1117;border:1px solid #30363d;border-radius:6px;padding:8px"></textarea>

      <label for="registration_token" style="margin-top:12px">Registration Token</label>
      <div class="hint">One-time token for registering with the manager (optional)</div>
      <input type="text" id="registration_token" maxlength="4096" placeholder="Token from manager admin">

      <div class="checkbox-row" style="margin-top:12px">
        <input type="checkbox" id="accept_self_signed_cert">
        <label for="accept_self_signed_cert" style="margin-bottom:0">Accept self-signed certificates</label>
      </div>
      <div class="hint">Enable for development/testing only</div>
    </div>

    <button type="submit" id="submit-btn">Save Configuration</button>
  </form>

  <div id="msg" class="msg"></div>
</div>

<script>
(function() {
  var form = document.getElementById('form');
  var loading = document.getElementById('loading');
  var msg = document.getElementById('msg');

  fetch('/setup/status')
    .then(function(r) { return r.json(); })
    .then(function(data) {
      loading.style.display = 'none';
      form.style.display = 'block';
      if (data.listen_addr) document.getElementById('listen_addr').value = data.listen_addr;
      if (data.listen_port) document.getElementById('listen_port').value = data.listen_port;
      if (data.manager_urls && data.manager_urls.length > 0) {
        document.getElementById('manager_urls').value = data.manager_urls.join('\n');
      }
      if (data.accept_self_signed_cert) document.getElementById('accept_self_signed_cert').checked = true;
      if (data.registration_token) document.getElementById('registration_token').value = data.registration_token;
      if (data.device_name) document.getElementById('device_name').value = data.device_name;
    })
    .catch(function() {
      loading.style.display = 'none';
      form.style.display = 'block';
    });
})();

function submitForm(e) {
  e.preventDefault();
  var btn = document.getElementById('submit-btn');
  btn.disabled = true;
  btn.textContent = 'Saving...';
  msg.className = 'msg';
  msg.style.display = 'none';

  var payload = {
    listen_addr: document.getElementById('listen_addr').value.trim(),
    listen_port: parseInt(document.getElementById('listen_port').value, 10),
    manager_urls: document.getElementById('manager_urls').value
      .split(/\r?\n|,/)
      .map(function(s) { return s.trim(); })
      .filter(function(s) { return s.length > 0; }),
    accept_self_signed_cert: document.getElementById('accept_self_signed_cert').checked,
    registration_token: document.getElementById('registration_token').value.trim() || null,
    device_name: document.getElementById('device_name').value.trim() || null
  };

  var token = document.getElementById('setup_token').value.trim();
  var fetchHeaders = {'Content-Type': 'application/json'};
  if (token) fetchHeaders['Authorization'] = 'Bearer ' + token;

  fetch('/setup', {
    method: 'POST',
    headers: fetchHeaders,
    body: JSON.stringify(payload)
  })
  .then(function(r) { return r.json().then(function(d) { return {ok: r.ok, data: d}; }); })
  .then(function(res) {
    btn.disabled = false;
    btn.textContent = 'Save Configuration';
    if (res.ok) {
      msg.className = 'msg ok';
      msg.innerHTML = '<strong>Configuration saved.</strong><br>Restart the bilbycast-edge service to apply the new settings.';
    } else {
      msg.className = 'msg err';
      msg.textContent = res.data.error || 'Unknown error';
    }
  })
  .catch(function(err) {
    btn.disabled = false;
    btn.textContent = 'Save Configuration';
    msg.className = 'msg err';
    msg.textContent = 'Network error: ' + err.message;
  });

  return false;
}
</script>
</body>
</html>"##;

pub const SETUP_DISABLED_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>BilbyCast Edge Setup</title>
<style>
*{margin:0;padding:0;box-sizing:border-box}
body{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,monospace;background:#0d1117;color:#c9d1d9;min-height:100vh;display:flex;justify-content:center;align-items:center;padding:40px 16px}
.card{background:#161b22;border:1px solid #30363d;border-radius:12px;max-width:480px;width:100%;padding:32px;text-align:center}
h1{font-size:20px;font-weight:600;color:#f0f6fc;margin-bottom:8px}
p{font-size:14px;color:#8b949e;line-height:1.5}
</style>
</head>
<body>
<div class="card">
  <h1>Setup Disabled</h1>
  <p>The setup wizard has been disabled on this edge node. To re-enable it, set <code>"setup_enabled": true</code> in the configuration file and restart the service.</p>
</div>
</body>
</html>"##;
