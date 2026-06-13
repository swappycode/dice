//! `ClientCore`: the host brain. Every Tauri command body lives here as a
//! PLAIN async fn so the whole surface is testable headless (no webview, no
//! Tauri runtime) — `src/commands` is a thin `#[tauri::command]` shim.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use dice_network_core::client::url::Url;
use dice_network_core::client::{
    ApiClient, ApiError, Command, ConnStateLite, GatewayClientConfig, PreferredTransport,
    QuicEndpoint, TlsOptions, TransportPolicy, connect,
};
use dice_protocol::v1;
use dice_protocol::v1::frame::Payload;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::bridge::{Bridge, PendingMap, PendingSend, PresenceMap, conn_state_str};
use crate::cache::{Cache, CacheError};
use crate::dto::{
    BootstrapDto, ChannelDto, DiceEvent, GuildDto, MessageDto, SessionDto, UserDto, id_str,
    parse_id, parse_presence, presence_str,
};
use crate::emit::{Emitter, emit_dice};
use crate::keystore::KeyStore;
use crate::session::SessionManager;

/// Host-side typing throttle: at most one `StartTyping` per channel per 8 s.
pub const TYPING_THROTTLE: Duration = Duration::from_secs(8);
/// Outbound command relay depth (core → bridge → gateway driver).
const COMMAND_RELAY: usize = 64;
/// `get_bootstrap` waits at most this long for the first `Ready` to land in
/// the cache when the cache is still empty (fresh login on a new machine).
const BOOTSTRAP_READY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error(transparent)]
    Cache(#[from] CacheError),
    #[error("invalid id: {0:?}")]
    BadId(String),
    #[error("not logged in")]
    NoSession,
    #[error("not connected")]
    NotConnected,
    #[error("the gateway did not become ready in time")]
    NotReady,
    #[error("{0}")]
    Internal(String),
}

impl CoreError {
    /// What the webview should show (server-provided message when there is
    /// one; never internals like URLs).
    pub fn user_message(&self) -> String {
        match self {
            Self::Api(ApiError::Api { error, .. }) if !error.message.is_empty() => {
                error.message.clone()
            }
            other => other.to_string(),
        }
    }
}

impl From<anyhow::Error> for CoreError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

/// Endpoints + trust + cache location. Injectable for tests.
#[derive(Debug, Clone)]
pub struct CoreConfig {
    /// REST base, e.g. `https://localhost:8443`.
    pub api_url: Url,
    /// Gateway WSS endpoint, e.g. `wss://localhost:8443/gateway/v1`.
    pub wss_url: Url,
    /// QUIC gateway endpoint (UDP, ALPN dice/1). None ⇒ WSS only.
    pub quic: Option<QuicEndpoint>,
    /// Transport-selection policy (protocol §1 / design §1.3).
    pub policy: TransportPolicy,
    pub tls: TlsOptions,
    pub cache_path: PathBuf,
}

impl CoreConfig {
    /// `DICE_API_URL` / `DICE_GATEWAY_WSS` / `DICE_GATEWAY_QUIC` /
    /// `DICE_TRANSPORT` / `DICE_DEV_CA` with the dev defaults from
    /// docs/design/desktop-client.md §2.
    pub fn from_env(cache_path: PathBuf) -> anyhow::Result<Self> {
        let api =
            std::env::var("DICE_API_URL").unwrap_or_else(|_| "https://localhost:8443".to_owned());
        let wss = std::env::var("DICE_GATEWAY_WSS")
            .unwrap_or_else(|_| "wss://localhost:8443/gateway/v1".to_owned());
        let quic_ep =
            std::env::var("DICE_GATEWAY_QUIC").unwrap_or_else(|_| "localhost:8444".to_owned());
        let policy = transport_policy_from_env(std::env::var("DICE_TRANSPORT").ok().as_deref());
        // An unparseable endpoint only matters when QUIC may actually be used.
        let quic = match QuicEndpoint::from_host_port(&quic_ep) {
            Ok(ep) => Some(ep),
            Err(e) if matches!(policy, TransportPolicy::WssOnly) => {
                tracing::debug!(%e, "ignoring bad DICE_GATEWAY_QUIC under wss-only policy");
                None
            }
            Err(e) => return Err(anyhow::anyhow!("DICE_GATEWAY_QUIC invalid: {e}")),
        };
        Ok(Self {
            api_url: Url::parse(&api)?,
            wss_url: Url::parse(&wss)?,
            quic,
            policy,
            tls: TlsOptions::from_env(),
            cache_path,
        })
    }
}

