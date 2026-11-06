//! In-memory state of Login flows and authorization codes (ADR-0003: never on disk).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

use crate::tg::{LoggedIn, LoginAttempt};

const LOGIN_TTL: Duration = Duration::from_secs(10 * 60);
const CODE_TTL: Duration = Duration::from_secs(60);
/// Bounds memory if someone opens /authorize in a loop.
const MAX_PENDING: usize = 10_000;

/// What the client asked for at /authorize, already validated.
#[derive(Debug, Clone)]
pub struct AuthRequest {
    pub client_id: String,
    pub client_name: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub state: Option<String>,
}

pub enum Progress {
    InProgress(Box<dyn LoginAttempt>),
    LoggedIn(LoggedIn),
}

pub struct PendingLogin {
    pub request: AuthRequest,
    pub progress: tokio::sync::Mutex<Progress>,
    created: Instant,
}

pub struct IssuedCode {
    pub request: AuthRequest,
    pub logged_in: LoggedIn,
    expires: Instant,
}

#[derive(Debug, thiserror::Error)]
#[error("too many logins in progress")]
pub struct Full;

#[derive(Default)]
pub struct Flows {
    logins: Mutex<HashMap<String, Arc<PendingLogin>>>,
    codes: Mutex<HashMap<String, IssuedCode>>,
}

impl Flows {
    pub fn start_login(
        &self,
        request: AuthRequest,
        attempt: Box<dyn LoginAttempt>,
    ) -> Result<String, Full> {
        let mut logins = self.logins.lock().expect("flows lock");
        if logins.len() >= MAX_PENDING {
            logins.retain(|_, l| l.created.elapsed() < LOGIN_TTL);
            if logins.len() >= MAX_PENDING {
                return Err(Full);
            }
        }
        let id = new_id();
        logins.insert(
            id.clone(),
            Arc::new(PendingLogin {
                request,
                progress: tokio::sync::Mutex::new(Progress::InProgress(attempt)),
                created: Instant::now(),
            }),
        );
        Ok(id)
    }

    pub fn login(&self, id: &str) -> Option<Arc<PendingLogin>> {
        let logins = self.logins.lock().expect("flows lock");
        logins
            .get(id)
            .filter(|l| l.created.elapsed() < LOGIN_TTL)
            .cloned()
    }

    /// Remove a login; it can be finished (or denied) exactly once.
    pub fn take_login(&self, id: &str) -> Option<Arc<PendingLogin>> {
        let mut logins = self.logins.lock().expect("flows lock");
        logins
            .remove(id)
            .filter(|l| l.created.elapsed() < LOGIN_TTL)
    }

    pub fn issue_code(&self, request: AuthRequest, logged_in: LoggedIn) -> String {
        let mut codes = self.codes.lock().expect("flows lock");
        codes.retain(|_, c| c.expires > Instant::now());
        let code = new_id();
        codes.insert(
            code.clone(),
            IssuedCode {
                request,
                logged_in,
                expires: Instant::now() + CODE_TTL,
            },
        );
        code
    }

    /// Drop expired logins and codes. An abandoned login holds an open Telegram
    /// connection until it is dropped, so this runs periodically.
    pub fn sweep(&self) -> usize {
        let mut logins = self.logins.lock().expect("flows lock");
        let before = logins.len();
        logins.retain(|_, l| l.created.elapsed() < LOGIN_TTL);
        let swept = before - logins.len();
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
