// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use axum::routing::{delete, get, post, put};
use tokio::sync::{RwLock, broadcast};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::config::models::AppConfig;
use crate::engine::manager::FlowManager;
use crate::engine::resource_monitor::SystemResourceState;
use crate::manager::events::EventSender;
use crate::tunnel::manager::TunnelManager;

use super::auth::{self, AuthState};
use super::nmos_is05::Is05State;
use super::nmos_is08::Is08State;
use super::{flows, inputs, nmos, nmos_is05, nmos_is08, outputs, ptp, stats, tunnels, ws};

/// Shared application state accessible from all Axum handlers via [`axum::extract::State`].
#[derive(Clone)]
pub struct AppState {
    /// The current in-memory application configuration.
    pub config: Arc<RwLock<AppConfig>>,
    /// Filesystem path to the persisted `config.json` file.
    pub config_path: PathBuf,
    /// Filesystem path to the persisted `secrets.json` file.
    pub secrets_path: PathBuf,
    /// Handle to the flow engine manager.
    pub flow_manager: Arc<FlowManager>,
    /// Handle to the IP tunnel manager.
    pub tunnel_manager: Arc<TunnelManager>,
    /// Monotonic timestamp recorded at application startup.
    pub start_time: Instant,
    /// Broadcast channel sender for WebSocket stats.
    pub ws_stats_tx: broadcast::Sender<String>,
    /// Optional auth state (None = auth disabled).
    pub auth_state: Option<Arc<AuthState>>,
    /// NMOS IS-05 staged transport parameters.
    pub is05_state: Arc<Is05State>,
    /// NMOS IS-08 audio channel mapping state. Active map is persisted next
    /// to `config.json`; staged map is in-memory only.
    pub is08_state: Arc<Is08State>,
    /// WebRTC session registry for WHIP/WHEP endpoints (None when webrtc feature disabled).
    #[cfg(feature = "webrtc")]
    pub webrtc_sessions: Option<Arc<crate::api::webrtc::registry::WebrtcSessionRegistry>>,
    /// Manager event sender. Used by NMOS IS-05/IS-08 handlers to surface
    /// `nmos` lifecycle events (sender/receiver activations, channel-map
    /// stage/activate). `None` is tolerated so unit tests that build a
    /// minimal AppState don't need to plumb the channel.
    pub event_sender: Option<EventSender>,
    /// System resource state (CPU, RAM) for Prometheus metrics and API stats.
    pub resource_state: Arc<SystemResourceState>,
    /// Standby listener manager for passive-type inputs not assigned to flows.
    pub standby_listeners: Option<Arc<crate::engine::standby_listeners::StandbyListenerManager>>,
    /// Per-IP rate limiter for the `/oauth/token` endpoint (None = no limiting).
    pub token_rate_limiter: Option<Arc<auth::TokenEndpointRateLimiter>>,
    /// Device-local manager-link state. Written by the manager-client loop on
    /// connect/disconnect; read by the local `/health` endpoint + monitor
    /// dashboard so an operator at the device sees a lost manager link. This
    /// is a purely local indicator — it never rides the WS protocol and does
    /// not affect the health payload sent UP to the manager.
    pub manager_link: Arc<crate::manager::link_state::ManagerLinkState>,
    /// Live node-level PTP clock state, kept fresh by the node PTP monitor
    /// task (`engine::st2110::ptp::PtpStateReporter::spawn_node_monitor`).
    /// Read non-blocking by the Prometheus `/metrics` handler so a scrape
    /// never has to do its own `ptp4l` round-trip.
    pub ptp_node_state: crate::engine::st2110::ptp::PtpStateHandle,
}

// ---------------------------------------------------------------------------
// Browser-origin policy
// ---------------------------------------------------------------------------
//
// The edge API used to run `CorsLayer::permissive()` — `Access-Control-Allow-
// Origin: *` plus `Access-Control-Allow-Methods: *` — on **every** route. Auth
// is off by default (`server.auth` is `None` in `ServerConfig::default`), so
// any web page the operator happened to load could
// `fetch('http://<edge>:8080/api/v1/config')`, read the response through the
// wildcard, and harvest the SRT passphrases / RTMP stream keys that live in
// `config.json` by design. The same wildcard made the NMOS write surface a
// browser target: a cross-origin `PATCH .../senders/{id}/staged` with
// `activate_immediate` re-points a live sender at an attacker-chosen address
// **and persists it to config.json** (`nmos_is05::patch_sender_staged`).
//
// CORS is a browser-only mechanism, so removing it costs nothing outside a
// browser: curl, Prometheus, and every native NMOS controller (Sony, Riedel,
// Lawo, the AMWA testing tool) never read these headers. The rest of the
// private API has no cross-origin browser consumer at all — the monitor
// dashboard fetches relative paths off its own port (`monitor::server`), the
// setup wizard fetches relative paths off this one, and the manager reaches
// the node over its outbound WebSocket, never from the browser.
//
// So instead of one blanket layer there are now exactly two, each scoped to
// the surface that genuinely has browser clients, and everything else — most
// importantly `/api/v1/config` — sends no `Access-Control-*` header at all,
// unmatched paths included (the router carries an explicit [`not_found`]
// fallback so a layered NMOS default fallback cannot answer `/nope` with a
// wildcard):
//
// - [`nmos_cors`] on `/x-nmos/**`. AMWA requires NMOS APIs to support CORS so
//   browser-hosted controllers can discover a node, and IS-04 discovery is
//   public by specification — but the wildcard covers **safe methods only**
//   unless `server.nmos_browser_control` names the controller's origin.
// - [`whip_whep_cors`] on the four WHIP / WHEP signalling routes. Browsers are
//   the documented client there and RFC 9725 requires preflight support.
//
// [`guard_cross_origin_write`] is the actual gate, and it sits on **both**
// mutation surfaces — `/x-nmos/**` and `/api/v1/**`. CORS alone stops a
// browser *reading* a response; it never stops the *write*, because a request
// with no non-safelisted header and no non-safelisted content type is
// "simple" and is never preflighted. `POST /api/v1/flows/{id}/stop` and
// `POST /api/v1/config/reload` take no body extractor, so before this guard
// any web page the operator loaded could take a live flow off air on an
// auth-off node (the shipped default) — strictly worse than re-pointing one
// IS-05 sender, and the reason the guard is not NMOS-only.
//
// RESIDUAL RISK, stated plainly: this is *mitigated-partially*, not closed.
// IS-05 / IS-08 writes remain unauthenticated by specification on auth-off
// nodes — anything on the LAN that is not a browser can still drive them —
// and when auth is on, a `monitor`-role token is enough to re-point a live
// sender, because NMOS connection management deliberately does not require
// the `admin` role. The AMWA NMOS Testing Tool has **not** been run against
// this change (no network-isolated bench for it here); its generic CORS check
// asserts that an `OPTIONS` advertises the methods a resource supports, which
// the default policy does not do for `.../staged` PATCH. `nmos_browser_control`
// is the supported way to restore that.

/// Request headers that only a browser sends. Every current browser stamps
/// `Sec-Fetch-*` on **every** request it issues, same-origin ones included;
/// curl, the AMWA testing tool and native NMOS controllers stamp none.
///
/// This is what closes the DNS-rebinding path that an `Origin` vs `Host`
/// comparison cannot: under rebinding both headers name the attacker's own
/// domain and therefore agree, so a same-origin test passes a forgery. Keying
/// on "a browser issued this" instead of "the two headers disagree" refuses it
/// without touching the native path.
const BROWSER_FETCH_METADATA: [&str; 3] = ["sec-fetch-site", "sec-fetch-mode", "sec-fetch-dest"];

