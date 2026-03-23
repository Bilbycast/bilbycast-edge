# BilbyCast Edge API Security Guide

This document covers the authentication, authorization, and transport security architecture of the bilbycast-edge REST API.

---

## Table of Contents

- [Security Architecture Overview](#security-architecture-overview)
- [OAuth 2.0 Client Credentials Flow](#oauth-20-client-credentials-flow)
- [JWT Token Structure and Validation](#jwt-token-structure-and-validation)
- [Role-Based Access Control (RBAC)](#role-based-access-control-rbac)
- [TLS/HTTPS Setup](#tlshttps-setup)
- [Auth Configuration Reference](#auth-configuration-reference)
- [Getting Started with API Security](#getting-started-with-api-security)
- [Prometheus Integration](#prometheus-integration)
- [Grafana Integration](#grafana-integration)
- [WebSocket Authentication](#websocket-authentication)
- [Deployment Scenarios](#deployment-scenarios)
- [Security Best Practices](#security-best-practices)
- [Future Roadmap](#future-roadmap)

---

## Security Architecture Overview

bilbycast-edge implements a self-contained API security layer with the following components:

1. **OAuth 2.0 Client Credentials** -- Clients authenticate by exchanging a `client_id` and `client_secret` for a time-limited JWT token via the `/oauth/token` endpoint.
2. **JWT (HS256)** -- Tokens are signed using HMAC-SHA256 with a shared secret configured in the config file. No external identity provider is required.
3. **Role-Based Access Control** -- Two roles (`admin` and `monitor`) control access to API endpoints. Read-only endpoints accept any role; mutation endpoints require `admin`.
4. **TLS/HTTPS** -- Optional TLS termination using PEM-encoded certificates, powered by `rustls` (pure Rust, no OpenSSL dependency).
5. **Public endpoints** -- `/health`, `/oauth/token`, and `/setup` are always accessible without authentication. `/metrics` is public by default but can be placed behind auth. The `/setup` wizard is gated by the `setup_enabled` config flag (default: true) and is intended for initial provisioning of unconfigured nodes.

When the `auth` configuration block is absent or has `enabled: false`, all endpoints are open with no authentication. This is suitable for development but should never be used in production.

---

## OAuth 2.0 Client Credentials Flow

bilbycast-edge implements the OAuth 2.0 Client Credentials grant type (RFC 6749 Section 4.4). This flow is designed for machine-to-machine authentication where no interactive user login is needed.

### Flow diagram

```
Client                                    bilbycast-edge
  |                                            |
  |  POST /oauth/token                         |
  |  grant_type=client_credentials             |
  |  client_id=...                             |
  |  client_secret=...                         |
  | -----------------------------------------> |
  |                                            |  1. Validate grant_type
  |                                            |  2. Look up client_id + client_secret
  |                                            |  3. Build JWT claims (sub, role, iat, exp, iss)
  |                                            |  4. Sign with HMAC-SHA256
  |  200 OK                                    |
  |  { access_token, token_type, expires_in }  |
  | <----------------------------------------- |
  |                                            |
  |  GET /api/v1/flows                         |
  |  Authorization: Bearer <access_token>      |
  | -----------------------------------------> |
  |                                            |  1. Extract Bearer token
  |                                            |  2. Verify HS256 signature
  |                                            |  3. Check exp > now
  |                                            |  4. Insert Claims into request context
  |  200 OK                                    |
  |  { success: true, data: {...} }            |
  | <----------------------------------------- |
```

### Token request

The `/oauth/token` endpoint accepts both `application/json` and `application/x-www-form-urlencoded` bodies:

```bash
# JSON
curl -X POST https://edge.example.com/oauth/token \
  -H "Content-Type: application/json" \
  -d '{"grant_type":"client_credentials","client_id":"admin-client","client_secret":"your-secret-here"}'

# Form-encoded
curl -X POST https://edge.example.com/oauth/token \
  -d "grant_type=client_credentials&client_id=admin-client&client_secret=your-secret-here"
```

### Token usage

Include the returned token in the `Authorization` header for all protected requests:

```bash
curl -H "Authorization: Bearer eyJhbGciOi..." https://edge.example.com/api/v1/flows
```

### Token refresh

Tokens are not refreshable. When a token expires, request a new one from `/oauth/token`. Best practice is to request a new token slightly before expiry (e.g., at 90% of `expires_in`).

---

## JWT Token Structure and Validation

### Token format

bilbycast-edge uses standard three-part JWTs: `header.payload.signature`

**Header** (fixed):
```json
{
  "alg": "HS256",
  "typ": "JWT"
}
```

**Payload** (example):
```json
{
  "sub": "admin-client",
  "role": "admin",
  "iat": 1709820000,
  "exp": 1709823600,
  "iss": "bilbycast-edge"
}
```

**Signature**: HMAC-SHA256 of `base64url(header).base64url(payload)` using the configured `jwt_secret`.

### Validation checks

The auth middleware performs these checks in order:

1. **Authorization header present** -- Must be `Authorization: Bearer <token>`.
2. **Structural validity** -- Token must contain exactly three dot-separated Base64URL parts.
3. **Header match** -- The header must be the standard HS256/JWT header.
4. **Signature verification** -- HMAC-SHA256 signature must match using the configured secret.
5. **Expiry check** -- The `exp` claim must be greater than the current Unix timestamp.

If any check fails, the request is rejected with HTTP 401 and a JSON error body.

### Rejection examples

| Condition | HTTP Status | Error message |
|-----------|-------------|---------------|
| No Authorization header | 401 | `"missing Authorization header"` |
| Wrong scheme (e.g., Basic) | 401 | `"invalid authorization scheme, expected Bearer"` |
| Tampered payload | 401 | `"invalid JWT signature"` |
| Wrong secret | 401 | `"invalid JWT signature"` |
| Expired token | 401 | `"token expired"` |
| Malformed token | 401 | `"malformed JWT: expected 3 parts"` |

---

## Role-Based Access Control (RBAC)

bilbycast-edge supports two roles:

| Role | Description |
|------|-------------|
| `admin` | Full access: can read all data and make changes (create/update/delete flows, manage outputs, replace config) |
| `monitor` | Read-only access: can view flows, statistics, configuration, and subscribe to WebSocket stats |

### Endpoint access by role

| Endpoint | Method | admin | monitor | No auth (disabled) |
|----------|--------|-------|---------|-------------------|
| `/health` | GET | Yes | Yes | Yes |
| `/oauth/token` | POST | Yes | Yes | Yes |
| `/setup` | GET | Yes | Yes | Yes (if setup_enabled) |
| `/setup` | POST | Yes | Yes | Yes (if setup_enabled) |
| `/setup/status` | GET | Yes | Yes | Yes (if setup_enabled) |
| `/metrics` | GET | Yes | Yes | Yes (if public_metrics) |
| `/api/v1/flows` | GET | Yes | Yes | Yes |
| `/api/v1/flows/{id}` | GET | Yes | Yes | Yes |
| `/api/v1/flows` | POST | Yes | **No (403)** | Yes |
| `/api/v1/flows/{id}` | PUT | Yes | **No (403)** | Yes |
| `/api/v1/flows/{id}` | DELETE | Yes | **No (403)** | Yes |
| `/api/v1/flows/{id}/start` | POST | Yes | **No (403)** | Yes |
| `/api/v1/flows/{id}/stop` | POST | Yes | **No (403)** | Yes |
| `/api/v1/flows/{id}/restart` | POST | Yes | **No (403)** | Yes |
| `/api/v1/flows/{id}/outputs` | POST | Yes | **No (403)** | Yes |
| `/api/v1/flows/{id}/outputs/{oid}` | DELETE | Yes | **No (403)** | Yes |
| `/api/v1/stats` | GET | Yes | Yes | Yes |
| `/api/v1/stats/{id}` | GET | Yes | Yes | Yes |
| `/api/v1/config` | GET | Yes | Yes | Yes |
| `/api/v1/config` | PUT | Yes | **No (403)** | Yes |
| `/api/v1/config/reload` | POST | Yes | **No (403)** | Yes |
| `/api/v1/ws/stats` | GET | Yes | Yes | Yes |
| `/api/v1/flows/{id}/whip` | POST | Yes | **No (403)** | Yes |
| `/api/v1/flows/{id}/whip/{sid}` | DELETE | Yes | **No (403)** | Yes |
| `/api/v1/flows/{id}/whep` | POST | Yes | **No (403)** | Yes |
| `/api/v1/flows/{id}/whep/{sid}` | DELETE | Yes | **No (403)** | Yes |

**Note:** WHIP/WHEP endpoints additionally validate per-flow Bearer tokens configured in the flow's WebRTC input/output config. This is separate from the API auth — even with API auth disabled, the per-flow bearer_token protects WebRTC signaling.

**How RBAC works internally:**

- The auth middleware validates the JWT and inserts the `Claims` (including `role`) into the request extensions.
- Write endpoints use a `RequireAdmin` extractor that checks `claims.role == "admin"` and returns HTTP 403 if not.
- When auth is disabled (no `AuthState`), the `RequireAdmin` extractor creates a synthetic `admin` identity, allowing all operations.

---

## TLS/HTTPS Setup

TLS support requires building bilbycast-edge with the `tls` Cargo feature:

```bash
cargo build --release --features tls
```

### Generating self-signed certificates (development)

```bash
# Generate a private key and self-signed certificate valid for 365 days
openssl req -x509 -newkey rsa:4096 -keyout key.pem -out cert.pem -days 365 -nodes \
  -subj "/CN=bilbycast-edge"
```

### Using certificates from a CA

For production, use certificates signed by a trusted CA (e.g., Let's Encrypt):

```bash
# Example with certbot
certbot certonly --standalone -d edge.example.com

# Resulting files:
#   /etc/letsencrypt/live/edge.example.com/fullchain.pem  (cert_path)
#   /etc/letsencrypt/live/edge.example.com/privkey.pem    (key_path)
```

### Configuration

Add the `tls` block to the `server` section of your config file:

```json
{
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8443,
    "tls": {
      "cert_path": "/etc/bilbycast/cert.pem",
      "key_path": "/etc/bilbycast/key.pem"
    }
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `cert_path` | string | Yes | Path to PEM-encoded TLS certificate (or fullchain) |
| `key_path` | string | Yes | Path to PEM-encoded TLS private key |

Both paths must be non-empty and point to readable PEM files. The server validates these at startup.

### Verifying TLS

```bash
curl --cacert cert.pem https://localhost:8443/health

# Or skip verification for self-signed certs (development only)
curl -k https://localhost:8443/health
```

---

## Auth Configuration Reference

The `auth` block is an optional sub-object of `server`:

```json
{
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080,
    "auth": {
      "enabled": true,
      "jwt_secret": "a-secret-that-is-at-least-32-characters-long-for-security",
      "token_lifetime_secs": 3600,
      "public_metrics": true,
      "clients": [
        {
          "client_id": "admin-client",
          "client_secret": "strong-random-secret-here",
          "role": "admin"
        },
        {
          "client_id": "monitoring-system",
          "client_secret": "another-strong-secret",
          "role": "monitor"
        }
      ]
    }
  }
}
```

### Auth fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | boolean | - | Master switch. When `false`, auth middleware is a no-op. |
| `jwt_secret` | string | - | HMAC-SHA256 secret for signing/verifying JWTs. Must be at least 32 characters. |
| `token_lifetime_secs` | integer | `3600` | Token validity duration in seconds (default: 1 hour). |
| `public_metrics` | boolean | `true` | When `true`, `/metrics` and `/health` are accessible without auth. When `false`, `/metrics` requires a valid JWT. |
| `clients` | array | - | List of registered OAuth clients. At least one client must be configured when auth is enabled. |

### Client fields

| Field | Type | Description |
|-------|------|-------------|
| `client_id` | string | Unique identifier for this client. Cannot be empty. |
| `client_secret` | string | Secret used to authenticate. Cannot be empty. |
| `role` | string | Must be `"admin"` or `"monitor"`. |

### Validation rules

When `enabled: true`, the following validation rules are enforced at startup and during config changes:

- `jwt_secret` must be at least 32 characters.
- At least one client must be configured.
- Each client's `client_id` and `client_secret` must be non-empty.
- Each client's `role` must be exactly `"admin"` or `"monitor"`.

---

## Getting Started with API Security

### Step 1: Generate a JWT secret

Generate a strong random secret (at least 32 characters, 64+ recommended):

```bash
openssl rand -base64 48
# Example output: K7nXp2qR8vF3mBwYd0hL5jZ1tA6gCeHsN9uIoP4xWkQrJfMaVbDcEiGyTlUwSzO
```

### Step 2: Add auth configuration

Edit your `config.json`:

```json
{
  "version": 1,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080,
    "auth": {
      "enabled": true,
      "jwt_secret": "K7nXp2qR8vF3mBwYd0hL5jZ1tA6gCeHsN9uIoP4xWkQrJfMaVbDcEiGyTlUwSzO",
      "token_lifetime_secs": 3600,
      "public_metrics": true,
      "clients": [
        {
          "client_id": "admin",
          "client_secret": "change-me-to-a-strong-random-secret",
          "role": "admin"
        },
        {
          "client_id": "grafana",
          "client_secret": "grafana-read-only-secret",
          "role": "monitor"
        }
      ]
    }
  },
  "flows": []
}
```

### Step 3: Start the server

```bash
./bilbycast-edge --config config.json
```

You should see in the logs:
```
API authentication enabled: 2 client(s) configured
```

### Step 4: Obtain a token

```bash
TOKEN=$(curl -s -X POST http://localhost:8080/oauth/token \
  -H "Content-Type: application/json" \
  -d '{"grant_type":"client_credentials","client_id":"admin","client_secret":"change-me-to-a-strong-random-secret"}' \
  | jq -r '.access_token')

echo $TOKEN
```

### Step 5: Use the token

```bash
# This works
curl -H "Authorization: Bearer $TOKEN" http://localhost:8080/api/v1/flows

# This fails with 401
curl http://localhost:8080/api/v1/flows
# {"success":false,"error":"missing Authorization header"}
```

### Step 6: (Optional) Add TLS

```bash
# Build with TLS support
cargo build --release --features tls

# Generate certs
openssl req -x509 -newkey rsa:4096 -keyout key.pem -out cert.pem -days 365 -nodes \
  -subj "/CN=bilbycast-edge"
```

Add TLS to config:
```json
{
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8443,
    "tls": {
      "cert_path": "./cert.pem",
      "key_path": "./key.pem"
    },
    "auth": { ... }
  }
}
```

---

## Prometheus Integration

bilbycast-edge exposes a Prometheus-compatible `/metrics` endpoint.

### Scrape configuration

Add the following to your `prometheus.yml`:

```yaml
scrape_configs:
  - job_name: 'bilbycast-edge'
    scrape_interval: 5s
    static_configs:
      - targets: ['edge-host:8080']
```

### When public_metrics is true (default)

The `/metrics` endpoint requires no authentication. Prometheus can scrape it directly with no extra configuration.

### When public_metrics is false

You need to configure Prometheus with a Bearer token. First, create a `monitor` client and obtain a token. Then configure Prometheus:

```yaml
scrape_configs:
  - job_name: 'bilbycast-edge'
    scrape_interval: 5s
    scheme: https
    tls_config:
      insecure_skip_verify: true  # only for self-signed certs
    authorization:
      type: Bearer
      credentials: 'eyJhbGciOi...'
    static_configs:
      - targets: ['edge-host:8443']
```

Since bilbycast-edge tokens expire (default: 1 hour), for long-running Prometheus scraping with auth-protected metrics, consider either:
- Setting a long `token_lifetime_secs` (e.g., `86400` for 24 hours) and using a script to refresh the token in the Prometheus config.
- Keeping `public_metrics: true` (the default), which is sufficient for most deployments since `/metrics` does not expose sensitive configuration data.

### Key metrics for alerting

```yaml
# Alert on zero bitrate (stream down)
- alert: BilbycastFlowDown
  expr: bilbycast_edge_flow_input_bitrate_bps == 0
  for: 30s

# Alert on packet loss
- alert: BilbycastPacketLoss
  expr: rate(bilbycast_edge_flow_input_packets_lost[5m]) > 0
  for: 1m

# Alert on TR-101290 errors
- alert: BilbycastTSErrors
  expr: rate(bilbycast_edge_tr101290_cc_errors_total[5m]) > 0
  for: 1m
```

---

## Grafana Integration

### With public metrics (default)

1. Add a Prometheus data source pointing to your Prometheus instance.
2. Create dashboards using `bilbycast_edge_*` metrics.

### With auth-protected JSON API

To use the bilbycast-edge JSON API directly as a Grafana data source (e.g., with the JSON API plugin):

1. Install the Grafana JSON API data source plugin.
2. Create a new data source with:
   - **URL:** `https://edge-host:8443`
   - **Custom HTTP Headers:** Add `Authorization` with value `Bearer <your-token>`
3. Use endpoints like `/api/v1/stats` for dashboard panels.

**Token management:** Grafana stores the Bearer token in its data source configuration. You will need to update the token when it expires. For a better experience, use the Prometheus approach with `public_metrics: true`.

---

## WebSocket Authentication

The WebSocket endpoint `/api/v1/ws/stats` follows the same auth middleware as other protected endpoints. The JWT must be provided during the WebSocket upgrade request.

### Method 1: Authorization header (preferred)

Most WebSocket client libraries support custom headers during the handshake:

```bash
wscat -c "wss://edge-host:8443/api/v1/ws/stats" \
  -H "Authorization: Bearer eyJhbGciOi..."
```

```javascript
// Node.js ws library
const ws = new WebSocket("wss://edge-host:8443/api/v1/ws/stats", {
  headers: { Authorization: "Bearer " + token }
});
```

### Method 2: Query parameter (browser workaround)

Browser `WebSocket` API does not support custom headers. If you need to connect from a browser, pass the token as a query parameter (requires application-level handling, not built into the default middleware):

```javascript
const ws = new WebSocket("wss://edge-host:8443/api/v1/ws/stats?token=" + token);
```

Note: Query parameter authentication is less secure than headers because tokens may appear in server logs and browser history. Use the header method when possible.

---

## Deployment Scenarios

| Scenario | Auth | TLS | public_metrics | Description |
|----------|------|-----|----------------|-------------|
| **Development / Lab** | Disabled | No | true | No security. All endpoints open. Quick setup for testing. |
| **Standalone appliance** | Enabled | Self-signed | true | JWT auth protects mutation endpoints. Prometheus scrapes metrics without auth. Self-signed TLS for encryption in transit. |
| **Small facility** | Enabled | CA-signed | true | CA-signed certificates. Admin and monitor clients for operators and dashboards. Prometheus scrapes public metrics. |
| **Enterprise / Multi-tenant** | Enabled | CA-signed | false | All endpoints behind auth including metrics. Separate clients per system (Grafana, automation, operators). Certificates from enterprise CA or Let's Encrypt. |

### Configuration for each scenario

**Development (no auth):**

```json
{
  "version": 1,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080
  },
  "flows": []
}
```

**Standalone appliance:**

```json
{
  "version": 1,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8443,
    "tls": {
      "cert_path": "/etc/bilbycast/cert.pem",
      "key_path": "/etc/bilbycast/key.pem"
    },
    "auth": {
      "enabled": true,
      "jwt_secret": "generate-a-64-char-random-string-here-using-openssl-rand-base64",
      "token_lifetime_secs": 3600,
      "public_metrics": true,
      "clients": [
        { "client_id": "admin", "client_secret": "strong-admin-secret", "role": "admin" },
        { "client_id": "dashboard", "client_secret": "dashboard-secret", "role": "monitor" }
      ]
    }
  },
  "flows": []
}
```

**Enterprise (everything locked down):**

```json
{
  "version": 1,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8443,
    "tls": {
      "cert_path": "/etc/pki/tls/certs/bilbycast.pem",
      "key_path": "/etc/pki/tls/private/bilbycast.key"
    },
    "auth": {
      "enabled": true,
      "jwt_secret": "use-openssl-rand-base64-64-to-generate-this-very-long-secret-string",
      "token_lifetime_secs": 1800,
      "public_metrics": false,
      "clients": [
        { "client_id": "ops-admin", "client_secret": "secret-1", "role": "admin" },
        { "client_id": "automation", "client_secret": "secret-2", "role": "admin" },
        { "client_id": "grafana", "client_secret": "secret-3", "role": "monitor" },
        { "client_id": "prometheus", "client_secret": "secret-4", "role": "monitor" },
        { "client_id": "noc-dashboard", "client_secret": "secret-5", "role": "monitor" }
      ]
    }
  },
  "flows": []
}
```

---

## Security Best Practices

### JWT secret

- Use at least 64 characters of cryptographically random data (the minimum enforced is 32).
- Generate with: `openssl rand -base64 64`
- Store the config file with restricted permissions: `chmod 600 config.json`
- Do not commit secrets to version control.

### Client credentials

- Use unique, strong secrets for each client (at least 20 characters of random data).
- Use separate clients for each integration (Grafana, Prometheus, automation scripts, human operators).
- Use the `monitor` role for read-only integrations; only grant `admin` to systems that need to make changes.

### TLS certificates

- Use CA-signed certificates in production (Let's Encrypt is free and automated).
- Set up certificate auto-renewal (e.g., certbot with a cron job).
- Use port 8443 (or 443 with appropriate permissions) for HTTPS.
- The `tls` feature uses `rustls`, which supports TLS 1.2 and 1.3 only (no legacy TLS 1.0/1.1).

### Token lifetime

- Default is 3600 seconds (1 hour), which is reasonable for most use cases.
- For automation scripts that run continuously, consider longer lifetimes (e.g., 86400 for 24 hours) combined with token refresh logic.
- For interactive operator use, shorter lifetimes (1800 seconds / 30 minutes) reduce risk from token theft.

### Network security

- Restrict API access to trusted networks using firewall rules.
- The monitor dashboard (if enabled) runs on a separate port and does not share auth with the API.
- Consider running behind a reverse proxy (nginx, Caddy) for additional features like rate limiting and IP allowlisting.

### Config file security

- The config file contains client secrets and the JWT secret in plaintext.
- Set file permissions to owner-only: `chmod 600 config.json`
- Config changes via the API (PUT /api/v1/config) are persisted to disk atomically (write to temp file, then rename).
- The auth section is not modifiable via the API -- it can only be changed by editing the config file and restarting.

---

## Future Roadmap

### External OAuth 2.0 / OpenID Connect

A future release may support delegating authentication to an external identity provider (e.g., Keycloak, Auth0, Azure AD) using standard OAuth 2.0 authorization code or token introspection flows. This would allow:
- Centralized user management
- Integration with enterprise SSO
- Token revocation via the IdP

### NMOS IS-10 (Authorization)

bilbycast-edge implements NMOS IS-04 (Node API v1.3) and IS-05 (Connection Management v1.1). These endpoints are currently unauthenticated for compatibility with NMOS controllers. The AMWA NMOS IS-10 specification defines an authorization framework for broadcast media networks. A future release may implement IS-10 compatibility, allowing bilbycast-edge to secure the NMOS endpoints using authorization tokens issued by a central NMOS Authorization Server and validated using JWKs (JSON Web Key Sets).

### API key authentication

For simpler integrations that do not need OAuth flows, a future release may support static API keys as an alternative authentication method alongside JWT tokens.