/// `DICE_TRANSPORT`: `quic-first` (default) | `wss` | `quic`.
fn transport_policy_from_env(value: Option<&str>) -> TransportPolicy {
    match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("wss") | Some("wss-only") => TransportPolicy::WssOnly,
        Some("quic") | Some("quic-only") => TransportPolicy::QuicOnly,
        Some("quic-first") | None => TransportPolicy::default(),
        Some(other) => {
            tracing::warn!(%other, "unknown DICE_TRANSPORT, using quic-first");
            TransportPolicy::default()
        }
    }
}

/// One live gateway connection: the bridge's command relay, the driver's
/// state watch, and the bridge task itself.
struct GatewayCtl {
    cmds: mpsc::Sender<Command>,
    state: watch::Receiver<dice_network_core::client::ConnState>,
    task: JoinHandle<()>,
}

pub struct ClientCore {
    cfg: CoreConfig,
    /// Bearer-capable client (token provider = the session manager).
    api: ApiClient,
    session: Arc<SessionManager>,
    cache: Cache,
    emitter: Arc<dyn Emitter>,
    rt: tokio::runtime::Handle,
    gateway: StdMutex<Option<GatewayCtl>>,
    pending: PendingMap,
    presence: PresenceMap,
    current_user: Arc<StdMutex<Option<v1::User>>>,
    /// Bumped by the bridge AFTER each `Ready` snapshot hits the cache.
    ready_tx: Arc<watch::Sender<u64>>,
    ready_rx: watch::Receiver<u64>,
    typing: TypingThrottle,
    nonce_seq: AtomicU64,
}

impl ClientCore {
    pub fn new(
        cfg: CoreConfig,
        keys: Arc<dyn KeyStore>,
        emitter: Arc<dyn Emitter>,
        rt: tokio::runtime::Handle,
    ) -> anyhow::Result<Self> {
        let bare = ApiClient::new(cfg.api_url.clone(), &cfg.tls)?;
        let session = Arc::new(SessionManager::new(bare.clone(), keys));
        let api = bare.with_token_provider(session.clone());
        let cache = Cache::open(&cfg.cache_path)?;
        let (ready_tx, ready_rx) = watch::channel(0u64);
        // Wire nonces only need per-session uniqueness; seed off the clock so
        // restarts never collide with a server still draining old nonces.
        let seed = (dice_common::time::now_ms() << 20) | 1;
        Ok(Self {
            cfg,
            api,
            session,
            cache,
            emitter,
            rt,
            gateway: StdMutex::new(None),
            pending: PendingMap::default(),
            presence: PresenceMap::default(),
            current_user: Arc::default(),
            ready_tx: Arc::new(ready_tx),
            ready_rx,
            typing: TypingThrottle::default(),
            nonce_seq: AtomicU64::new(seed),
        })
    }

    /// Test/diagnostic access to the cache.
    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    pub fn has_stored_session(&self) -> bool {
        self.session.has_stored_session()
    }

    // ------------------------------------------------------------- gateway