/// Count of requests [`guard_cross_origin_write`] has refused, for the log
/// line. The `Origin` is deliberately **not** logged: it is remote-controlled
/// and one forged request per log line is a journal-amplification primitive on
/// a box whose disk carries media.
static CROSS_ORIGIN_REFUSALS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Browser origins allowed to issue state-changing requests to the surface
/// this policy is attached to.
///
/// Built once in [`build_router`] and cloned into the guard as its axum
/// `State`; read-only thereafter, so the guard takes no lock and allocates
/// nothing per request beyond the refused-request path.
#[derive(Clone)]
struct BrowserWritePolicy {
    allowed_origins: Arc<[String]>,
}

impl Default for BrowserWritePolicy {
    /// The shipped default: no browser may write.
    fn default() -> Self {
        Self::new(&[])
    }
}

impl BrowserWritePolicy {
    /// Policy admitting the listed origins. An empty list refuses every
    /// browser-issued state change, which is the shipped default.
    fn new(origins: &[String]) -> Self {
        Self {
            allowed_origins: origins.into(),
        }
    }

    /// True when the operator explicitly listed this origin.
    fn permits(&self, origin: &str) -> bool {
        self.allowed_origins
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(origin))
    }

    /// True when at least one origin is allowed to write from a browser.
    fn grants_browser_writes(&self) -> bool {
        !self.allowed_origins.is_empty()
    }
}

/// Methods that cannot change state, and are therefore safe to answer for any
/// browser origin.
fn is_safe_method(method: &Method) -> bool {
    *method == Method::GET || *method == Method::HEAD || *method == Method::OPTIONS
}

/// The authority this request was addressed to.
///
/// Prefers the request URI's authority and falls back to the `Host` header.
/// Both are needed: axum-server negotiates ALPN `h2` first whenever
/// `server.tls` is set, and an HTTP/2 request carries its authority in the
/// `:authority` pseudo-header, which hyper surfaces on `req.uri()` and does
/// **not** mirror into a `Host` header. Reading `Host` alone therefore made
/// this inert on every TLS deployment.
fn request_authority(req: &Request) -> Option<&str> {
    req.uri()
        .authority()
        .map(axum::http::uri::Authority::as_str)
        .or_else(|| req.headers().get(header::HOST).and_then(|v| v.to_str().ok()))
}

/// True when an `Origin` header names the same authority the request was
/// addressed to, i.e. the request is same-origin.
///
/// Compares authorities verbatim after stripping the scheme. A same-origin
/// request that elides a default port (`http://edge` against an authority of
/// `edge:80`) is not recognised, which fails *closed*.
///
/// This is a **fallback**, not the primary test: it is only consulted for a
/// client that sends `Origin` but no `Sec-Fetch-*`, because two
/// attacker-controlled headers agreeing proves nothing under DNS rebinding —
/// see [`BROWSER_FETCH_METADATA`].
fn origin_matches_host(origin: &str, host: Option<&str>) -> bool {
    let Some(host) = host else { return false };
    let authority = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    !authority.is_empty() && authority.eq_ignore_ascii_case(host)
}

/// CORS policy for `/x-nmos/**`.
///
/// Wildcard origin — required by AMWA and expected by browser-hosted NMOS
/// clients. `Accept`, `Content-Type` and `Authorization` are allowed
/// unconditionally: without `authorization` in the preflight response a
/// browser-hosted controller cannot attach a Bearer token, so an
/// auth-enabled node is unreachable from one **even for a `GET`** — an
/// earlier revision of this policy allowed `Accept` only and made the node
/// browser-unusable the moment auth was turned on.
///
/// `browser_control` mirrors `server.nmos_browser_control`. With no origins
/// configured the method list is the **safe** methods only, so a cross-origin
/// `PATCH .../staged` preflight finds no match and the browser never sends the
/// real request. With origins configured, `PATCH` / `POST` are advertised and
/// [`guard_cross_origin_write`] — not this layer — decides which origins may
/// actually use them; CORS cannot express a per-origin method set, and the
/// guard is the authoritative gate in either case.
///
/// Deliberately no `allow_credentials` — pairing it with a wildcard origin is
/// rejected by browsers anyway, and the edge authenticates with a Bearer
/// header rather than a cookie.
fn nmos_cors(browser_control: bool) -> CorsLayer {
    let methods = if browser_control {
        vec![
            Method::GET,
            Method::HEAD,
            Method::OPTIONS,
            Method::PATCH,
            Method::POST,
        ]
    } else {
        vec![Method::GET, Method::HEAD, Method::OPTIONS]
    };
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(methods)
        .allow_headers([header::ACCEPT, header::CONTENT_TYPE, header::AUTHORIZATION])
}

/// CORS policy for the WHIP / WHEP signalling endpoints.
///
/// WHIP publishers and WHEP viewers are browsers by design (RFC 9725 requires
/// a WHIP endpoint to answer preflight), and their `Content-Type:
/// application/sdp` is not a CORS-safelisted value, so **every** browser
/// request here is preflighted. Dropping CORS from these four routes would
/// break a documented, shipped consumer, so they keep the wildcard — narrowed
/// to what the protocol actually needs:
///
/// - methods POST (offer) and DELETE (session teardown);
/// - `Content-Type` and `Authorization` on the request, the latter because a
///   flow may carry a per-flow `bearer_token`;
/// - `Location` exposed on the response, because the client cannot tear its
///   own session down without reading it.
///
/// The risk profile is nothing like `/api/v1/config`: these endpoints return
/// an SDP answer, hold their own per-flow token check, and change no node
/// configuration.
#[cfg(feature = "webrtc")]
fn whip_whep_cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
        .expose_headers([header::LOCATION])
}

