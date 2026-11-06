//! Anonymous sign-in against Supabase Auth, and keeping its session fresh.
//!
//! [`SupabaseAuthPlugin`] adds the [`SupabaseAuth`] resource. Trigger
//! [`SignInAnonymously`] to create a new anonymous user, or
//! [`RefreshAuthSession`] with a stored refresh token to resume one; the
//! outcome arrives as [`AuthSessionStarted`] or [`AuthFailed`]. While a session
//! is live, every [`SupabasePlugin`](crate::SupabasePlugin) write and
//! [`SupabaseViewPlugin`](crate::SupabaseViewPlugin) read carries its access
//! token, so row-level security sees that user. The plugin refreshes the
//! session shortly before its access token expires.
//!
//! Supabase rotates refresh tokens: each [`AuthSessionStarted`] carries a new
//! one and the previous one stops working, so persist it as soon as it arrives.
//!
//! # Example
//!
//! ```rust
//! use bevy::prelude::*;
//! use msg_supabase::prelude::*;
//!
//! let mut app = App::new();
//! app.add_plugins(MinimalPlugins);
//! app.add_plugins(SupabaseAuthPlugin::new(
//!     "https://your-project.supabase.co",
//!     "your-publishable-key",
//! ));
//! app.add_observer(|started: On<AuthSessionStarted>| {
//!     // Store `started.refresh_token` to resume this user next launch.
//!     let _ = &started.refresh_token;
//! });
//!
//! // Requests carry the publishable key until a session starts.
//! assert!(!app.world().resource::<SupabaseAuth>().is_signed_in());
//! ```

use std::time::Duration;

use bevy::log::{info, warn};
use bevy::platform::time::Instant;
use bevy::prelude::*;
use serde::Deserialize;

use crate::error::RequestError;
use crate::queue::ResultQueue;
use crate::request::{SupabaseConnection, send};

/// How long before its access token expires a session is refreshed.
const REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// How long a failed automatic refresh waits before it is tried again.
const RETRY_DELAY: Duration = Duration::from_secs(30);

/// Plugin that signs in to Supabase Auth and keeps the session fresh.
pub struct SupabaseAuthPlugin {
    connection: SupabaseConnection,
}

impl SupabaseAuthPlugin {
    /// Sign in to the project at `url` with its publishable `api_key`.
    pub fn new(url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            connection: SupabaseConnection::new(url, api_key),
        }
    }
}

impl Plugin for SupabaseAuthPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(SupabaseAuth::new(self.connection.clone()));
        app.add_observer(on_sign_in_anonymously);
        app.add_observer(on_refresh_auth_session);
        app.add_systems(Update, (poll_auth_results, refresh_before_expiry).chain());
    }
}

/// Which Auth request an outcome answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRequest {
    /// [`SignInAnonymously`]: a new anonymous user.
    SignIn,
    /// [`RefreshAuthSession`] or an automatic refresh: an existing user.
    Refresh,
}

/// Create a new anonymous user and start a session for it.
///
/// Ignored while another Auth request is in flight.
#[derive(Event, Debug, Clone, Copy, Default)]
pub struct SignInAnonymously;

/// Resume the user `refresh_token` belongs to.
///
/// Ignored while another Auth request is in flight.
#[derive(Event, Debug, Clone)]
pub struct RefreshAuthSession {
    /// A refresh token from an earlier [`AuthSessionStarted`].
    pub refresh_token: String,
}

/// A session started or was refreshed.
#[derive(Event, Debug, Clone)]
pub struct AuthSessionStarted {
    /// The request this answers.
    pub request: AuthRequest,
    /// The signed-in user's id (`auth.uid()` in row-level security policies).
    pub user_id: String,
    /// The refresh token to resume this user with. It replaces every earlier
    /// one.
    pub refresh_token: String,
}

/// An Auth request failed.
#[derive(Event, Debug, Clone)]
pub struct AuthFailed {
    /// The request this answers.
    pub request: AuthRequest,
    /// What went wrong.
    pub error: RequestError,
}

impl AuthFailed {
    /// Whether the server refused the request itself — an invalid or revoked
    /// refresh token, or anonymous sign-ins disabled — rather than being
    /// unreachable, overloaded or rate limiting. A refused request fails the
    /// same way when repeated.
    pub fn refused(&self) -> bool {
        self.error
            .status
            .is_some_and(|status| (400..500).contains(&status) && status != 408 && status != 429)
    }
}

/// A live Auth session.
#[derive(Debug, Clone)]
pub struct AuthSession {
    /// The signed-in user's id.
    pub user_id: String,
    /// The bearer token requests carry.
    pub access_token: String,
    /// The token that renews this session.
    pub refresh_token: String,
    /// When `access_token` stops being accepted.
    pub expires_at: Instant,
}