    /// Spawn the gateway driver + bridge if none is running. Idempotent.
    /// Async only to read the persisted last-good transport from cache meta
    /// (so a WSS-bound network doesn't pay the QUIC probe timeout again).
    pub async fn ensure_gateway(&self) {
        {
            let slot = self.gateway.lock().expect("gateway lock");
            if let Some(ctl) = slot.as_ref()
                && !ctl.task.is_finished()
            {
                return;
            }
        }
        let initial_preference = self
            .cache
            .get_meta("last_transport".to_owned())
            .await
            .ok()
            .flatten()
            .and_then(|v| PreferredTransport::from_name(&v));
        let mut slot = self.gateway.lock().expect("gateway lock");
        if let Some(ctl) = slot.as_ref()
            && !ctl.task.is_finished()
        {
            return; // raced with another caller while we awaited
        }
        let handle = connect(
            GatewayClientConfig {
                wss_url: self.cfg.wss_url.clone(),
                quic: self.cfg.quic.clone(),
                policy: self.cfg.policy,
                initial_preference,
                tls: self.cfg.tls.clone(),
                token: self.session.clone(),
                properties: v1::ClientProperties {
                    client: "dice-desktop".to_owned(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                    os: std::env::consts::OS.to_owned(),
                },
            },
            self.rt.clone(),
        );
        let state = handle.state();
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_RELAY);
        let bridge = Bridge::new(
            self.cache.clone(),
            Arc::clone(&self.emitter),
            Arc::clone(&self.session),
            Arc::clone(&self.presence),
            Arc::clone(&self.pending),
            Arc::clone(&self.current_user),
            Arc::clone(&self.ready_tx),
            self.rt.clone(),
        );
        let task = self.rt.spawn(bridge.run(handle, cmd_rx));
        *slot = Some(GatewayCtl {
            cmds: cmd_tx,
            state,
            task,
        });
    }

    /// Stop the driver + bridge cleanly (logout, app exit, tests).
    pub async fn shutdown_gateway(&self) {
        let ctl = self.gateway.lock().expect("gateway lock").take();
        if let Some(ctl) = ctl {
            if ctl.cmds.send(Command::Shutdown).await.is_ok() {
                if tokio::time::timeout(Duration::from_secs(5), ctl.task)
                    .await
                    .is_err()
                {
                    tracing::warn!("bridge task did not stop within 5 s");
                }
            } else {
                ctl.task.abort();
            }
        }
    }

    fn gateway_cmds(&self) -> Option<mpsc::Sender<Command>> {
        self.gateway
            .lock()
            .expect("gateway lock")
            .as_ref()
            .map(|c| c.cmds.clone())
    }

    /// The frontend `ConnState` vocabulary for the current driver state.
    pub fn connection_state(&self) -> String {
        let slot = self.gateway.lock().expect("gateway lock");
        match slot.as_ref() {
            Some(ctl) => conn_state_str(ConnStateLite::from(&*ctl.state.borrow())).to_owned(),
            None => "idle".to_owned(),
        }
    }

    // ---------------------------------------------------------------- auth

    pub async fn login(&self, email: &str, password: &str) -> Result<SessionDto, CoreError> {
        let auth = self.api.login(email, password).await?;
        self.adopt_auth(auth).await
    }

    pub async fn register(
        &self,
        email: &str,
        username: &str,
        password: &str,
    ) -> Result<SessionDto, CoreError> {
        let auth = self.api.register(email, username, password).await?;
        self.adopt_auth(auth).await
    }

    async fn adopt_auth(&self, auth: v1::AuthSuccess) -> Result<SessionDto, CoreError> {
        let user = auth
            .user
            .clone()
            .ok_or_else(|| CoreError::Internal("auth response carried no user".to_owned()))?;
        self.session.install(&auth).await;
        *self.current_user.lock().expect("user lock") = Some(user.clone());
        self.cache.set_current_user(user.clone()).await?;
        self.ensure_gateway().await;
        Ok(SessionDto {
            user: UserDto::from(&user),
        })
    }

    /// `getSession`: resume from the keystore. Cache-first so a cold offline
    /// start still lands in the app shell; falls back to one online refresh.
    pub async fn session_status(&self) -> Result<Option<SessionDto>, CoreError> {
        if !self.session.has_stored_session() {
            return Ok(None);
        }
        if let Some(user) = self.cache.current_user().await? {
            if let Some(id) = parse_id(&user.id) {
                *self.current_user.lock().expect("user lock") = Some(v1::User {
                    id,
                    username: user.username.clone(),
                    display_name: user.display_name.clone(),
                    flags: 0,
                });
            }
            self.ensure_gateway().await;
            return Ok(Some(SessionDto { user }));
        }
        // Stored token but empty cache (first run on this machine): prove the
        // session online.
        match self.session.refresh_user().await {
            Ok(user) => {
                self.cache.set_current_user(user.clone()).await?;
                *self.current_user.lock().expect("user lock") = Some(user.clone());
                self.ensure_gateway().await;
                Ok(Some(SessionDto {
                    user: UserDto::from(&user),
                }))
            }
            Err(error) => {
                tracing::warn!(%error, "stored session could not be refreshed");
                Ok(None)
            }
        }
    }

