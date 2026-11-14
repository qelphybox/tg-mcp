//! In-memory state of Login flows and authorization codes (ADR-0003: never on disk).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

use super::{rate_limit::ClientIp, send_list::Pickers};
use crate::{
    allow_list::SendAllowList,
    scope::Scopes,
    tg::{LoggedIn, LoginAttempt},
};

const LOGIN_TTL: Duration = Duration::from_secs(10 * 60);
const CODE_TTL: Duration = Duration::from_secs(60);

/// What the client asked for at /authorize, already validated.
#[derive(Debug, Clone)]
pub struct AuthRequest {
    pub client_id: String,
    pub client_name: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub state: Option<String>,
    /// The `scope` the client asked for (`read` at least): pre-checked on Consent.
    pub scopes: Scopes,
}

/// What a Login flow is for.
#[derive(Debug)]
pub enum Purpose {
    /// An OAuth authorization: ends on Consent with an authorization code.
    Authorize(AuthRequest),
    /// `/forget` (#63): proves who owns the Account, then deletes its Grants.
    /// Never ends in a code.
    Forget,
}

// `InProgress` reads as the login's state; renaming it helps no reader.
#[allow(clippy::enum_variant_names)]
pub enum Progress {
    InProgress(Box<dyn LoginAttempt>),
    LoggedIn(LoggedIn),
    /// Its session was handed on ([`Progress::take_logged_in`]): `/forget`
    /// used it and logged it out.
    Finished,
}

impl Progress {
    /// The logged-in session, leaving [`Progress::Finished`]; `None` (and
    /// nothing changed) when the login isn't finished.
    pub fn take_logged_in(&mut self) -> Option<LoggedIn> {
        match std::mem::replace(self, Progress::Finished) {
            Progress::LoggedIn(logged_in) => Some(logged_in),
            other => {
                *self = other;
                None
            }
        }
    }
}

pub struct PendingLogin {
    pub purpose: Purpose,
    pub progress: tokio::sync::Mutex<Progress>,
    /// Only touched while `progress` is locked.
    pub password_gate: Mutex<PasswordGate>,
    /// The Consent screen's chat pickers, once loaded: they hold a Live client
    /// for the logged-in session, closed when the login goes. Locked after
    /// `progress`, never before.
    pub send_pickers: tokio::sync::Mutex<Option<Pickers>>,
    /// How many times loading the pickers was tried.
    pub picker_loads: std::sync::atomic::AtomicU32,
    client: ClientIp,
    created: Instant,
}

/// Limits on Cloud password attempts within one Login flow.
#[derive(Debug, Default)]
pub struct PasswordGate {
    pub wrong: u32,
    /// Telegram's `FLOOD_WAIT`: nothing goes to Telegram before this.
    pub retry_at: Option<Instant>,
}

pub struct IssuedCode {
    pub request: AuthRequest,
    pub logged_in: LoggedIn,
    /// What the user allowed on Consent: the new Grant's Scopes.
    pub scopes: Scopes,
    /// With `send`: the Chats chosen on Consent, to seal with the new Grant.
    pub send_allow_list: Option<SendAllowList>,
    expires: Instant,
}

/// No room for another Login flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Full {
    #[error("too many logins in progress")]
    Server,
    #[error("too many logins in progress from this client")]
    Client,
}

/// Pending Login flows, plus slots reserved for ones whose Telegram login is
/// still being started. Both count against the caps.
#[derive(Default)]
struct Logins {
    pending: HashMap<String, Arc<PendingLogin>>,
    starting: HashMap<ClientIp, usize>,
}

impl Logins {
    fn count(&self, client: ClientIp) -> (usize, usize) {
        let starting_total: usize = self.starting.values().sum();
        let pending_client = self.pending.values().filter(|l| l.client == client).count();
        let starting_client = self.starting.get(&client).copied().unwrap_or(0);
        (
            self.pending.len() + starting_total,
            pending_client + starting_client,
        )
    }

    fn release(&mut self, client: ClientIp) {
        if let Some(n) = self.starting.get_mut(&client) {
            *n -= 1;
            if *n == 0 {
                self.starting.remove(&client);
            }
        }
    }
}

/// A slot for one Login flow, taken before Telegram is asked to start a login.
/// Dropping it unused frees the slot.
pub struct Reservation<'a> {
    flows: &'a Flows,
    client: ClientIp,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.flows
            .logins
            .lock()
            .expect("flows lock")
            .release(self.client);
    }
}

pub struct Flows {
    max_pending: usize,
    max_pending_per_client: usize,
    logins: Mutex<Logins>,
    codes: Mutex<HashMap<String, IssuedCode>>,
}

impl Flows {
    pub fn new(max_pending: usize, max_pending_per_client: usize) -> Self {
        Self {
            max_pending,
            max_pending_per_client,
            logins: Mutex::default(),
            codes: Mutex::default(),
        }
    }