/// Refuse **unauthenticated, browser-initiated, state-changing** requests.
///
/// Mounted on `/x-nmos/**` and on `/api/v1/**`, inside the auth layer on both
/// so it can read the [`auth::Claims`] the auth middleware inserts. It is the
/// authoritative gate, not defence in depth behind CORS: CORS is advisory and
/// only ever governs what a browser will *send*, and a state-changing request
/// carrying no non-safelisted header is "simple" — never preflighted, so a
/// CORS policy has no say in it at all.
///
/// Four deliberate pass-throughs keep real clients working:
/// - **Safe methods** pass. IS-04 discovery and every `GET` are unchanged.
/// - **Authenticated requests** pass. A validated [`auth::Claims`] means the
///   caller presented a Bearer token, which a foreign page cannot attach to a
///   request the browser makes on the operator's behalf. Note this branch is
///   reachable **only when `server.auth.enabled` is true** — with auth off,
///   [`auth::auth_middleware`] returns without inserting `Claims`, so on the
///   default configuration authentication is not an available escape hatch
///   and `server.nmos_browser_control` is the one that is.
/// - **An origin the operator listed** in `server.nmos_browser_control`
///   passes, on the NMOS surface. This is the supported way to run a
///   browser-hosted controller (sony/nmos-js and friends) against a node.
/// - **Requests that carry no browser fingerprint** pass: no `Sec-Fetch-*`
///   (see [`BROWSER_FETCH_METADATA`]) and either no `Origin` at all or an
///   `Origin` matching the authority they were addressed to. This is what
///   keeps an unauthenticated on-prem NMOS controller — the normal ST 2110
///   deployment — and every `curl` / manager / CI caller working exactly as
///   they do today.
async fn guard_cross_origin_write(
    State(policy): State<BrowserWritePolicy>,
    req: Request,
    next: Next,
) -> Response {
    if is_safe_method(req.method()) || req.extensions().get::<auth::Claims>().is_some() {
        return next.run(req).await;
    }

    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let from_browser = BROWSER_FETCH_METADATA
        .iter()
        .any(|name| req.headers().contains_key(*name));
    let listed = origin.as_deref().is_some_and(|o| policy.permits(o));
    let same_origin = origin
        .as_deref()
        .is_some_and(|o| origin_matches_host(o, request_authority(&req)));

    if listed || (!from_browser && (origin.is_none() || same_origin)) {
        return next.run(req).await;
    }

    // Nothing remote-controlled reaches this line. The route template and the
    // counter are ours. The method is remote-chosen and `http::Method` admits
    // any RFC 9110 token of *any* length — past 15 bytes `from_bytes` allocates
    // rather than rejecting — so only the four routed mutation verbs are
    // echoed and anything else collapses to a fixed string. The `Origin` is
    // remote-chosen too, and is dropped entirely rather than bounded: one
    // attacker-sized string per forged request is a journal-amplification
    // primitive on a box whose disk carries media.
    let refusals = CROSS_ORIGIN_REFUSALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let route = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or("<unmatched>", axum::extract::MatchedPath::as_str);
    let method = req.method();
    let method = if *method == Method::POST {
        "POST"
    } else if *method == Method::PUT {
        "PUT"
    } else if *method == Method::PATCH {
        "PATCH"
    } else if *method == Method::DELETE {
        "DELETE"
    } else {
        "<other>"
    };
    tracing::warn!(
        "SECURITY: refused browser-initiated {method} {route} (refusal #{refusals} since boot) — \
         state changes are not reachable from a web page"
    );
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "success": false,
            "error": "browser-initiated state changes are refused; use a Bearer token, a \
                      non-browser client, or list the controller's origin in \
                      server.nmos_browser_control",
        })),
    )
        .into_response()
}

/// Explicit router-wide fallback.
///
/// Without one, `Router::merge` inherits the *layered* default fallback of
/// whichever sub-router was merged last, so `/x-nmos/**`'s CORS layer answered
/// unmatched paths — `OPTIONS /nope` came back `200` with
/// `Access-Control-Allow-Origin: *`. Harmless payload, but it contradicted the
/// policy this module states, and only existing routes were ever probed.
async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "success": false, "error": "not found" })),
    )
        .into_response()
}