    pub async fn logout(&self) -> Result<(), CoreError> {
        // Gateway first: a live driver must not race a re-Identify against
        // the credential teardown below.
        self.shutdown_gateway().await;
        if let Some(refresh) = self.session.refresh_token().await {
            // Best-effort revocation; local teardown happens regardless.
            if let Err(error) = self.api.logout(&refresh).await {
                tracing::warn!(%error, "server-side logout failed (continuing locally)");
            }
        }
        self.session.clear().await;
        self.cache.wipe().await?;
        self.pending.lock().expect("pending lock").clear();
        self.presence.lock().expect("presence lock").clear();
        *self.current_user.lock().expect("user lock") = None;
        emit_dice(
            &self.emitter,
            &DiceEvent::ConnState {
                state: "idle".to_owned(),
                transport: None,
            },
        );
        Ok(())
    }

    // ----------------------------------------------------------- bootstrap

    /// Instant first paint from the cache — EXCEPT right after a fresh
    /// login/first run, when the cache is empty or holds only the user row
    /// (`adopt_auth` wrote it): then wait, bounded, for the bridge to apply
    /// the first `Ready` so an existing account's guilds aren't rendered as
    /// an empty shell that never refreshes. Offline starts (driver in
    /// Backoff/Failed, or no driver) never wait: cache wins immediately.
    pub async fn get_bootstrap(&self) -> Result<BootstrapDto, CoreError> {
        let mut ready = self.ready_rx.clone();
        let mut applied = *ready.borrow_and_update();
        let deadline = Instant::now() + BOOTSTRAP_READY_TIMEOUT;
        loop {
            let snapshot = self.cache.bootstrap_snapshot().await?;
            let must_wait = applied == 0
                && self.gateway_pending_ready()
                && snapshot
                    .as_ref()
                    .is_none_or(|s| s.guilds.is_empty() && s.dms.is_empty());
            if !must_wait {
                return match snapshot {
                    Some(snapshot) => Ok(snapshot.into_dto(self.presence_strings())),
                    None => Err(CoreError::NotReady),
                };
            }
            let now = Instant::now();
            if now >= deadline {
                // Late: serve whatever the cache has rather than erroring.
                return match snapshot {
                    Some(snapshot) => Ok(snapshot.into_dto(self.presence_strings())),
                    None => Err(CoreError::NotReady),
                };
            }
            // Wake on the next applied Ready, or re-poll the driver state
            // every 250 ms so Backoff/Failed cuts the wait short.
            let tick = Duration::from_millis(250).min(deadline - now);
            match tokio::time::timeout(tick, ready.changed()).await {
                Ok(Ok(())) => applied = *ready.borrow_and_update(),
                // Bridge gone: stop waiting, serve the cache on the next turn.
                Ok(Err(_)) => applied = u64::MAX,
                Err(_) => {} // tick elapsed; re-check the driver state
            }
        }
    }

    /// True while the driver is still working toward (or finishing) a
    /// `Ready` this connection cycle: Idle/Connecting/Authenticating, or
    /// Ready with the cache application still in flight. Backoff/Failed or
    /// no driver at all ⇒ false (offline; don't hold the UI).
    fn gateway_pending_ready(&self) -> bool {
        use dice_network_core::client::ConnState;
        let slot = self.gateway.lock().expect("gateway lock");
        match slot.as_ref() {
            Some(ctl) if ctl.task.is_finished() => false, // bridge gone
            Some(ctl) => !matches!(
                &*ctl.state.borrow(),
                ConnState::Backoff { .. } | ConnState::Failed { .. }
            ),
            None => false,
        }
    }