    /// Reserve a slot for a Login flow from `client`, within both caps.
    pub fn reserve(&self, client: ClientIp) -> Result<Reservation<'_>, Full> {
        let mut logins = self.logins.lock().expect("flows lock");
        let (mut total, mut from_client) = logins.count(client);
        if total >= self.max_pending || from_client >= self.max_pending_per_client {
            logins
                .pending
                .retain(|_, l| l.created.elapsed() < LOGIN_TTL);
            (total, from_client) = logins.count(client);
        }
        if total >= self.max_pending {
            return Err(Full::Server);
        }
        if from_client >= self.max_pending_per_client {
            return Err(Full::Client);
        }
        *logins.starting.entry(client).or_default() += 1;
        Ok(Reservation {
            flows: self,
            client,
        })
    }

    /// Turn a reserved slot into a pending Login flow.
    pub fn start_login(
        &self,
        reservation: Reservation<'_>,
        purpose: Purpose,
        attempt: Box<dyn LoginAttempt>,
    ) -> String {
        let client = reservation.client;
        let id = new_id();
        let mut logins = self.logins.lock().expect("flows lock");
        logins.pending.insert(
            id.clone(),
            Arc::new(PendingLogin {
                purpose,
                progress: tokio::sync::Mutex::new(Progress::InProgress(attempt)),
                password_gate: Mutex::default(),
                send_pickers: tokio::sync::Mutex::default(),
                picker_loads: Default::default(),
                client,
                created: Instant::now(),
            }),
        );
        // The slot now belongs to the pending login.
        logins.release(client);
        drop(logins);
        std::mem::forget(reservation);
        id
    }

    pub fn login(&self, id: &str) -> Option<Arc<PendingLogin>> {
        let logins = self.logins.lock().expect("flows lock");
        logins
            .pending
            .get(id)
            .filter(|l| l.created.elapsed() < LOGIN_TTL)
            .cloned()
    }

    /// Remove a login; it can be finished (or denied) exactly once.
    pub fn take_login(&self, id: &str) -> Option<Arc<PendingLogin>> {
        let mut logins = self.logins.lock().expect("flows lock");
        logins
            .pending
            .remove(id)
            .filter(|l| l.created.elapsed() < LOGIN_TTL)
    }

    pub fn issue_code(
        &self,
        request: AuthRequest,
        logged_in: LoggedIn,
        scopes: Scopes,
        send_allow_list: Option<SendAllowList>,
    ) -> String {
        let mut codes = self.codes.lock().expect("flows lock");
        codes.retain(|_, c| c.expires > Instant::now());
        let code = new_id();
        codes.insert(
            code.clone(),
            IssuedCode {
                request,
                logged_in,
                scopes,
                send_allow_list,
                expires: Instant::now() + CODE_TTL,
            },
        );
        code
    }

    /// Drop expired logins and codes, returning the logins dropped (for
    /// `/forget`'s to be logged out). An abandoned login holds an open
    /// Telegram connection until it is dropped, so this runs periodically.
    pub fn sweep(&self) -> Vec<Arc<PendingLogin>> {
        let mut logins = self.logins.lock().expect("flows lock");
        let expired: Vec<String> = logins
            .pending
            .iter()
            .filter(|(_, l)| l.created.elapsed() >= LOGIN_TTL)
            .map(|(id, _)| id.clone())
            .collect();
        let swept = expired
            .iter()
            .filter_map(|id| logins.pending.remove(id))
            .collect();
        drop(logins);
        self.codes
            .lock()
            .expect("flows lock")
            .retain(|_, c| c.expires > Instant::now());
        swept
    }

    /// Single use: the code is gone after this, whether or not the exchange succeeds.
    pub fn take_code(&self, code: &str) -> Option<IssuedCode> {
        let mut codes = self.codes.lock().expect("flows lock");
        codes.remove(code).filter(|c| c.expires > Instant::now())
    }
}

fn new_id() -> String {
    URL_SAFE_NO_PAD.encode(crate::sessions::random_bytes::<32>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tg::{FakeLogin, Login};

    fn client(last: u8) -> ClientIp {
        ClientIp::new([203, 0, 113, last].into())
    }

    fn request() -> AuthRequest {
        AuthRequest {
            client_id: "tgmc_test".into(),
            client_name: "Test".into(),
            redirect_uri: "http://127.0.0.1/cb".into(),
            code_challenge: "x".into(),
            state: None,
            scopes: Scopes::READ_ONLY,
        }
    }

    async fn start(flows: &Flows, from: ClientIp) -> Result<String, Full> {
        let slot = flows.reserve(from)?;
        let attempt = FakeLogin::default().start().await.unwrap();
        Ok(flows.start_login(slot, Purpose::Authorize(request()), attempt))
    }

    #[tokio::test]
    async fn the_server_wide_cap_counts_every_client() {
        let flows = Flows::new(2, 10);
        start(&flows, client(1)).await.unwrap();
        start(&flows, client(2)).await.unwrap();
        assert_eq!(start(&flows, client(3)).await, Err(Full::Server));
    }

    #[tokio::test]
    async fn the_per_client_cap_leaves_room_for_others() {
        let flows = Flows::new(10, 2);
        start(&flows, client(1)).await.unwrap();
        start(&flows, client(1)).await.unwrap();
        assert_eq!(start(&flows, client(1)).await, Err(Full::Client));
        start(&flows, client(2)).await.unwrap();
    }

    #[tokio::test]
    async fn slots_being_started_count_and_an_unused_one_is_freed() {
        let flows = Flows::new(10, 1);
        let slot = flows.reserve(client(1)).unwrap();
        assert!(matches!(flows.reserve(client(1)), Err(Full::Client)));
        drop(slot); // e.g. Telegram failed to start the login
        start(&flows, client(1)).await.unwrap();
        assert!(matches!(flows.reserve(client(1)), Err(Full::Client)));
    }

    #[tokio::test]
    async fn a_finished_login_frees_its_slot() {
        let flows = Flows::new(1, 1);
        let id = start(&flows, client(1)).await.unwrap();
        assert!(flows.reserve(client(2)).is_err());
        flows.take_login(&id).unwrap();
        start(&flows, client(2)).await.unwrap();
    }
}