/// Constructs the main Axum [`Router`] with all API routes, auth middleware, and layers.
///
/// When auth is enabled, the router is split into:
/// - **Public routes**: `/health`, `/oauth/token`, and optionally `/metrics` — no auth required
/// - **Read-only routes**: GET endpoints — require valid JWT (any role)
/// - **Admin routes**: POST/PUT/DELETE mutation endpoints — require `admin` role
///
/// When auth is disabled (no `auth` config or `enabled: false`), all routes are open.
///
/// Browser reachability is a separate axis from auth. Exactly two surfaces
/// emit any `Access-Control-*` header — `/x-nmos/**` and the four WHIP / WHEP
/// signalling routes (POST + DELETE). Everything else, `/api/v1/config` and
/// unmatched paths included, emits none. See the "Browser-origin policy"
/// section above.
///
/// `nmos_browser_control` is `server.nmos_browser_control` — the origins of
/// browser-hosted NMOS controllers permitted to drive connection management.
/// It is passed in rather than read off `state.config`, which is an
/// `RwLock` this synchronous function could only `try_read`: a fail-open
/// fallback on a security policy is not acceptable, and the sole caller
/// (`main.rs`) already holds the resolved `AppConfig`.
pub fn build_router(state: AppState, nmos_browser_control: &[String]) -> Router {
    let auth_state = state.auth_state.clone();
    let nmos_policy = BrowserWritePolicy::new(nmos_browser_control);
    // `/api/v1` has no browser client that writes: the monitor dashboard and
    // the setup wizard both fetch relative paths off their own origin and
    // neither touches this surface, so there is nothing to opt in.
    let private_api_policy = BrowserWritePolicy::default();

    // --- Public routes (never require auth) ---
    let public_routes = Router::new()
        .route("/health", get(stats::health))
        .route("/oauth/token", post(auth::oauth_token_handler));

    // --- Optionally public metrics ---
    let metrics_public = auth_state
        .as_ref()
        .map(|a| a.config.public_metrics)
        .unwrap_or(true);

    // Setup wizard routes (public, no auth — for initial provisioning)
    let public_routes = public_routes
        .route("/setup", get(crate::setup::handlers::setup_page).post(crate::setup::handlers::apply_setup))
        .route("/setup/status", get(crate::setup::handlers::setup_status));

    let public_routes = if metrics_public {
        public_routes.route("/metrics", get(stats::prometheus_metrics))
    } else {
        public_routes
    };

    // --- Protected routes (require valid JWT when auth enabled) ---
    // Read-only routes: any authenticated role (admin or monitor)
    let read_routes = Router::new()
        .route("/api/v1/inputs", get(inputs::list_inputs))
        .route("/api/v1/inputs/{input_id}", get(inputs::get_input))
        .route("/api/v1/outputs", get(outputs::list_outputs))
        .route("/api/v1/outputs/{output_id}", get(outputs::get_output))
        .route("/api/v1/flows", get(flows::list_flows))
        .route("/api/v1/flows/{flow_id}", get(flows::get_flow))
        .route("/api/v1/stats", get(stats::all_stats))
        .route("/api/v1/stats/{flow_id}", get(stats::flow_stats))
        .route("/api/v1/config", get(flows::get_config))
        .route("/api/v1/ptp", get(ptp::get_ptp))
        .route("/api/v1/ws/stats", get(ws::ws_stats_handler))
        .route("/api/v1/tunnels", get(tunnels::list_tunnels))
        .route("/api/v1/tunnels/{id}", get(tunnels::get_tunnel));

    // Add metrics under auth if not public
    let read_routes = if !metrics_public {
        read_routes.route("/metrics", get(stats::prometheus_metrics))
    } else {
        read_routes
    };

    // Write routes: require admin role (enforced by RequireAdmin extractor in handlers,
    // middleware just validates the JWT is present and not expired)
    let write_routes = Router::new()
        .route("/api/v1/inputs", post(inputs::create_input))
        .route(
            "/api/v1/inputs/{input_id}",
            put(inputs::update_input).delete(inputs::delete_input),
        )
        .route("/api/v1/outputs", post(outputs::create_output))
        .route(
            "/api/v1/outputs/{output_id}",
            put(outputs::update_output).delete(outputs::delete_output),
        )
        .route("/api/v1/flows", post(flows::create_flow))
        .route(
            "/api/v1/flows/{flow_id}",
            put(flows::update_flow).delete(flows::delete_flow),
        )
        .route(
            "/api/v1/flows/{flow_id}/assembly",
            put(flows::update_flow_assembly),
        )
        .route("/api/v1/flows/{flow_id}/start", post(flows::start_flow))
        .route("/api/v1/flows/{flow_id}/stop", post(flows::stop_flow))
        .route("/api/v1/flows/{flow_id}/restart", post(flows::restart_flow))
        .route("/api/v1/flows/{flow_id}/outputs", post(flows::add_output))
        .route(
            "/api/v1/flows/{flow_id}/outputs/{output_id}",
            delete(flows::remove_output),
        )
        .route(
            "/api/v1/flows/{flow_id}/activate-input",
            post(flows::activate_input),
        )
        .route(
            "/api/v1/outputs/{output_id}/active",
            post(flows::set_output_active),
        )
        .route("/api/v1/config", put(flows::replace_config))
        .route("/api/v1/config/reload", post(flows::reload_config))
        .route("/api/v1/ptp", put(ptp::put_ptp))
        .route("/api/v1/tunnels", post(tunnels::create_tunnel))
        .route("/api/v1/tunnels/{id}", delete(tunnels::delete_tunnel));

    // WHIP/WHEP routes (feature-gated).
    //
    // These are the ONE part of `/api/v1` that a browser is *supposed* to
    // reach cross-origin: WHIP publishers and WHEP viewers are documented,
    // shipped consumers (`docs/supported-protocols.md`), and a player page is
    // essentially never served from the edge's own origin. They carry their
    // own per-flow `bearer_token` check, exchange SDP rather than node
    // configuration, and mutate no config — so they keep a CORS layer of their
    // own while the rest of the private API has none. See [`whip_whep_cors`].
    // WHIP/WHEP is built as its own router and merged at TOP level, never into
    // `write_routes`.
    //
    // `route_layer` wraps inward-out: a layer added later sits OUTSIDE one
    // added earlier. Merging these routes into `write_routes` therefore put
    // `protected_routes`' auth middleware outside this router's CORS layer, so
    // a browser's `OPTIONS` preflight was answered 401 by the auth middleware
    // before the CORS layer ever saw it — which breaks every browser WHEP
    // player against an auth-enabled node. Keeping the router separate lets it
    // carry its own auth INSIDE its own CORS, which is the ordering
    // `nmos_routes` already uses.
    #[cfg(feature = "webrtc")]
    let whip_whep_routes = {
        use crate::api::webrtc::handlers;
        Router::new()
            .route("/api/v1/flows/{flow_id}/whip", post(handlers::whip_offer))
            .route("/api/v1/flows/{flow_id}/whip/{session_id}", delete(handlers::whip_delete))
            .route("/api/v1/flows/{flow_id}/whep", post(handlers::whep_offer))
            .route("/api/v1/flows/{flow_id}/whep/{session_id}", delete(handlers::whep_delete))
            .route_layer(middleware::from_fn_with_state(
                auth_state.clone(),
                auth::auth_middleware,
            ))
            .layer(whip_whep_cors())
    };

    // Combine protected routes with the cross-origin write guard and the auth
    // middleware.
    //
    // `route_layer` wraps inward-out, so the guard added first sits INSIDE the
    // auth layer added second and can read the `Claims` auth inserts — the
    // ordering `nmos_routes` uses. The guard is on `/api/v1` and not only on
    // `/x-nmos/**` because the damage here is worse: `POST
    // /api/v1/flows/{id}/stop` and `POST /api/v1/config/reload` take no body
    // extractor, so they are CORS-*simple* requests that no preflight ever
    // sees, and `RequireAdmin` returns `Ok` when auth is off (the shipped
    // default). Any web page the operator loaded could take a live flow off
    // air. The WHIP/WHEP sub-router is deliberately merged at top level and
    // NOT caught by this — cross-origin POST/DELETE from a player page is its
    // documented mode of operation and it carries its own per-flow
    // `bearer_token` check.
    let protected_routes = Router::new()
        .merge(read_routes)
        .merge(write_routes)
        .route_layer(middleware::from_fn_with_state(
            private_api_policy,
            guard_cross_origin_write,
        ))
        .route_layer(middleware::from_fn_with_state(
            auth_state.clone(),
            auth::auth_middleware,
        ));

    // NMOS IS-04, IS-05, and IS-08 routes — optionally protected by JWT auth.
    // Default is public (no auth) for backward compatibility with NMOS controllers.
    let nmos_routes = Router::new()
        .nest("/x-nmos/node/v1.3", nmos::nmos_node_router())
        .nest("/x-nmos/connection/v1.1", nmos_is05::nmos_connection_router())
        .nest("/x-nmos/channelmapping/v1.0", nmos_is08::nmos_is08_router())
        // Innermost of the three NMOS layers: it reads the Claims that the
        // auth middleware inserts, so it must run after it.
        .route_layer(middleware::from_fn_with_state(
            nmos_policy.clone(),
            guard_cross_origin_write,
        ));

    let nmos_routes = if auth_state
        .as_ref()
        .is_some_and(|a| a.config.nmos_require_auth_effective())
    {
        tracing::info!("NMOS endpoints require JWT Bearer authentication");
        nmos_routes.route_layer(middleware::from_fn_with_state(
            auth_state.clone(),
            auth::auth_middleware,
        ))
    } else {
        nmos_routes
    };

    // Outermost NMOS layer, so a preflight is answered before auth or the
    // write guard — a preflight carries no credentials and must not 401.
    let nmos_routes = nmos_routes.layer(nmos_cors(nmos_policy.grants_browser_writes()));

    // Merge everything.
    //
    // Note the absence of a router-wide CORS layer: only `/x-nmos/**` and the
    // WHIP/WHEP routes send any `Access-Control-*` header. The explicit
    // `.fallback` is part of that: `Router::merge` otherwise adopts the
    // *layered* default fallback of the last-merged sub-router, which put a
    // CORS layer on every unmatched path. See the "Browser-origin policy"
    // section above.
    let app = Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        .merge(nmos_routes);
    #[cfg(feature = "webrtc")]
    let app = app.merge(whip_whep_routes);
    app.fallback(not_found)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod browser_origin_tests {
    //! The edge API shipped `CorsLayer::permissive()` on every route while
    //! auth is off by default, which made `GET /api/v1/config` (SRT
    //! passphrases, RTMP stream keys) readable by any web page the operator
    //! loaded, and made the IS-05 activation surface — which re-points a live
    //! sender and persists the change — reachable from one too.
    //!
    //! These tests drive the real router over a real socket, because the
    //! defect was in *layer composition*, not in any single function: the
    //! wildcard came from a layer at the bottom of `build_router`, and the
    //! NMOS read surface has to keep its wildcard while the write surface
    //! loses it.

    use super::*;
    use crate::config::models::AppConfig;
    use std::net::SocketAddr;

    const EVIL: &str = "https://evil.example";
    /// Any syntactically valid UUID; no sender by this id exists, so a request
    /// that survives the guard lands on the handler's 404 — which is exactly
    /// the signal that distinguishes "refused" from "reached the handler".
    const UNKNOWN_SENDER: &str = "11111111-2222-3333-4444-555555555555";
    /// ≥ 32 chars, as `validate_config` requires of a real one.
    const JWT_SECRET: &str = "test-jwt-secret-at-least-32-chars-long";
    /// Stands in for a browser-hosted NMOS controller (sony/nmos-js).
    const CONTROLLER: &str = "https://nmos-js.example.tv";

    fn test_state(dir: &std::path::Path) -> AppState {
        let stats = std::sync::Arc::new(crate::stats::collector::StatsCollector::new());
        let (event_sender, rx) = crate::manager::events::event_channel();
        // Keep the receiver alive so best-effort emits aren't send errors.
        Box::leak(Box::new(rx));
        let resource_state =
            std::sync::Arc::new(crate::engine::resource_monitor::SystemResourceState::new());
        let flow_manager = std::sync::Arc::new(FlowManager::new(
            stats,
            false,
            event_sender.clone(),
            resource_state.clone(),
            None,
            None,
            #[cfg(all(feature = "display", target_os = "linux"))]
            crate::display::claim_registry::DisplayClaimRegistry::new(),
            #[cfg(feature = "webrtc")]
            None,
        ));
        let (ws_stats_tx, _ws_rx) = broadcast::channel(4);
        AppState {
            config: Arc::new(RwLock::new(AppConfig::default())),
            config_path: dir.join("config.json"),
            secrets_path: dir.join("secrets.json"),
            flow_manager,
            tunnel_manager: Arc::new(TunnelManager::new(event_sender.clone())),
            start_time: Instant::now(),
            ws_stats_tx,
            // Auth disabled — the shipped default, and the condition under
            // which the wildcard was exploitable.
            auth_state: None,
            is05_state: Arc::new(Is05State::new()),
            is08_state: Is08State::load_or_default(dir.join("nmos_channel_map.json")),
            #[cfg(feature = "webrtc")]
            webrtc_sessions: None,
            event_sender: Some(event_sender),
            resource_state,
            standby_listeners: None,
            token_rate_limiter: None,
            manager_link: crate::manager::link_state::ManagerLinkState::new(false),
            ptp_node_state: crate::engine::st2110::ptp::PtpStateHandle::new(0),
        }
    }

    /// Turn auth on for a fixture state, exactly as `main.rs` does from
    /// `server.auth`. `nmos_require_auth` is left unset, so NMOS inherits the
    /// secure-by-default `true`.
    fn with_auth(mut state: AppState) -> AppState {
        state.auth_state = Some(Arc::new(auth::AuthState::new(auth::AuthConfig {
            enabled: true,
            jwt_secret: JWT_SECRET.to_string(),
            token_lifetime_secs: 3600,
            clients: vec![auth::AuthClient {
                client_id: "ci".into(),
                client_secret: "ci-secret".into(),
                role: "admin".into(),
            }],
            public_metrics: true,
            nmos_require_auth: None,
            token_rate_limit_per_minute: 10,
        })));
        state
    }

    /// Mint a valid admin Bearer token for the auth-enabled fixture.
    fn admin_token() -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        auth::jwt_encode(
            &auth::Claims {
                sub: "ci".into(),
                role: "admin".into(),
                iat: now,
                exp: now + 3600,
                iss: "bilbycast-edge".into(),
            },
            JWT_SECRET.as_bytes(),
        )
        .expect("encode")
    }

    /// Bind the real router on an ephemeral loopback port and return its base
    /// URL. The `TempDir` is returned so the caller keeps it alive.
    async fn serve() -> (String, tempfile::TempDir) {
        serve_with(false, &[]).await
    }

    /// As [`serve`], but lets a test pick the two axes the shipped default
    /// pins: whether `server.auth` is on, and which browser origins
    /// `server.nmos_browser_control` admits.
    async fn serve_with(
        auth_enabled: bool,
        nmos_browser_control: &[String],
    ) -> (String, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = test_state(dir.path());
        let state = if auth_enabled { with_auth(state) } else { state };
        let router = build_router(state, nmos_browser_control);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });
        (format!("http://{addr}"), dir)
    }

    fn header(resp: &reqwest::Response, name: &str) -> Option<String> {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    // ── The private API must not be readable cross-origin ──

    #[tokio::test]
    async fn private_api_sends_no_cors_header() {
        let (base, _dir) = serve().await;
        let client = reqwest::Client::new();
        // `/api/v1/config` is the one that leaks credentials, but the whole
        // private surface is covered — a wildcard on `/oauth/token` would let
        // a foreign page mint tokens once auth is enabled.
        for path in [
            "/api/v1/config",
            "/api/v1/stats",
            "/api/v1/inputs",
            "/health",
            "/metrics",
            // An unmatched path. `Router::merge` adopts the *layered* default
            // fallback of the last sub-router merged, so before the explicit
            // `.fallback(not_found)` this answered with the NMOS (or WHIP)
            // CORS layer's wildcard — the module comment claimed "everything
            // else emits none" while only existing routes were ever probed.
            "/nope",
        ] {
            let resp = client
                .get(format!("{base}{path}"))
                .header("origin", EVIL)
                .send()
                .await
                .expect("request");
            assert_eq!(
                header(&resp, "access-control-allow-origin"),
                None,
                "{path} must not tell a browser that {EVIL} may read its body"
            );
        }

        let resp = client
            .request(reqwest::Method::OPTIONS, format!("{base}/nope"))
            .header("origin", EVIL)
            .header("access-control-request-method", "GET")
            .send()
            .await
            .expect("request");
        assert_eq!(
            header(&resp, "access-control-allow-origin"),
            None,
            "an unmatched path must not answer a preflight at all"
        );
        assert_eq!(header(&resp, "access-control-allow-methods"), None);
    }

    #[tokio::test]
    async fn private_api_preflight_grants_nothing() {
        let (base, _dir) = serve().await;
        let resp = reqwest::Client::new()
            .request(reqwest::Method::OPTIONS, format!("{base}/api/v1/config"))
            .header("origin", EVIL)
            .header("access-control-request-method", "PUT")
            .send()
            .await
            .expect("request");
        assert_eq!(header(&resp, "access-control-allow-origin"), None);
        assert_eq!(header(&resp, "access-control-allow-methods"), None);
    }

    // ── NMOS discovery stays browser-reachable (AMWA requires CORS) ──

    #[tokio::test]
    async fn nmos_discovery_keeps_its_wildcard() {
        let (base, _dir) = serve().await;
        let client = reqwest::Client::new();
        for path in [
            "/x-nmos/node/v1.3/self",
            "/x-nmos/connection/v1.1/single/senders",
            "/x-nmos/channelmapping/v1.0/io",
        ] {
            let resp = client
                .get(format!("{base}{path}"))
                .header("origin", EVIL)
                .send()
                .await
                .expect("request");
            assert_eq!(
                header(&resp, "access-control-allow-origin").as_deref(),
                Some("*"),
                "{path} is public by NMOS specification and must stay readable \
                 by a browser-hosted controller"
            );
        }
    }

    #[tokio::test]
    async fn nmos_read_preflight_is_granted() {
        let (base, _dir) = serve().await;
        let resp = reqwest::Client::new()
            .request(
                reqwest::Method::OPTIONS,
                format!("{base}/x-nmos/node/v1.3/self"),
            )
            .header("origin", EVIL)
            .header("access-control-request-method", "GET")
            .send()
            .await
            .expect("request");
        let methods = header(&resp, "access-control-allow-methods")
            .unwrap_or_default()
            .to_ascii_uppercase();
        assert!(
            methods.contains("GET"),
            "NMOS GET preflight must succeed, got '{methods}'"
        );
    }

    // ── …but the NMOS write surface does not ──

    #[tokio::test]
    async fn nmos_write_preflight_is_refused() {
        let (base, _dir) = serve().await;
        let client = reqwest::Client::new();
        for (path, method) in [
            (
                format!("/x-nmos/connection/v1.1/single/senders/{UNKNOWN_SENDER}/staged"),
                "PATCH",
            ),
            (
                "/x-nmos/channelmapping/v1.0/map/activate".to_string(),
                "POST",
            ),
        ] {
            let resp = client
                .request(reqwest::Method::OPTIONS, format!("{base}{path}"))
                .header("origin", EVIL)
                .header("access-control-request-method", method)
                .send()
                .await
                .expect("request");
            let methods = header(&resp, "access-control-allow-methods")
                .unwrap_or_default()
                .to_ascii_uppercase();
            assert!(
                !methods.contains(method) && !methods.contains('*'),
                "{path} preflight must not grant {method} to a foreign origin, \
                 got '{methods}'"
            );
        }
    }

    #[tokio::test]
    async fn unauthenticated_cross_origin_nmos_write_is_refused() {
        let (base, _dir) = serve().await;
        let client = reqwest::Client::new();

        let resp = client
            .patch(format!(
                "{base}/x-nmos/connection/v1.1/single/senders/{UNKNOWN_SENDER}/staged"
            ))
            .header("origin", EVIL)
            .header("content-type", "application/json")
            .body(r#"{"transport_params":[{"destination_ip":"203.0.113.9"}]}"#)
            .send()
            .await
            .expect("request");
        assert_eq!(
            resp.status().as_u16(),
            403,
            "an IS-05 activation carrying a foreign browser Origin must never \
             reach the handler"
        );

        let resp = client
            .post(format!("{base}/x-nmos/channelmapping/v1.0/map/activate"))
            .header("origin", EVIL)
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("request");
        assert_eq!(
            resp.status().as_u16(),
            403,
            "the guard must cover IS-08 activation too"
        );
    }

    // ── Native controllers keep working ──

    #[tokio::test]
    async fn nmos_write_without_an_origin_header_reaches_the_handler() {
        let (base, _dir) = serve().await;
        let resp = reqwest::Client::new()
            .patch(format!(
                "{base}/x-nmos/connection/v1.1/single/senders/{UNKNOWN_SENDER}/staged"
            ))
            .header("content-type", "application/json")
            .body(r#"{"transport_params":[{"destination_ip":"203.0.113.9"}]}"#)
            .send()
            .await
            .expect("request");
        // 404: no such sender on this node. The point is that it is the
        // handler answering, not the guard — a Sony / Riedel / Lawo
        // controller sends no Origin and must be untouched by this change.
        assert_eq!(
            resp.status().as_u16(),
            404,
            "a non-browser IS-05 controller must not be refused"
        );
    }

    #[tokio::test]
    async fn same_origin_nmos_write_reaches_the_handler() {
        let (base, _dir) = serve().await;
        let host = base.trim_start_matches("http://").to_string();
        let resp = reqwest::Client::new()
            .patch(format!(
                "{base}/x-nmos/connection/v1.1/single/senders/{UNKNOWN_SENDER}/staged"
            ))
            .header("origin", format!("http://{host}"))
            .header("content-type", "application/json")
            .body(r#"{"transport_params":[]}"#)
            .send()
            .await
            .expect("request");
        assert_eq!(resp.status().as_u16(), 404);
    }

    // ── WHIP / WHEP keep the browser access they are documented to have ──

    /// `docs/supported-protocols.md` sells WHIP ingest "from OBS, browsers"
    /// and WHEP playout to "browser viewers". A player page is essentially
    /// never on the edge's own origin, and `application/sdp` is not a
    /// CORS-safelisted content type, so every browser request here is
    /// preflighted — dropping CORS from these routes would silently break a
    /// shipped feature.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn whip_whep_preflight_is_granted() {
        let (base, _dir) = serve().await;
        let client = reqwest::Client::new();
        for (path, method) in [
            ("/api/v1/flows/f1/whep", "POST"),
            ("/api/v1/flows/f1/whep/s1", "DELETE"),
            ("/api/v1/flows/f1/whip", "POST"),
            ("/api/v1/flows/f1/whip/s1", "DELETE"),
        ] {
            let resp = client
                .request(reqwest::Method::OPTIONS, format!("{base}{path}"))
                .header("origin", "https://player.example")
                .header("access-control-request-method", method)
                .header("access-control-request-headers", "content-type,authorization")
                .send()
                .await
                .expect("request");
            assert_eq!(
                header(&resp, "access-control-allow-origin").as_deref(),
                Some("*"),
                "{path} must stay reachable from a browser player page"
            );
            let methods = header(&resp, "access-control-allow-methods")
                .unwrap_or_default()
                .to_ascii_uppercase();
            assert!(
                methods.contains(method),
                "{path} preflight must grant {method}, got '{methods}'"
            );
            let allowed = header(&resp, "access-control-allow-headers")
                .unwrap_or_default()
                .to_ascii_lowercase();
            assert!(
                allowed.contains("content-type") && allowed.contains("authorization"),
                "WHIP/WHEP need application/sdp plus the per-flow bearer token, \
                 got '{allowed}'"
            );
        }
    }

    /// A WHIP/WHEP client cannot tear its own session down without reading the
    /// `Location` header off the 201, so it has to stay exposed.
    ///
    /// This also pins the other half of the carve-out, which a preflight test
    /// cannot reach: `OPTIONS` is a safe method and passes
    /// [`guard_cross_origin_write`] unconditionally, so a green preflight says
    /// nothing about the **real** request. A browser stamps `Sec-Fetch-*` on
    /// that POST, which is precisely the fingerprint the guard refuses
    /// everywhere else — so if `whip_whep_routes` ever stopped being merged at
    /// top level and picked the guard up, every browser player would break
    /// with a 403 while both preflight tests stayed green.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn whip_whep_expose_location() {
        let (base, _dir) = serve().await;
        // `Access-Control-Expose-Headers` rides the *actual* response, never
        // the preflight, so this has to be a real POST. Flow `f1` is not
        // running in this fixture, so the handler answers 404 — the header is
        // what is under test, not the status.
        let resp = reqwest::Client::new()
            .post(format!("{base}/api/v1/flows/f1/whep"))
            .header("origin", "https://player.example")
            .header("sec-fetch-site", "cross-site")
            .header("sec-fetch-mode", "cors")
            .header("sec-fetch-dest", "empty")
            .header("content-type", "application/sdp")
            .body("v=0\r\n")
            .send()
            .await
            .expect("request");
        assert_ne!(
            resp.status().as_u16(),
            403,
            "the real browser POST — not just its preflight — must reach the \
             WHIP/WHEP handler; a 403 here means the cross-origin write guard \
             caught the carve-out and every browser player is broken"
        );
        let exposed = header(&resp, "access-control-expose-headers")
            .unwrap_or_default()
            .to_ascii_lowercase();
        assert!(
            exposed.contains("location"),
            "Location must be readable cross-origin, got '{exposed}'"
        );
    }

    /// The carve-out is exactly four routes wide: a sibling under the same
    /// `/api/v1/flows/{id}/` prefix must not inherit it.
    #[tokio::test]
    async fn the_whip_whep_carve_out_does_not_leak_to_neighbours() {
        let (base, _dir) = serve().await;
        let resp = reqwest::Client::new()
            .request(
                reqwest::Method::OPTIONS,
                format!("{base}/api/v1/flows/f1/activate-input"),
            )
            .header("origin", "https://player.example")
            .header("access-control-request-method", "POST")
            .send()
            .await
            .expect("request");
        assert_eq!(header(&resp, "access-control-allow-origin"), None);
    }

    // ── The private `/api/v1` mutation surface is guarded too ──

    /// `POST /api/v1/config/reload` and `POST /api/v1/flows/{id}/stop` take no
    /// body extractor, so a cross-origin `fetch(..., {method:'POST',
    /// mode:'no-cors'})` is a CORS-**simple** request: never preflighted, so
    /// removing the CORS layer does not stop it, and `RequireAdmin` returns
    /// `Ok` when auth is off — the shipped default. Before the guard was
    /// mounted here, `config/reload` answered 200 and actually reloaded, which
    /// calls `stop_all()` — every flow on the node off air from a foreign web
    /// page.
    #[tokio::test]
    async fn cross_origin_private_api_writes_are_refused() {
        let (base, _dir) = serve().await;
        let client = reqwest::Client::new();
        for path in [
            "/api/v1/config/reload",
            "/api/v1/flows/f1/stop",
            "/api/v1/flows/f1/start",
            "/api/v1/flows/f1/restart",
        ] {
            let resp = client
                .post(format!("{base}{path}"))
                .header("origin", EVIL)
                .send()
                .await
                .expect("request");
            assert_eq!(
                resp.status().as_u16(),
                403,
                "{path} must not be reachable from a foreign web page"
            );
        }
    }

    /// The manager, `curl` and every CI script reach this surface with no
    /// `Origin` and no `Sec-Fetch-*`; the guard must be invisible to them.
    #[tokio::test]
    async fn private_api_writes_without_a_browser_fingerprint_reach_the_handler() {
        let (base, _dir) = serve().await;
        let client = reqwest::Client::new();

        let resp = client
            .post(format!("{base}/api/v1/flows/f1/stop"))
            .send()
            .await
            .expect("request");
        assert_eq!(
            resp.status().as_u16(),
            404,
            "no such flow — the point is that the handler answered, not the guard"
        );

        // Same-origin, and pre-`Sec-Fetch-*` in shape: still admitted.
        let host = base.trim_start_matches("http://").to_string();
        let resp = client
            .post(format!("{base}/api/v1/flows/f1/stop"))
            .header("origin", format!("http://{host}"))
            .send()
            .await
            .expect("request");
        assert_ne!(
            resp.status().as_u16(),
            403,
            "a same-origin caller with no browser fetch-metadata must not be refused"
        );
    }

    // ── DNS rebinding: a browser is refused even when Origin == Host ──

    /// `origin_matches_host` compares two attacker-controlled headers. Under
    /// DNS rebinding a page on `evil.example` re-resolves to the node's LAN
    /// address, so the browser sends `Origin: http://evil.example` **and**
    /// addresses the request to that same authority — they agree, and a
    /// same-origin test passes the forgery. `Sec-Fetch-*` is the header a page
    /// cannot forge and a native controller never sends, so the guard keys on
    /// "a browser issued this" instead.
    #[tokio::test]
    async fn a_browser_is_refused_even_when_it_looks_same_origin() {
        let (base, _dir) = serve().await;
        let host = base.trim_start_matches("http://").to_string();
        let resp = reqwest::Client::new()
            .patch(format!(
                "{base}/x-nmos/connection/v1.1/single/senders/{UNKNOWN_SENDER}/staged"
            ))
            .header("origin", format!("http://{host}"))
            .header("sec-fetch-site", "same-origin")
            .header("sec-fetch-mode", "cors")
            .header("content-type", "application/json")
            .body(r#"{"transport_params":[]}"#)
            .send()
            .await
            .expect("request");
        assert_eq!(
            resp.status().as_u16(),
            403,
            "a same-origin-looking browser write is exactly the DNS-rebinding shape"
        );
    }

    // ── The browser-control escape hatch ──

    /// Without a config opt-out a browser-hosted NMOS controller
    /// (sony/nmos-js PATCHes `.../staged` and `.../activate` directly on the
    /// node) has no working path at all: auth-off nodes never insert `Claims`,
    /// so the "authenticate instead" advice is unreachable.
    /// `server.nmos_browser_control` is that opt-out.
    #[tokio::test]
    async fn a_listed_browser_origin_may_drive_nmos_connection_management() {
        let (base, _dir) = serve_with(false, &[CONTROLLER.to_string()]).await;
        let client = reqwest::Client::new();

        // The preflight now advertises PATCH…
        let resp = client
            .request(
                reqwest::Method::OPTIONS,
                format!("{base}/x-nmos/connection/v1.1/single/senders/{UNKNOWN_SENDER}/staged"),
            )
            .header("origin", CONTROLLER)
            .header("access-control-request-method", "PATCH")
            .header("access-control-request-headers", "content-type")
            .send()
            .await
            .expect("request");
        let methods = header(&resp, "access-control-allow-methods")
            .unwrap_or_default()
            .to_ascii_uppercase();
        assert!(
            methods.contains("PATCH"),
            "a configured controller must be able to preflight a PATCH, got '{methods}'"
        );

        // …and the real request reaches the handler.
        let resp = client
            .patch(format!(
                "{base}/x-nmos/connection/v1.1/single/senders/{UNKNOWN_SENDER}/staged"
            ))
            .header("origin", CONTROLLER)
            .header("sec-fetch-site", "cross-site")
            .header("content-type", "application/json")
            .body(r#"{"transport_params":[]}"#)
            .send()
            .await
            .expect("request");
        assert_eq!(resp.status().as_u16(), 404, "handler, not guard");

        // An origin that is not on the list is still refused, so the opt-in
        // is per-origin and not a re-introduced wildcard.
        let resp = client
            .patch(format!(
                "{base}/x-nmos/connection/v1.1/single/senders/{UNKNOWN_SENDER}/staged"
            ))
            .header("origin", EVIL)
            .header("sec-fetch-site", "cross-site")
            .header("content-type", "application/json")
            .body(r#"{"transport_params":[]}"#)
            .send()
            .await
            .expect("request");
        assert_eq!(resp.status().as_u16(), 403);
    }

    /// The opt-in is NMOS-only. `/api/v1` has no browser client that writes,
    /// so listing an origin must not hand it the flow-control surface.
    #[tokio::test]
    async fn the_nmos_opt_in_does_not_widen_the_private_api() {
        let (base, _dir) = serve_with(false, &[CONTROLLER.to_string()]).await;
        let resp = reqwest::Client::new()
            .post(format!("{base}/api/v1/flows/f1/stop"))
            .header("origin", CONTROLLER)
            .header("sec-fetch-site", "cross-site")
            .send()
            .await
            .expect("request");
        assert_eq!(resp.status().as_u16(), 403);
    }

    // ── The authenticated half of the router ──

    /// The regression guard for the layer ordering. `whip_whep_cors()` has to
    /// sit OUTSIDE the auth middleware: a preflight carries no credentials by
    /// definition, so an auth layer wrapped around the CORS layer answers it
    /// 401 and the browser never sends the POST. Merging the WHIP/WHEP routes
    /// into `write_routes` did exactly that — measured 401 with no CORS
    /// headers — while every test in this module built `auth_state: None` and
    /// so could not see it.
    #[cfg(feature = "webrtc")]
    #[tokio::test]
    async fn whip_whep_preflight_survives_an_auth_enabled_node() {
        let (base, _dir) = serve_with(true, &[]).await;
        let client = reqwest::Client::new();
        for (path, method) in [
            ("/api/v1/flows/f1/whep", "POST"),
            ("/api/v1/flows/f1/whip", "POST"),
            ("/api/v1/flows/f1/whep/s1", "DELETE"),
            ("/api/v1/flows/f1/whip/s1", "DELETE"),
        ] {
            let resp = client
                .request(reqwest::Method::OPTIONS, format!("{base}{path}"))
                .header("origin", "https://player.example")
                .header("access-control-request-method", method)
                .header("access-control-request-headers", "content-type,authorization")
                .send()
                .await
                .expect("request");
            assert_eq!(
                resp.status().as_u16(),
                200,
                "{path} preflight must be answered by CORS, not 401'd by auth"
            );
            assert_eq!(
                header(&resp, "access-control-allow-origin").as_deref(),
                Some("*"),
                "{path} preflight must carry the wildcard on an auth-enabled node"
            );
        }
    }

    /// An auth-enabled node makes `Authorization` mandatory on every NMOS
    /// request, which takes even a plain `GET` out of the CORS-simple class.
    /// If the preflight does not list `authorization`, a browser-hosted
    /// controller cannot attach the token and the node is unusable from one.
    #[tokio::test]
    async fn nmos_preflight_admits_authorization_on_an_auth_enabled_node() {
        let (base, _dir) = serve_with(true, &[]).await;
        let resp = reqwest::Client::new()
            .request(
                reqwest::Method::OPTIONS,
                format!("{base}/x-nmos/node/v1.3/self"),
            )
            .header("origin", CONTROLLER)
            .header("access-control-request-method", "GET")
            .header("access-control-request-headers", "authorization")
            .send()
            .await
            .expect("request");
        assert_eq!(
            resp.status().as_u16(),
            200,
            "a preflight carries no credentials and must not be 401'd"
        );
        let allowed = header(&resp, "access-control-allow-headers")
            .unwrap_or_default()
            .to_ascii_lowercase();
        assert!(
            allowed.contains("authorization"),
            "without `authorization` a browser controller cannot send its Bearer \
             token, got '{allowed}'"
        );
        assert!(
            allowed.contains("content-type"),
            "AMWA's CORS guidance asks for Content-Type too, got '{allowed}'"
        );
    }

    /// The `Claims` pass-through branch, which was never exercised: it is
    /// reachable only on an auth-enabled node, and it is what lets an
    /// authenticated controller keep connection management from any origin.
    #[tokio::test]
    async fn an_authenticated_cross_origin_nmos_write_passes_the_guard() {
        let (base, _dir) = serve_with(true, &[]).await;
        let client = reqwest::Client::new();
        let path = format!(
            "{base}/x-nmos/connection/v1.1/single/senders/{UNKNOWN_SENDER}/staged"
        );

        let resp = client
            .patch(&path)
            .header("origin", EVIL)
            .header("sec-fetch-site", "cross-site")
            .header("authorization", format!("Bearer {}", admin_token()))
            .header("content-type", "application/json")
            .body(r#"{"transport_params":[]}"#)
            .send()
            .await
            .expect("request");
        assert_eq!(
            resp.status().as_u16(),
            404,
            "a validated Bearer token is not something a foreign page can attach, \
             so the guard must let it through to the handler"
        );

        // Same request without the token is 401 from auth (not 403 from the
        // guard) — the two layers are ordered auth-outside-guard.
        let resp = client
            .patch(&path)
            .header("origin", EVIL)
            .header("sec-fetch-site", "cross-site")
            .header("content-type", "application/json")
            .body(r#"{"transport_params":[]}"#)
            .send()
            .await
            .expect("request");
        assert_eq!(resp.status().as_u16(), 401);
    }

    // ── Pure helpers ──

    #[test]
    fn browser_write_policy_matches_origins_case_insensitively() {
        let policy = BrowserWritePolicy::new(&[CONTROLLER.to_string()]);
        assert!(policy.grants_browser_writes());
        assert!(policy.permits(CONTROLLER));
        assert!(policy.permits("HTTPS://NMOS-JS.EXAMPLE.TV"));
        assert!(!policy.permits(EVIL));
        // A trailing slash is not what a browser sends in `Origin`; validation
        // rejects it in config, and it must not match here either.
        assert!(!policy.permits("https://nmos-js.example.tv/"));

        let default = BrowserWritePolicy::default();
        assert!(!default.grants_browser_writes());
        assert!(!default.permits(CONTROLLER));
    }

    #[test]
    fn safe_methods_are_exactly_the_read_methods() {
        assert!(is_safe_method(&Method::GET));
        assert!(is_safe_method(&Method::HEAD));
        assert!(is_safe_method(&Method::OPTIONS));
        assert!(!is_safe_method(&Method::PATCH));
        assert!(!is_safe_method(&Method::POST));
        assert!(!is_safe_method(&Method::PUT));
        assert!(!is_safe_method(&Method::DELETE));
    }

    #[test]
    fn origin_host_comparison_is_authority_only() {
        assert!(origin_matches_host("http://edge.local:8080", Some("edge.local:8080")));
        assert!(origin_matches_host("https://EDGE.local:8080", Some("edge.local:8080")));
        assert!(!origin_matches_host("https://evil.example", Some("edge.local:8080")));
        // A sandboxed iframe / `file://` page sends `Origin: null`; it is not
        // the node's own origin and must not be treated as one.
        assert!(!origin_matches_host("null", Some("edge.local:8080")));
        assert!(!origin_matches_host("http://edge.local:8080", None));
        assert!(!origin_matches_host("http://", Some("edge.local:8080")));
    }

    /// axum-server sets `alpn_protocols = ["h2", "http/1.1"]`, so every TLS
    /// deployment serves browsers over HTTP/2 — where the authority rides the
    /// `:authority` pseudo-header, which hyper puts on the request URI and
    /// does **not** mirror into a `Host` header. Reading `Host` alone left the
    /// same-origin branch dead on every HTTPS node.
    #[test]
    fn request_authority_reads_the_http2_authority_and_falls_back_to_host() {
        // HTTP/2 shape: absolute URI, no Host header.
        let h2 = Request::builder()
            .uri("https://edge.local:8443/x-nmos/node/v1.3/self")
            .body(axum::body::Body::empty())
            .expect("build");
        assert_eq!(request_authority(&h2), Some("edge.local:8443"));
        assert!(origin_matches_host(
            "https://edge.local:8443",
            request_authority(&h2)
        ));

        // HTTP/1.1 shape: origin-form URI, authority in Host.
        let h1 = Request::builder()
            .uri("/x-nmos/node/v1.3/self")
            .header(header::HOST, "edge.local:8080")
            .body(axum::body::Body::empty())
            .expect("build");
        assert_eq!(request_authority(&h1), Some("edge.local:8080"));

        // Neither: fails closed.
        let bare = Request::builder()
            .uri("/x-nmos/node/v1.3/self")
            .body(axum::body::Body::empty())
            .expect("build");
        assert_eq!(request_authority(&bare), None);
    }
}