impl AuthSession {
    /// Whether the access token is still accepted at `now`.
    pub fn is_valid_at(&self, now: Instant) -> bool {
        now < self.expires_at
    }

    /// Whether the session is due for a refresh at `now`.
    fn is_due_at(&self, now: Instant) -> bool {
        now + REFRESH_MARGIN >= self.expires_at
    }
}

/// The Auth session requests are made as, and the Auth request in flight.
#[derive(Resource)]
pub struct SupabaseAuth {
    connection: SupabaseConnection,
    session: Option<AuthSession>,
    pending: Option<AuthRequest>,
    retry_at: Option<Instant>,
    queue: ResultQueue<AuthOutcome>,
}

/// What an Auth request returned, handed from its HTTP callback to the World.
struct AuthOutcome {
    request: AuthRequest,
    result: Result<TokenResponse, RequestError>,
    sent_at: Instant,
}

/// The body Supabase Auth answers a sign-in or refresh with.
#[derive(Deserialize, Debug)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
    user: TokenUser,
}

#[derive(Deserialize, Debug)]
struct TokenUser {
    id: String,
}

impl SupabaseAuth {
    /// Signed out, talking to the project `connection` names.
    pub fn new(connection: SupabaseConnection) -> Self {
        Self {
            connection,
            session: None,
            pending: None,
            retry_at: None,
            queue: ResultQueue::new(),
        }
    }

    /// The live session, if any — its access token may have expired.
    pub fn session(&self) -> Option<&AuthSession> {
        self.session.as_ref()
    }

    /// The access token to send, while it is still accepted.
    pub fn access_token(&self) -> Option<&str> {
        self.session
            .as_ref()
            .filter(|session| session.is_valid_at(Instant::now()))
            .map(|session| session.access_token.as_str())
    }

    /// Whether requests are made as a signed-in user right now.
    pub fn is_signed_in(&self) -> bool {
        self.access_token().is_some()
    }

    /// Whether an Auth request is in flight.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Make requests as `session`, one obtained outside this plugin (a test,
    /// or a sign-in flow of the caller's own).
    pub fn insert_session(&mut self, session: AuthSession) {
        self.session = Some(session);
        self.retry_at = None;
    }

    /// Forget the session. Requests are made as the anonymous role again.
    pub fn sign_out(&mut self) {
        self.session = None;
        self.retry_at = None;
    }

    /// `connection`, carrying the access token of `auth`'s live session if
    /// there is one.
    pub fn authenticate(
        auth: Option<&Self>,
        connection: &SupabaseConnection,
    ) -> SupabaseConnection {
        connection
            .clone()
            .with_access_token(auth.and_then(Self::access_token).map(str::to_owned))
    }

    /// Send `request`, unless another one is in flight.
    fn start(&mut self, request: AuthRequest, http: ehttp::Request) {
        if self.pending.is_some() {
            return;
        }
        self.pending = Some(request);
        let sender = self.queue.sender();
        let sent_at = Instant::now();
        send(http, move |result| {
            let result = result.and_then(|response| {
                let body = response.text().unwrap_or_default();
                serde_json::from_str::<TokenResponse>(body).map_err(|error| {
                    RequestError::http(
                        format!("Failed to parse Auth response: {error}"),
                        response.status,
                        Some(body.to_string()),
                    )
                })
            });
            sender.send(AuthOutcome {
                request,
                result,
                sent_at,
            });
        });
    }

    /// Apply an outcome, returning the event that reports it.
    fn apply(&mut self, outcome: AuthOutcome) -> AuthEvent {
        self.pending = None;
        match outcome.result {
            Ok(token) => {
                self.retry_at = None;
                self.session = Some(AuthSession {
                    user_id: token.user.id.clone(),
                    access_token: token.access_token,
                    refresh_token: token.refresh_token.clone(),
                    expires_at: outcome.sent_at + Duration::from_secs(token.expires_in),
                });
                AuthEvent::Started(AuthSessionStarted {
                    request: outcome.request,
                    user_id: token.user.id,
                    refresh_token: token.refresh_token,
                })
            }
            Err(error) => {
                let failed = AuthFailed {
                    request: outcome.request,
                    error,
                };
                if failed.refused() && failed.request == AuthRequest::Refresh {
                    self.session = None;
                }
                self.retry_at = Some(Instant::now() + RETRY_DELAY);
                AuthEvent::Failed(failed)
            }
        }
    }
}

enum AuthEvent {
    Started(AuthSessionStarted),
    Failed(AuthFailed),
}