    fn presence_strings(&self) -> std::collections::BTreeMap<String, String> {
        self.presence
            .lock()
            .expect("presence lock")
            .iter()
            .map(|(&id, &status)| (id_str(id), presence_str(status).to_owned()))
            .collect()
    }

    // ------------------------------------------------------------ messages

    /// Optimistic send: pending cache row first (negative id, the caller's
    /// nonce), then the wire command. The ack reconciles through the bridge
    /// and re-emits `messageCreate` with the same nonce.
    pub async fn send_message(
        &self,
        channel_id: &str,
        content: &str,
        reply_to: Option<&str>,
        nonce: &str,
    ) -> Result<MessageDto, CoreError> {
        let channel = parse_id(channel_id).ok_or_else(|| CoreError::BadId(channel_id.into()))?;
        let reply_to = match reply_to {
            Some(s) => Some(parse_id(s).ok_or_else(|| CoreError::BadId(s.into()))?),
            None => None,
        };
        let author = self
            .current_user
            .lock()
            .expect("user lock")
            .as_ref()
            .map(|u| u.id)
            .ok_or(CoreError::NoSession)?;
        let wire_nonce = self.nonce_seq.fetch_add(1, Ordering::Relaxed);
        let pending_dto = self
            .cache
            .insert_pending(
                channel,
                author,
                content.to_owned(),
                reply_to,
                nonce.to_owned(),
            )
            .await?;
        // Register the nonce mapping BEFORE the command goes out so even an
        // instant ack finds it.
        self.pending.lock().expect("pending lock").insert(
            wire_nonce,
            PendingSend {
                client_nonce: nonce.to_owned(),
                channel_id: channel,
                waiter: None,
                dispatched: false,
            },
        );
        let Some(cmds) = self.gateway_cmds() else {
            return self.fail_send(wire_nonce, nonce).await;
        };
        if cmds
            .send(Command::SendMessage {
                channel_id: channel,
                content: content.to_owned(),
                reply_to_id: reply_to.unwrap_or(0),
                nonce: wire_nonce,
            })
            .await
            .is_err()
        {
            return self.fail_send(wire_nonce, nonce).await;
        }
        Ok(pending_dto)
    }

    /// Toggle a reaction. `add=true` reacts, `add=false` un-reacts. Confirmed by
    /// the broadcast `ReactionUpdate` delta (which the reactor also receives).
    pub async fn react(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
        add: bool,
    ) -> Result<(), CoreError> {
        let channel = parse_id(channel_id).ok_or_else(|| CoreError::BadId(channel_id.into()))?;
        let message = parse_id(message_id).ok_or_else(|| CoreError::BadId(message_id.into()))?;
        let cmds = self.gateway_cmds().ok_or(CoreError::NotConnected)?;
        let nonce = self.nonce_seq.fetch_add(1, Ordering::Relaxed);
        let cmd = if add {
            Command::AddReaction {
                channel_id: channel,
                message_id: message,
                emoji: emoji.to_owned(),
                nonce,
            }
        } else {
            Command::RemoveReaction {
                channel_id: channel,
                message_id: message,
                emoji: emoji.to_owned(),
                nonce,
            }
        };
        cmds.send(cmd).await.map_err(|_| CoreError::NotConnected)
    }

    async fn fail_send(
        &self,
        wire_nonce: u64,
        client_nonce: &str,
    ) -> Result<MessageDto, CoreError> {
        self.pending
            .lock()
            .expect("pending lock")
            .remove(&wire_nonce);
        if let Err(error) = self.cache.mark_failed(client_nonce.to_owned()).await {
            tracing::warn!(%error, "mark_failed after offline send failed");
        }
        Err(CoreError::NotConnected)
    }

    /// Edit a message (server enforces author-only). Non-optimistic: the
    /// broadcast `MessageUpdate` dispatch updates cache + UI; a rejection comes
    /// back as a logged `RequestError`.
    pub async fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<(), CoreError> {
        let channel = parse_id(channel_id).ok_or_else(|| CoreError::BadId(channel_id.into()))?;
        let message = parse_id(message_id).ok_or_else(|| CoreError::BadId(message_id.into()))?;
        let cmds = self.gateway_cmds().ok_or(CoreError::NotConnected)?;
        cmds.send(Command::EditMessage {
            channel_id: channel,
            message_id: message,
            content: content.to_owned(),
            nonce: self.nonce_seq.fetch_add(1, Ordering::Relaxed),
        })
        .await
        .map_err(|_| CoreError::NotConnected)
    }