/// The request that creates an anonymous user.
pub fn build_sign_in_request(connection: &SupabaseConnection) -> ehttp::Request {
    auth_request(connection, connection.auth_url("signup"), "{}".to_string())
}

/// The request that trades `refresh_token` for a fresh session.
pub fn build_refresh_request(
    connection: &SupabaseConnection,
    refresh_token: &str,
) -> ehttp::Request {
    let body = serde_json::json!({ "refresh_token": refresh_token }).to_string();
    auth_request(
        connection,
        connection.auth_url("token?grant_type=refresh_token"),
        body,
    )
}

fn auth_request(connection: &SupabaseConnection, url: String, body: String) -> ehttp::Request {
    ehttp::Request {
        body: body.into_bytes(),
        ..ehttp::Request::new(
            ehttp::Method::POST,
            url,
            ehttp::Headers::new(&[
                ("Content-Type", "application/json"),
                ("apikey", &connection.api_key),
                ("Authorization", &format!("Bearer {}", connection.api_key)),
            ]),
        )
    }
}

fn on_sign_in_anonymously(_trigger: On<SignInAnonymously>, mut auth: ResMut<SupabaseAuth>) {
    let request = build_sign_in_request(&auth.connection);
    auth.start(AuthRequest::SignIn, request);
}

fn on_refresh_auth_session(trigger: On<RefreshAuthSession>, mut auth: ResMut<SupabaseAuth>) {
    let request = build_refresh_request(&auth.connection, &trigger.refresh_token);
    auth.start(AuthRequest::Refresh, request);
}

/// Applies finished Auth requests and reports each.
fn poll_auth_results(mut auth: ResMut<SupabaseAuth>, mut commands: Commands) {
    for outcome in auth.queue.drain() {
        match auth.apply(outcome) {
            AuthEvent::Started(started) => {
                info!("Supabase Auth session started ({:?})", started.request);
                commands.trigger(started);
            }
            AuthEvent::Failed(failed) => {
                warn!(
                    "Supabase Auth {:?} failed: {} (status: {:?})",
                    failed.request, failed.error.message, failed.error.status
                );
                commands.trigger(failed);
            }
        }
    }
}

/// Refreshes the live session shortly before its access token expires.
fn refresh_before_expiry(mut auth: ResMut<SupabaseAuth>) {
    let now = Instant::now();
    if auth.is_pending() || auth.retry_at.is_some_and(|retry_at| now < retry_at) {
        return;
    }
    let Some(session) = auth
        .session
        .as_ref()
        .filter(|session| session.is_due_at(now))
    else {
        return;
    };
    let request = build_refresh_request(&auth.connection, &session.refresh_token);
    auth.start(AuthRequest::Refresh, request);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> SupabaseConnection {
        SupabaseConnection::new("https://test.supabase.co", "key123")
    }

    fn header(request: &ehttp::Request, name: &str) -> Option<String> {
        request.headers.get(name).map(String::from)
    }

    fn token(user: &str, refresh: &str, expires_in: u64) -> TokenResponse {
        TokenResponse {
            access_token: format!("access-{user}"),
            refresh_token: refresh.to_string(),
            expires_in,
            user: TokenUser {
                id: user.to_string(),
            },
        }
    }

    /// An app with the auth resource and its result system, but no observers
    /// that would send real requests.
    fn auth_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(SupabaseAuth::new(connection()));
        app.add_systems(Update, poll_auth_results);
        app.init_resource::<Reports>();
        app.add_observer(
            |started: On<AuthSessionStarted>, mut reports: ResMut<Reports>| {
                reports.started.push(started.event().clone());
            },
        );
        app.add_observer(|failed: On<AuthFailed>, mut reports: ResMut<Reports>| {
            reports.failed.push(failed.event().clone());
        });
        app
    }

    #[derive(Resource, Default)]
    struct Reports {
        started: Vec<AuthSessionStarted>,
        failed: Vec<AuthFailed>,
    }

    fn deliver(app: &mut App, request: AuthRequest, result: Result<TokenResponse, RequestError>) {
        let mut auth = app.world_mut().resource_mut::<SupabaseAuth>();
        auth.pending = Some(request);
        auth.queue.send(AuthOutcome {
            request,
            result,
            sent_at: Instant::now(),
        });
        app.update();
    }

    #[test]
    fn sign_in_posts_to_signup_with_the_api_key() {
        let request = build_sign_in_request(&connection());
        assert_eq!(request.method, ehttp::Method::POST);
        assert_eq!(request.url, "https://test.supabase.co/auth/v1/signup");
        assert_eq!(header(&request, "apikey").as_deref(), Some("key123"));
        assert_eq!(String::from_utf8(request.body).unwrap(), "{}");
    }

    #[test]
    fn refresh_posts_the_refresh_token() {
        let request = build_refresh_request(&connection(), "r1");
        assert_eq!(
            request.url,
            "https://test.supabase.co/auth/v1/token?grant_type=refresh_token"
        );
        assert_eq!(
            String::from_utf8(request.body).unwrap(),
            r#"{"refresh_token":"r1"}"#
        );
    }

    #[test]
    fn token_response_parses_the_auth_body() {
        let body = r#"{"access_token":"a","token_type":"bearer","expires_in":3600,
            "expires_at":1,"refresh_token":"r","user":{"id":"u","is_anonymous":true}}"#;
        let token: TokenResponse = serde_json::from_str(body).unwrap();
        assert_eq!(token.user.id, "u");
        assert_eq!(token.refresh_token, "r");
        assert_eq!(token.expires_in, 3600);
    }

    #[test]
    fn a_started_session_authenticates_requests() {
        let mut app = auth_app();
        deliver(&mut app, AuthRequest::SignIn, Ok(token("u1", "r1", 3600)));

        let auth = app.world().resource::<SupabaseAuth>();
        assert!(!auth.is_pending());
        assert!(auth.is_signed_in());
        let connection = SupabaseAuth::authenticate(Some(auth), &connection());
        assert_eq!(connection.bearer(), "access-u1");

        let reports = app.world().resource::<Reports>();
        assert_eq!(reports.started.len(), 1);
        assert_eq!(reports.started[0].user_id, "u1");
        assert_eq!(reports.started[0].refresh_token, "r1");
    }

    #[test]
    fn without_a_session_requests_carry_the_api_key() {
        let auth = SupabaseAuth::new(connection());
        assert_eq!(
            SupabaseAuth::authenticate(Some(&auth), &connection()).bearer(),
            "key123"
        );
        assert_eq!(
            SupabaseAuth::authenticate(None, &connection()).bearer(),
            "key123"
        );
    }

    #[test]
    fn an_expired_token_is_not_sent() {
        let mut app = auth_app();
        deliver(&mut app, AuthRequest::SignIn, Ok(token("u1", "r1", 0)));

        let auth = app.world().resource::<SupabaseAuth>();
        assert!(auth.session().is_some());
        assert!(auth.access_token().is_none());
    }

    #[test]
    fn a_session_is_due_within_the_margin_of_expiry() {
        let now = Instant::now();
        let session = |expires_in: Duration| AuthSession {
            user_id: "u".into(),
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: now + expires_in,
        };
        assert!(session(REFRESH_MARGIN / 2).is_due_at(now));
        assert!(!session(REFRESH_MARGIN * 2).is_due_at(now));
    }

    #[test]
    fn a_refused_refresh_drops_the_session() {
        let mut app = auth_app();
        deliver(&mut app, AuthRequest::SignIn, Ok(token("u1", "r1", 3600)));
        deliver(
            &mut app,
            AuthRequest::Refresh,
            Err(RequestError::http("invalid refresh token", 400, None)),
        );

        assert!(app.world().resource::<SupabaseAuth>().session().is_none());
        let reports = app.world().resource::<Reports>();
        assert_eq!(reports.failed.len(), 1);
        assert!(reports.failed[0].refused());
    }

    #[test]
    fn an_unreachable_server_keeps_the_session() {
        let mut app = auth_app();
        deliver(&mut app, AuthRequest::SignIn, Ok(token("u1", "r1", 3600)));
        deliver(
            &mut app,
            AuthRequest::Refresh,
            Err(RequestError::network("offline")),
        );

        assert!(app.world().resource::<SupabaseAuth>().is_signed_in());
        let failed = &app.world().resource::<Reports>().failed[0];
        assert!(!failed.refused());
    }

    #[test]
    fn rate_limiting_is_not_a_refusal() {
        let failed = |status| AuthFailed {
            request: AuthRequest::SignIn,
            error: RequestError::http("", status, None),
        };
        assert!(failed(422).refused());
        assert!(!failed(429).refused());
        assert!(!failed(500).refused());
    }

    #[test]
    fn an_inserted_session_authenticates_requests() {
        let mut auth = SupabaseAuth::new(connection());
        auth.insert_session(AuthSession {
            user_id: "u".into(),
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: Instant::now() + Duration::from_secs(3600),
        });
        assert_eq!(auth.access_token(), Some("a"));
    }

    #[test]
    fn a_second_request_waits_for_the_first() {
        let mut auth = SupabaseAuth::new(connection());
        auth.pending = Some(AuthRequest::SignIn);
        auth.start(
            AuthRequest::Refresh,
            build_refresh_request(&connection(), "r"),
        );
        assert_eq!(auth.pending, Some(AuthRequest::SignIn));
    }
}