    /// Delete a message (server enforces author-or-MANAGE_MESSAGES). Confirmed
    /// by the broadcast `MessageDelete` dispatch.
    pub async fn delete_message(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> Result<(), CoreError> {
        let channel = parse_id(channel_id).ok_or_else(|| CoreError::BadId(channel_id.into()))?;
        let message = parse_id(message_id).ok_or_else(|| CoreError::BadId(message_id.into()))?;
        let cmds = self.gateway_cmds().ok_or(CoreError::NotConnected)?;
        cmds.send(Command::DeleteMessage {
            channel_id: channel,
            message_id: message,
            nonce: self.nonce_seq.fetch_add(1, Ordering::Relaxed),
        })
        .await
        .map_err(|_| CoreError::NotConnected)
    }

    /// Cache-first history. `before = None` serves the newest page (fetching
    /// from the API only when the window is stale/missing); `before = Some`
    /// pages older rows, extending the window downward from the API when the
    /// cache runs short. API failures degrade to whatever the cache has.
    pub async fn fetch_messages(
        &self,
        channel_id: &str,
        before: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<MessageDto>, CoreError> {
        let channel = parse_id(channel_id).ok_or_else(|| CoreError::BadId(channel_id.into()))?;
        let limit = limit.unwrap_or(50).clamp(1, 100);
        let before = match before {
            None => None,
            Some(s) => Some(parse_id(s).ok_or_else(|| CoreError::BadId(s.into()))?),
        };
        match before {
            None => {
                let fresh = self
                    .cache
                    .channel_sync(channel)
                    .await?
                    .is_some_and(|s| !s.stale && s.newest_synced_id.is_some());
                if !fresh {
                    match self
                        .api
                        .fetch_messages(channel, None, None, limit as u8)
                        .await
                    {
                        Ok(page) => self.cache.note_newest_page(channel, page).await?,
                        Err(error) => {
                            tracing::warn!(%error, channel, "history fetch failed; serving cache");
                        }
                    }
                }
                Ok(self.cache.page_messages(channel, None, limit).await?)
            }
            Some(before) => {
                let cached = self
                    .cache
                    .page_messages(channel, Some(before), limit)
                    .await?;
                if cached.len() as u32 >= limit {
                    return Ok(cached);
                }
                // The cache ran short: extend the window downward.
                let api_before = cached
                    .first()
                    .and_then(|m| parse_id(&m.id))
                    .unwrap_or(before);
                match self
                    .api
                    .fetch_messages(channel, Some(api_before), None, limit as u8)
                    .await
                {
                    Ok(older) if !older.is_empty() => {
                        self.cache.note_older_page(channel, older).await?;
                        Ok(self
                            .cache
                            .page_messages(channel, Some(before), limit)
                            .await?)
                    }
                    Ok(_) => Ok(cached),
                    Err(error) => {
                        tracing::warn!(%error, channel, "older-page fetch failed; serving cache");
                        Ok(cached)
                    }
                }
            }
        }
    }

    // ----------------------------------------------------------- ephemeral

    /// Throttled host-side to 1/8 s/channel; lossy when not connected.
    pub async fn start_typing(&self, channel_id: &str) -> Result<(), CoreError> {
        let channel = parse_id(channel_id).ok_or_else(|| CoreError::BadId(channel_id.into()))?;
        if !self.typing.allow(channel, Instant::now()) {
            return Ok(());
        }
        if let Some(cmds) = self.gateway_cmds() {
            let _ = cmds
                .send(Command::StartTyping {
                    channel_id: channel,
                })
                .await;
        }
        Ok(())
    }

    pub async fn set_presence(&self, status: &str) -> Result<(), CoreError> {
        let code = parse_presence(status);
        let user_id = self
            .current_user
            .lock()
            .expect("user lock")
            .as_ref()
            .map(|u| u.id)
            .ok_or(CoreError::NoSession)?;
        self.presence
            .lock()
            .expect("presence lock")
            .insert(user_id, code);
        if let Some(cmds) = self.gateway_cmds() {
            let _ = cmds.send(Command::UpdatePresence { status: code }).await;
        }
        // Local echo so the own orb flips even before the server fan-out.
        emit_dice(
            &self.emitter,
            &DiceEvent::PresenceUpdate {
                user_id: id_str(user_id),
                status: presence_str(code).to_owned(),
            },
        );
        Ok(())
    }

    // ---------------------------------------------------- guild / dm REST

    /// REST create; the cache applies immediately (idempotent with the
    /// sequenced `GuildCreate` dispatch that follows over the gateway).
    pub async fn create_guild(&self, name: &str) -> Result<GuildDto, CoreError> {
        let guild = self.api.create_guild(name).await?;
        self.cache
            .apply_event(Payload::GuildCreate(v1::GuildCreate {
                guild: Some(guild.clone()),
            }))
            .await?;
        Ok(GuildDto::from(&guild))
    }

    pub async fn join_guild(&self, code: &str) -> Result<GuildDto, CoreError> {
        let guild = self.api.join_guild(code).await?;
        self.cache
            .apply_event(Payload::GuildCreate(v1::GuildCreate {
                guild: Some(guild.clone()),
            }))
            .await?;
        Ok(GuildDto::from(&guild))
    }

    pub async fn open_dm(&self, recipient_id: &str) -> Result<ChannelDto, CoreError> {
        let recipient =
            parse_id(recipient_id).ok_or_else(|| CoreError::BadId(recipient_id.into()))?;
        let channel = self.api.open_dm(recipient).await?;
        self.cache
            .apply_event(Payload::DmChannelCreate(v1::DmChannelCreate {
                channel: Some(channel.clone()),
            }))
            .await?;
        Ok(ChannelDto::from(&channel))
    }
}

// ---------------------------------------------------------------- throttle

/// Per-channel typing throttle (injectable clock for tests).
#[derive(Default)]
struct TypingThrottle(StdMutex<HashMap<u64, Instant>>);

impl TypingThrottle {
    fn allow(&self, channel: u64, now: Instant) -> bool {
        let mut map = self.0.lock().expect("typing lock");
        match map.get(&channel) {
            Some(&last) if now.duration_since(last) < TYPING_THROTTLE => false,
            _ => {
                map.insert(channel, now);
                true
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn typing_throttle_is_one_per_eight_seconds_per_channel() {
        let throttle = TypingThrottle::default();
        let t0 = Instant::now();
        assert!(throttle.allow(1, t0), "first call passes");
        assert!(!throttle.allow(1, t0 + Duration::from_secs(1)), "1 s: held");
        assert!(
            !throttle.allow(1, t0 + Duration::from_millis(7_999)),
            "7.999 s: held"
        );
        assert!(throttle.allow(2, t0), "other channels are independent");
        assert!(
            throttle.allow(1, t0 + Duration::from_secs(8)),
            "8 s: passes again"
        );
        assert!(
            !throttle.allow(1, t0 + Duration::from_secs(9)),
            "window restarted at 8 s"
        );
    }

    #[test]
    fn core_config_from_env_defaults() {
        // Default URLs parse and point at localhost:8443.
        let cfg = CoreConfig::from_env(PathBuf::from("x.db"));
        // DICE_API_URL may be set in the developer's env; only assert shape.
        let cfg = cfg.unwrap();
        assert!(cfg.api_url.scheme() == "https" || cfg.api_url.scheme() == "http");
        assert!(cfg.wss_url.scheme() == "wss" || cfg.wss_url.scheme() == "ws");
    }

    #[test]
    fn user_messages_are_clean() {
        assert_eq!(CoreError::NoSession.user_message(), "not logged in");
        let api_err = CoreError::Api(ApiError::Api {
            status: 401,
            error: v1::Error {
                code: 16,
                message: "invalid credentials".into(),
                retry_after_ms: 0,
            },
        });
        assert_eq!(api_err.user_message(), "invalid credentials");
    }
}
