//! OpenAI's public-client Sign in with ChatGPT flow.

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use rand::{RngCore, rngs::OsRng};
use reqwest::blocking::{Client, Response};
use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use url::Url;

const ISSUER: &str = "https://auth.openai.com";
const DISCOVERY: &str = "https://auth.openai.com/.well-known/openid-configuration";
const RESOURCE: &str = "https://api.openai.com/v1";
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const KEYCHAIN_SERVICE: &str = "com.cesarferreira.quick-ask.chatgpt";
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const KEYCHAIN_ITEM_NOT_FOUND: i32 = -25300;

#[derive(Clone, Debug)]
pub struct AccountSummary {
    pub email: String,
    pub subject: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct Registration {
    client_id: String,
    subject: String,
    email: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct Installation {
    ext_agent_host_id: String,
    registration: Option<Registration>,
    #[serde(default)]
    pending_client_id: Option<String>,
    #[serde(default)]
    signed_in: bool,
}

// Credentials deliberately do not implement Debug. The complete rotating token set is
// replaced in one Keychain item; no token is written to the metadata file.
#[derive(Clone, Deserialize, Serialize)]
struct Credentials {
    issuer: String,
    client_id: String,
    ext_agent_host_id: String,
    subject: String,
    email: String,
    access_token: String,
    refresh_token: String,
    id_token: String,
    scopes: Vec<String>,
    expires_at: u64,
    auth_nonce: String,
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    revocation_endpoint: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    token_type: String,
    expires_in: u64,
    #[serde(default)]
    scope: Option<String>,
}

struct TokenFailure {
    terminal: bool,
    message: &'static str,
}

/// Use one manager per running app, behind the UI's mutex. Mutable access serializes
/// rotating refresh-token exchanges; the app must also prevent duplicate processes.
pub struct AuthManager {
    data_dir: PathBuf,
    installation: Installation,
    credentials: Option<Credentials>,
    http: Client,
}

impl AuthManager {
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        let installation = load_installation(&data_dir)?;
        let credentials = if installation.signed_in {
            if let Some(registration) = &installation.registration {
                load_credentials(&installation.ext_agent_host_id, registration)?
            } else {
                None
            }
        } else {
            None
        };
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("Quick-Ask/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| anyhow!("Could not prepare the sign-in connection."))?;
        Ok(Self {
            data_dir,
            installation,
            credentials,
            http,
        })
    }

    pub fn account(&self) -> Option<AccountSummary> {
        self.credentials.as_ref().map(|credentials| AccountSummary {
            email: credentials.email.clone(),
            subject: credentials.subject.clone(),
        })
    }

    pub fn sign_in(&mut self, on_status: impl Fn(String)) -> Result<AccountSummary> {
        self.sign_in_cancellable(on_status, &AtomicBool::new(false))
    }

    pub fn sign_in_cancellable(
        &mut self,
        on_status: impl Fn(String),
        cancel: &AtomicBool,
    ) -> Result<AccountSummary> {
        check_sign_in_cancelled(cancel)?;
        on_status("Preparing secure sign-in…".into());
        let discovery = self.discovery()?;
        check_sign_in_cancelled(cancel)?;
        // Bind before launching the browser. The exact ephemeral port is retained
        // for both authorization and the one-time code exchange.
        let callback = Loopback::bind()?;
        let attempt = Attempt::new(callback.redirect_uri.clone())?;
        let selected_client = self
            .installation
            .registration
            .as_ref()
            .map(|registration| registration.client_id.as_str())
            .or(self.installation.pending_client_id.as_deref());
        let authorize = authorization_url(
            &discovery,
            &self.installation,
            self.credentials.as_ref(),
            &attempt,
        )?;
        check_sign_in_cancelled(cancel)?;
        on_status("Finish signing in with ChatGPT in your browser…".into());
        open::that(authorize.as_str()).map_err(|_| {
            anyhow!("Could not open your browser. Set a default browser and try again.")
        })?;
        let (code, client_id) = callback.wait_cancellable(
            &attempt.state,
            selected_client,
            Duration::from_secs(180),
            cancel,
        )?;
        check_sign_in_cancelled(cancel)?;
        // A completed registration is reused even if its authorization code expires.
        // Its subject is not trusted or assigned until ID-token validation succeeds.
        if self.installation.registration.is_none() {
            self.installation.pending_client_id = Some(client_id.clone());
            save_installation(&self.data_dir, &self.installation)?;
        }
        on_status("Verifying your ChatGPT account…".into());
        let token = self
            .token_request(
                &discovery.token_endpoint,
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", &client_id),
                    ("code", &code),
                    ("code_verifier", &attempt.verifier),
                    ("redirect_uri", &attempt.redirect_uri),
                    ("resource", RESOURCE),
                ],
            )
            .map_err(|failure| anyhow!(failure.message))?;
        check_sign_in_cancelled(cancel)?;
        let id_token = token
            .id_token
            .as_deref()
            .ok_or_else(|| anyhow!("ChatGPT did not return an identity token. Sign in again."))?;
        let keys = self.jwks(&discovery.jwks_uri)?;
        check_sign_in_cancelled(cancel)?;
        let expected_subject = self
            .installation
            .registration
            .as_ref()
            .map(|registration| registration.subject.as_str());
        let identity = validate_identity(
            id_token,
            &keys,
            &client_id,
            Some(&attempt.nonce),
            expected_subject,
        )?;
        let credentials = credentials_from_response(
            token,
            None,
            &identity,
            &client_id,
            &self.installation.ext_agent_host_id,
            &attempt.nonce,
            unix_now()?,
        )?;
        check_sign_in_cancelled(cancel)?;
        self.persist_credentials(credentials)?;
        self.account()
            .ok_or_else(|| anyhow!("Sign-in did not complete."))
    }

    pub fn access_token(&mut self) -> Result<String> {
        let old = self
            .credentials
            .as_ref()
            .ok_or_else(|| anyhow!("Continue with ChatGPT to ask a question."))?
            .clone();
        if old.expires_at > unix_now()?.saturating_add(60) {
            return Ok(old.access_token);
        }
        let discovery = self.discovery()?;
        let token = match self.token_request(
            &discovery.token_endpoint,
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &old.client_id),
                ("refresh_token", &old.refresh_token),
                ("resource", RESOURCE),
            ],
        ) {
            Ok(token) => token,
            Err(failure) => {
                if failure.terminal {
                    self.clear_local()?;
                }
                bail!(failure.message);
            }
        };
        let identity = if let Some(id_token) = token.id_token.as_deref() {
            let identity = validate_identity(
                id_token,
                &self.jwks(&discovery.jwks_uri)?,
                &old.client_id,
                None,
                Some(&old.subject),
            )?;
            if let Some(nonce) = &identity.nonce {
                ensure!(
                    nonce == &old.auth_nonce,
                    "The renewed identity token did not match this session. Sign in again."
                );
            }
            identity
        } else {
            // OIDC permits a refresh response to omit its ID token. Identity and
            // client remain bound to the previously verified token set.
            IdentityClaims {
                iss: ISSUER.into(),
                sub: old.subject.clone(),
                aud: serde_json::json!(old.client_id),
                exp: old.expires_at,
                iat: 0,
                nonce: None,
                email: Some(old.email.clone()),
                azp: None,
            }
        };
        let credentials = credentials_from_response(
            token,
            Some(&old),
            &identity,
            &old.client_id,
            &old.ext_agent_host_id,
            &old.auth_nonce,
            unix_now()?,
        )?;
        let access_token = credentials.access_token.clone();
        self.persist_credentials(credentials)?;
        Ok(access_token)
    }

    pub fn sign_out(&mut self) -> Result<()> {
        let remotely_revoked = if let Some(credentials) = &self.credentials {
            if let Ok(discovery) = self.discovery() {
                self.revoke(&discovery.revocation_endpoint, credentials)
            } else {
                false
            }
        } else {
            true
        };
        self.clear_local()?;
        ensure!(
            remotely_revoked,
            "Signed out locally. Remote revocation could not be confirmed; you can disconnect Quick Ask in ChatGPT Settings."
        );
        Ok(())
    }

    fn discovery(&self) -> Result<Discovery> {
        let response = self.http.get(DISCOVERY).send().map_err(|_| {
            anyhow!("Cannot reach ChatGPT sign-in. Check your connection and try again.")
        })?;
        ensure!(
            response.status().is_success(),
            "ChatGPT sign-in is temporarily unavailable. Try again later."
        );
        let discovery: Discovery = read_json(response)?;
        ensure!(
            discovery.issuer == ISSUER,
            "ChatGPT returned an unexpected identity provider."
        );
        for endpoint in [
            &discovery.authorization_endpoint,
            &discovery.token_endpoint,
            &discovery.jwks_uri,
            &discovery.revocation_endpoint,
        ] {
            trusted_endpoint(endpoint)?;
        }
        Ok(discovery)
    }

    fn jwks(&self, endpoint: &str) -> Result<JwkSet> {
        let response = self.http.get(endpoint).send().map_err(|_| {
            anyhow!("Could not fetch ChatGPT identity verification keys. Try again.")
        })?;
        ensure!(
            response.status().is_success(),
            "ChatGPT identity verification keys are temporarily unavailable."
        );
        // Fetch on every identity-token validation, including after an unknown kid.
        // Sign-ins are infrequent; this avoids stale keys without a persistent cache.
        read_json(response)
    }

    fn token_request(
        &self,
        endpoint: &str,
        form: &[(&str, &str)],
    ) -> std::result::Result<TokenResponse, TokenFailure> {
        let response = self
            .http
            .post(endpoint)
            .form(form)
            .send()
            .map_err(|_| TokenFailure {
                terminal: false,
                message: "Cannot reach ChatGPT. Your saved session is retained; try again.",
            })?;
        let status = response.status();
        if status.is_success() {
            return read_json(response).map_err(|_| TokenFailure { terminal: false,
                message: "ChatGPT returned an incomplete token response. Please try signing in again." });
        }
        let body: serde_json::Value = read_json(response).unwrap_or_default();
        let code = body
            .get("error")
            .and_then(|error| {
                error
                    .as_str()
                    .or_else(|| error.get("code").and_then(serde_json::Value::as_str))
            })
            .unwrap_or("");
        Err(classify_token_failure(status.as_u16(), code))
    }

    fn persist_credentials(&mut self, credentials: Credentials) -> Result<()> {
        let registration = Registration {
            client_id: credentials.client_id.clone(),
            subject: credentials.subject.clone(),
            email: credentials.email.clone(),
        };
        let key = credential_key(&self.installation.ext_agent_host_id, &registration);
        let bytes = serde_json::to_vec(&credentials)
            .map_err(|_| anyhow!("Could not prepare the protected session."))?;
        set_generic_password(KEYCHAIN_SERVICE, &key, &bytes).map_err(|_| {
            anyhow!(
                "Could not save your session in macOS Keychain. Unlock Keychain and sign in again."
            )
        })?;
        // Update in-memory tokens immediately after the atomic Keychain write so
        // an unrelated metadata failure cannot reuse a rotated refresh token.
        self.credentials = Some(credentials);
        self.installation.registration = Some(registration);
        self.installation.pending_client_id = None;
        self.installation.signed_in = true;
        save_installation(&self.data_dir, &self.installation)
    }

    fn clear_local(&mut self) -> Result<()> {
        self.credentials = None;
        self.installation.signed_in = false;
        // This marker prevents a deleted/locked Keychain item from being loaded
        // after restart if macOS cannot remove it immediately.
        let metadata_result = save_installation(&self.data_dir, &self.installation);
        let deletion_result = if let Some(registration) = &self.installation.registration {
            let key = credential_key(&self.installation.ext_agent_host_id, registration);
            match delete_generic_password(KEYCHAIN_SERVICE, &key) {
                Ok(()) => Ok(()),
                Err(error) if error.code() == KEYCHAIN_ITEM_NOT_FOUND => Ok(()),
                Err(_) => Err(anyhow!(
                    "Signed out in this app, but macOS could not remove the Keychain session. Unlock Keychain and sign out again."
                )),
            }
        } else {
            Ok(())
        };
        metadata_result?;
        deletion_result
    }

    fn revoke(&self, endpoint: &str, credentials: &Credentials) -> bool {
        for delay_ms in [0, 250, 750] {
            thread::sleep(Duration::from_millis(delay_ms));
            match self
                .http
                .post(endpoint)
                .form(&[
                    ("token", credentials.refresh_token.as_str()),
                    ("token_type_hint", "refresh_token"),
                    ("client_id", credentials.client_id.as_str()),
                ])
                .send()
            {
                Ok(response) if response.status().as_u16() == 200 => return true,
                Ok(response) if !response.status().is_server_error() => return false,
                _ => {}
            }
        }
        false
    }
}

struct Attempt {
    state: String,
    nonce: String,
    verifier: String,
    redirect_uri: String,
}

impl Attempt {
    fn new(redirect_uri: String) -> Result<Self> {
        Ok(Self {
            state: random_secret()?,
            nonce: random_secret()?,
            verifier: random_secret()?,
            redirect_uri,
        })
    }
}

fn random_secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| anyhow!("Secure random data is unavailable. Try again."))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn authorization_url(
    discovery: &Discovery,
    installation: &Installation,
    credentials: Option<&Credentials>,
    attempt: &Attempt,
) -> Result<Url> {
    let client_id = installation
        .registration
        .as_ref()
        .map(|registration| registration.client_id.as_str())
        .or(installation.pending_client_id.as_deref())
        .unwrap_or(DYNAMIC_CLIENT);
    let mut url = trusted_endpoint(&discovery.authorization_endpoint)?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("client_id", client_id)
            .append_pair("ext_agent_host_id", &installation.ext_agent_host_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &attempt.redirect_uri)
            .append_pair("scope", SCOPES)
            .append_pair("resource", RESOURCE)
            .append_pair("state", &attempt.state)
            .append_pair("nonce", &attempt.nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", &pkce_challenge(&attempt.verifier));
        if client_id == DYNAMIC_CLIENT {
            query.append_pair("agent_name_hint", "Quick Ask");
        } else if let Some(registration) = &installation.registration {
            query.append_pair("login_hint", &registration.email);
            if let Some(credentials) = credentials {
                query.append_pair("id_token_hint", &credentials.id_token);
            }
        }
    }
    Ok(url)
}

#[derive(Debug, PartialEq)]
enum Callback {
    Ignore,
    Code { code: String, client_id: String },
}

fn parse_callback(target: &str, state: &str, selected_client: Option<&str>) -> Result<Callback> {
    if !target.starts_with('/') || target.starts_with("//") {
        return Ok(Callback::Ignore);
    }
    let url = Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|_| anyhow!("The sign-in callback was malformed."))?;
    if url.path() != "/auth/callback" || url.fragment().is_some() {
        return Ok(Callback::Ignore);
    }
    let mut parameters = HashMap::new();
    for (key, value) in url.query_pairs() {
        if ["state", "code", "client_id", "error"].contains(&key.as_ref()) {
            ensure!(
                parameters
                    .insert(key.into_owned(), value.into_owned())
                    .is_none(),
                "The sign-in callback contained duplicate parameters."
            );
        }
    }
    if parameters.get("state").map(String::as_str) != Some(state) {
        return Ok(Callback::Ignore);
    }
    if let Some(error) = parameters.get("error") {
        if error == "access_denied" {
            bail!(
                "ChatGPT sign-in or plan access was declined. Continue with ChatGPT to try again."
            );
        }
        bail!("ChatGPT could not complete sign-in. Please try again.");
    }
    let code = parameters
        .remove("code")
        .filter(|code| !code.is_empty())
        .ok_or_else(|| {
            anyhow!("ChatGPT did not return an authorization code. Please sign in again.")
        })?;
    let issued = parameters.remove("client_id");
    let client_id = match (selected_client, issued) {
        (Some(selected), Some(issued)) => {
            ensure!(
                selected == issued,
                "The sign-in callback did not match the selected account registration."
            );
            issued
        }
        (Some(selected), None) => selected.to_owned(),
        (None, Some(issued)) => issued,
        (None, None) => bail!("ChatGPT registration was incomplete. Please sign in again."),
    };
    ensure!(
        !client_id.is_empty() && client_id != DYNAMIC_CLIENT && client_id.len() <= 512,
        "ChatGPT did not return an issued account registration. Please sign in again."
    );
    Ok(Callback::Code { code, client_id })
}

fn terminal_refresh_error(code: &str) -> bool {
    matches!(
        code,
        "invalid_grant"
            | "invalid_refresh_token"
            | "token_expired"
            | "refresh_token_expired"
            | "refresh_token_invalidated"
            | "refresh_token_reused"
    )
}

fn classify_token_failure(status: u16, code: &str) -> TokenFailure {
    if status == 429 || status >= 500 {
        TokenFailure {
            terminal: false,
            message: "ChatGPT is temporarily unavailable. Your saved session is retained; try again later.",
        }
    } else if terminal_refresh_error(code) {
        TokenFailure {
            terminal: true,
            message: "Your ChatGPT session has expired or was disconnected. Continue with ChatGPT again.",
        }
    } else if code == "invalid_client" {
        TokenFailure {
            terminal: false,
            message: "ChatGPT rejected this app registration. Please try signing in again.",
        }
    } else {
        TokenFailure {
            terminal: false,
            message: "ChatGPT could not renew or complete sign-in. Your saved session is retained; please try again.",
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct IdentityClaims {
    iss: String,
    sub: String,
    aud: serde_json::Value,
    exp: u64,
    iat: u64,
    nonce: Option<String>,
    email: Option<String>,
    azp: Option<String>,
}

fn validate_identity(
    token: &str,
    keys: &JwkSet,
    client: &str,
    nonce: Option<&str>,
    subject: Option<&str>,
) -> Result<IdentityClaims> {
    let header =
        decode_header(token).map_err(|_| anyhow!("ChatGPT returned an invalid identity token."))?;
    ensure!(
        header.alg == Algorithm::RS256,
        "ChatGPT returned an unsupported identity signature."
    );
    let kid = header
        .kid
        .as_deref()
        .ok_or_else(|| anyhow!("ChatGPT identity token did not identify a signing key."))?;
    let jwk = keys.find(kid).ok_or_else(|| {
        anyhow!("ChatGPT identity signing key was unavailable. Please try signing in again.")
    })?;
    let key = DecodingKey::from_jwk(jwk)
        .map_err(|_| anyhow!("ChatGPT returned an invalid identity signing key."))?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.leeway = 5;
    validation.validate_nbf = true;
    validation.set_required_spec_claims(&["iss", "aud", "sub", "exp"]);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[client]);
    if let Some(subject) = subject {
        validation.sub = Some(subject.into());
    }
    let identity = decode::<IdentityClaims>(token, &key, &validation)
        .map_err(|_| {
            anyhow!(
                "ChatGPT identity verification failed. Check your Mac's clock and sign in again."
            )
        })?
        .claims;
    ensure!(
        !identity.sub.trim().is_empty(),
        "ChatGPT identity did not contain an account identifier."
    );
    ensure!(
        identity.iat <= unix_now()?.saturating_add(5),
        "ChatGPT identity was issued in the future. Check your Mac's clock."
    );
    if let Some(nonce) = nonce {
        ensure!(
            identity.nonce.as_deref() == Some(nonce),
            "ChatGPT identity did not match this sign-in attempt."
        );
    }
    let audience_count = identity.aud.as_array().map_or(1, Vec::len);
    if audience_count > 1 || identity.azp.is_some() {
        ensure!(
            identity.azp.as_deref() == Some(client),
            "ChatGPT identity was authorized for a different client."
        );
    }
    Ok(identity)
}

struct Loopback {
    listener: TcpListener,
    redirect_uri: String,
}

impl Loopback {
    fn bind() -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|_| {
            anyhow!("Could not start the local sign-in callback. Please try again.")
        })?;
        let address = listener
            .local_addr()
            .map_err(|_| anyhow!("Could not determine the local sign-in address."))?;
        listener
            .set_nonblocking(true)
            .map_err(|_| anyhow!("Could not prepare the local sign-in callback."))?;
        Ok(Self {
            listener,
            redirect_uri: format!("http://127.0.0.1:{}/auth/callback", address.port()),
        })
    }

    fn wait_cancellable(
        self,
        state: &str,
        client: Option<&str>,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<(String, String)> {
        let deadline = Instant::now() + timeout;
        let authority = self
            .redirect_uri
            .strip_prefix("http://")
            .and_then(|uri| uri.split('/').next())
            .unwrap_or_default();
        loop {
            check_sign_in_cancelled(cancel)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "ChatGPT sign-in timed out. Continue with ChatGPT to try again."
            );
            match self.listener.accept() {
                Ok((mut stream, peer)) => {
                    if !peer.ip().is_loopback() {
                        continue;
                    }
                    // Accepted sockets inherit O_NONBLOCK on macOS. Read each
                    // callback as a bounded blocking request so packet boundaries
                    // do not turn a legitimate browser redirect into a failure.
                    if stream.set_nonblocking(false).is_err() {
                        continue;
                    }
                    let _ = stream.set_read_timeout(Some(remaining.min(Duration::from_secs(2))));
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
                    let request_deadline = deadline.min(Instant::now() + Duration::from_secs(2));
                    let target = match read_callback_request(
                        &mut stream,
                        authority,
                        request_deadline,
                        cancel,
                    ) {
                        Ok(target) => target,
                        Err(_) => {
                            check_sign_in_cancelled(cancel)?;
                            reply(&mut stream, false);
                            continue;
                        }
                    };
                    match parse_callback(&target, state, client) {
                        Ok(Callback::Code { code, client_id }) => {
                            check_sign_in_cancelled(cancel)?;
                            reply(&mut stream, true);
                            return Ok((code, client_id));
                        }
                        Ok(Callback::Ignore) => reply(&mut stream, false),
                        Err(error) => {
                            reply(&mut stream, false);
                            // Unsolicited malformed local requests must not abort a
                            // legitimate pending sign-in whose state they lack.
                            if callback_has_state(&target, state) {
                                return Err(error);
                            }
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(remaining.min(Duration::from_millis(25)));
                }
                Err(_) => {
                    bail!("The local sign-in callback stopped unexpectedly. Please try again.")
                }
            }
        }
    }
}

fn read_callback_request(
    stream: &mut TcpStream,
    authority: &str,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<String> {
    let mut request = Vec::new();
    let mut buffer = [0u8; 2048];
    while !request.windows(4).any(|part| part == b"\r\n\r\n") {
        check_sign_in_cancelled(cancel)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "Callback request timed out.");
        stream
            .set_read_timeout(Some(remaining.min(Duration::from_millis(100))))
            .map_err(|_| anyhow!("Could not limit the callback request."))?;
        let size = match stream.read(&mut buffer) {
            Ok(size) => size,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(_) => bail!("Incomplete callback request."),
        };
        ensure!(
            size > 0 && request.len() + size <= 16_384,
            "Invalid callback request."
        );
        request.extend_from_slice(&buffer[..size]);
    }
    let request =
        std::str::from_utf8(&request).map_err(|_| anyhow!("Invalid callback encoding."))?;
    let mut lines = request.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    ensure!(first.next() == Some("GET"), "Invalid callback method.");
    let target = first
        .next()
        .ok_or_else(|| anyhow!("Missing callback target."))?;
    ensure!(
        matches!(first.next(), Some("HTTP/1.1" | "HTTP/1.0")) && first.next().is_none(),
        "Invalid callback protocol."
    );
    let hosts: Vec<_> = lines
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
        .map(|(_, value)| value.trim())
        .collect();
    ensure!(
        hosts.len() == 1 && hosts[0] == authority,
        "Invalid callback host."
    );
    Ok(target.into())
}

fn check_sign_in_cancelled(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Relaxed),
        "ChatGPT sign-in was canceled."
    );
    Ok(())
}

fn callback_has_state(target: &str, expected: &str) -> bool {
    if !target.starts_with("/auth/callback?") {
        return false;
    }
    let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) else {
        return false;
    };
    let states: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .collect();
    states.len() == 1 && states[0] == expected
}

fn reply(stream: &mut TcpStream, received: bool) {
    let (status, text) = if received {
        (
            "200 OK",
            "Sign-in received. Return to Quick Ask while your account is verified. You can close this tab.",
        )
    } else {
        (
            "400 Bad Request",
            "This sign-in callback was not accepted. Return to Quick Ask and try again if needed.",
        )
    };
    let body = format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>Quick Ask</title><body><h1>Quick Ask</h1><p>{text}</p></body></html>"
    );
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; frame-ancestors 'none'\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn trusted_endpoint(endpoint: &str) -> Result<Url> {
    let url = Url::parse(endpoint)
        .map_err(|_| anyhow!("ChatGPT returned an invalid sign-in endpoint."))?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("auth.openai.com")
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "ChatGPT returned an untrusted sign-in endpoint."
    );
    Ok(url)
}

fn unix_now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn read_json<T: serde::de::DeserializeOwned>(response: Response) -> Result<T> {
    let mut bytes = Vec::new();
    response
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow!("ChatGPT's response could not be read. Please try again."))?;
    ensure!(
        bytes.len() as u64 <= MAX_RESPONSE_BYTES,
        "ChatGPT returned an unexpectedly large sign-in response."
    );
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow!("ChatGPT returned an invalid sign-in response."))
}

fn credential_key(host: &str, registration: &Registration) -> String {
    // Hash a structured tuple to avoid delimiter ambiguity and exposing account
    // identifiers in Keychain labels. The Keychain itself protects the full record.
    let binding =
        serde_json::to_vec(&(ISSUER, host, &registration.client_id, &registration.subject))
            .expect("a tuple of strings can be serialized");
    URL_SAFE_NO_PAD.encode(Sha256::digest(binding))
}

fn load_credentials(host: &str, registration: &Registration) -> Result<Option<Credentials>> {
    let bytes = match get_generic_password(KEYCHAIN_SERVICE, &credential_key(host, registration)) {
        Ok(bytes) => bytes,
        Err(error) if error.code() == KEYCHAIN_ITEM_NOT_FOUND => return Ok(None),
        Err(_) => bail!(
            "Could not read the ChatGPT session from macOS Keychain. Unlock Keychain and try again."
        ),
    };
    let credentials: Credentials = serde_json::from_slice(&bytes).map_err(|_| {
        anyhow!("The saved ChatGPT session is damaged. Sign out and sign in again.")
    })?;
    ensure!(
        credentials.issuer == ISSUER
            && credentials.client_id == registration.client_id
            && credentials.subject == registration.subject
            && credentials.ext_agent_host_id == host,
        "The saved ChatGPT session does not match this account registration."
    );
    ensure!(
        !credentials.access_token.is_empty()
            && !credentials.refresh_token.is_empty()
            && !credentials.id_token.is_empty()
            && credentials.scopes.iter().any(|scope| scope == PLAN_SCOPE),
        "The saved ChatGPT session does not include plan access. Continue with ChatGPT again."
    );
    Ok(Some(credentials))
}

fn load_installation(data_dir: &Path) -> Result<Installation> {
    prepare_directory(data_dir)?;
    let path = data_dir.join("chatgpt-account.json");
    match fs::read(&path) {
        Ok(bytes) => {
            let installation: Installation = serde_json::from_slice(&bytes)
                .context("The saved ChatGPT account registration is damaged.")?;
            let host_uuid = installation
                .ext_agent_host_id
                .strip_prefix("urn:uuid:")
                .and_then(|host| uuid::Uuid::parse_str(host).ok());
            ensure!(
                host_uuid.is_some(),
                "The saved ChatGPT host identifier is invalid."
            );
            if let Some(registration) = &installation.registration {
                ensure!(
                    !registration.client_id.is_empty()
                        && registration.client_id != DYNAMIC_CLIENT
                        && !registration.subject.is_empty(),
                    "The saved ChatGPT account registration is invalid."
                );
            }
            if let Some(pending) = &installation.pending_client_id {
                ensure!(
                    !pending.is_empty() && pending != DYNAMIC_CLIENT,
                    "The pending ChatGPT registration is invalid."
                );
            }
            Ok(installation)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let installation = Installation {
                ext_agent_host_id: format!("urn:uuid:{}", uuid::Uuid::new_v4()),
                registration: None,
                pending_client_id: None,
                signed_in: false,
            };
            save_installation(data_dir, &installation)?;
            Ok(installation)
        }
        Err(error) => Err(error).context("Could not read the saved ChatGPT account registration."),
    }
}

fn prepare_directory(data_dir: &Path) -> Result<()> {
    fs::create_dir_all(data_dir).context("Could not create the Quick Ask data directory.")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(data_dir, fs::Permissions::from_mode(0o700))
            .context("Could not protect the Quick Ask data directory.")?;
    }
    Ok(())
}

fn save_installation(data_dir: &Path, installation: &Installation) -> Result<()> {
    prepare_directory(data_dir)?;
    let temporary = data_dir.join(format!(".chatgpt-account-{}.tmp", uuid::Uuid::new_v4()));
    let path = data_dir.join("chatgpt-account.json");
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .context("Could not prepare the account registration file.")?;
        let bytes = serde_json::to_vec_pretty(installation)
            .context("Could not encode the account registration.")?;
        file.write_all(&bytes)
            .context("Could not save the account registration.")?;
        file.sync_all()
            .context("Could not synchronize the account registration.")?;
        fs::rename(&temporary, &path).context("Could not replace the account registration.")?;
        fs::File::open(data_dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn credentials_from_response(
    token: TokenResponse,
    previous: Option<&Credentials>,
    identity: &IdentityClaims,
    client: &str,
    host: &str,
    nonce: &str,
    now: u64,
) -> Result<Credentials> {
    ensure!(
        token.token_type.eq_ignore_ascii_case("bearer"),
        "ChatGPT returned an unsupported token type."
    );
    ensure!(
        !token.access_token.is_empty() && token.expires_in > 0,
        "ChatGPT did not return a usable access token."
    );
    let scopes = if let Some(scope) = &token.scope {
        scope
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    } else if let Some(previous) = previous {
        previous.scopes.clone()
    } else {
        Vec::new()
    };
    ensure!(
        scopes.iter().any(|scope| scope == PLAN_SCOPE),
        "ChatGPT plan access is not enabled for Quick Ask. Enable it in ChatGPT Settings and Continue with ChatGPT again."
    );
    let refresh_token = token
        .refresh_token
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            anyhow!("ChatGPT did not return a renewable session. Continue with ChatGPT again.")
        })?;
    let id_token = token
        .id_token
        .filter(|token| !token.is_empty())
        .or_else(|| previous.map(|previous| previous.id_token.clone()))
        .ok_or_else(|| {
            anyhow!("ChatGPT did not return an identity token. Continue with ChatGPT again.")
        })?;
    let expires_at = now
        .checked_add(token.expires_in)
        .ok_or_else(|| anyhow!("ChatGPT returned an invalid session expiry."))?;
    Ok(Credentials {
        issuer: ISSUER.into(),
        client_id: client.into(),
        ext_agent_host_id: host.into(),
        subject: identity.sub.clone(),
        email: identity
            .email
            .clone()
            .or_else(|| previous.map(|previous| previous.email.clone()))
            .unwrap_or_else(|| "ChatGPT account".into()),
        access_token: token.access_token,
        refresh_token,
        id_token,
        scopes,
        expires_at,
        auth_nonce: nonce.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_the_rfc_7636_s256_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn callback_requires_matching_state_before_interpreting_errors() {
        assert_eq!(
            parse_callback(
                "/auth/callback?state=wrong&error=access_denied",
                "expected",
                None
            )
            .unwrap(),
            Callback::Ignore
        );
        assert!(
            parse_callback(
                "/auth/callback?state=expected&error=access_denied",
                "expected",
                None
            )
            .unwrap_err()
            .to_string()
            .contains("declined")
        );
    }

    #[test]
    fn callback_uses_the_issued_registration_client() {
        assert_eq!(
            parse_callback(
                "/auth/callback?state=expected&code=one%2Btwo&client_id=oaiapp_issued",
                "expected",
                None
            )
            .unwrap(),
            Callback::Code {
                code: "one+two".into(),
                client_id: "oaiapp_issued".into()
            }
        );
        for target in [
            "/auth/callback?state=expected&code=code",
            "/auth/callback?state=expected&code=code&client_id=dynamic_agent_client",
            "/auth/callback?state=expected&code=code&client_id=",
        ] {
            assert!(parse_callback(target, "expected", None).is_err());
        }
    }

    #[test]
    fn callback_cannot_replace_an_existing_registration_client() {
        assert!(
            parse_callback(
                "/auth/callback?state=expected&code=code&client_id=oaiapp_attacker",
                "expected",
                Some("oaiapp_saved")
            )
            .is_err()
        );
        assert_eq!(
            parse_callback(
                "/auth/callback?state=expected&code=code",
                "expected",
                Some("oaiapp_saved")
            )
            .unwrap(),
            Callback::Code {
                code: "code".into(),
                client_id: "oaiapp_saved".into()
            }
        );
    }

    #[test]
    fn callback_rejects_ambiguous_parameters_and_other_routes() {
        for target in [
            "/auth/callback?state=expected&state=wrong&code=code&client_id=oaiapp_issued",
            "/auth/callback?state=expected&code=a&code=b&client_id=oaiapp_issued",
            "/auth/callback?state=expected&code=&client_id=oaiapp_issued",
        ] {
            assert!(parse_callback(target, "expected", None).is_err());
        }
        assert_eq!(
            parse_callback("/callback?state=expected&code=code", "expected", None).unwrap(),
            Callback::Ignore
        );
        assert_eq!(
            parse_callback(
                "https://evil.test/auth/callback?state=expected&code=code",
                "expected",
                None
            )
            .unwrap(),
            Callback::Ignore
        );
    }

    #[test]
    fn only_documented_terminal_refresh_errors_discard_sessions() {
        for code in [
            "invalid_grant",
            "invalid_refresh_token",
            "token_expired",
            "refresh_token_expired",
            "refresh_token_invalidated",
            "refresh_token_reused",
        ] {
            assert!(terminal_refresh_error(code), "{code}");
        }
        for code in [
            "server_error",
            "temporarily_unavailable",
            "invalid_client",
            "",
            "unknown",
        ] {
            assert!(!terminal_refresh_error(code), "{code}");
        }
    }

    fn signed_claims(mut edit: impl FnMut(&mut serde_json::Value)) -> String {
        let now = jsonwebtoken::get_current_timestamp();
        let mut claims = serde_json::json!({
            "iss": "https://auth.openai.com", "sub": "subject", "aud": ["oaiapp_saved"],
            "exp": now + 3600, "iat": now, "nonce": "expected", "email": "person@example.test"
        });
        edit(&mut claims);
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some("test-key".into());
        jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_rsa_pem(include_bytes!("auth/test-key.pem")).unwrap(),
        )
        .unwrap()
    }

    fn test_keys() -> jsonwebtoken::jwk::JwkSet {
        serde_json::from_str(include_str!("auth/test-jwks.json")).unwrap()
    }

    #[test]
    fn identity_verifies_signature_and_expected_issuer_audience_nonce_subject() {
        let token = signed_claims(|_| {});
        let claims = validate_identity(
            &token,
            &test_keys(),
            "oaiapp_saved",
            Some("expected"),
            Some("subject"),
        )
        .unwrap();
        assert_eq!(claims.sub, "subject");
        assert!(
            validate_identity(&token, &test_keys(), "other_client", Some("expected"), None)
                .is_err()
        );
        assert!(
            validate_identity(
                &token,
                &test_keys(),
                "oaiapp_saved",
                Some("other_nonce"),
                None
            )
            .is_err()
        );
        assert!(
            validate_identity(
                &token,
                &test_keys(),
                "oaiapp_saved",
                Some("expected"),
                Some("other_subject")
            )
            .is_err()
        );
        let mut tampered: Vec<_> = token.split('.').map(str::to_owned).collect();
        let replacement = if tampered[2].starts_with('A') {
            "B"
        } else {
            "A"
        };
        tampered[2].replace_range(0..1, replacement);
        assert!(
            validate_identity(
                &tampered.join("."),
                &test_keys(),
                "oaiapp_saved",
                Some("expected"),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn identity_rejects_expired_future_issued_missing_and_ambiguous_claims() {
        let cases = [
            ("iss", serde_json::json!("https://attacker.test")),
            ("exp", serde_json::json!(1)),
            (
                "iat",
                serde_json::json!(jsonwebtoken::get_current_timestamp() + 600),
            ),
            ("sub", serde_json::json!("")),
            ("nonce", serde_json::Value::Null),
            ("aud", serde_json::json!(["oaiapp_saved", "other_client"])),
            ("azp", serde_json::json!("other_client")),
        ];
        for (key, value) in cases {
            let token = signed_claims(|claims| claims[key] = value.clone());
            assert!(
                validate_identity(&token, &test_keys(), "oaiapp_saved", Some("expected"), None)
                    .is_err(),
                "{key}"
            );
        }
    }

    fn installation() -> Installation {
        Installation {
            ext_agent_host_id: "urn:uuid:test-host".into(),
            registration: None,
            pending_client_id: None,
            signed_in: false,
        }
    }

    fn discovery() -> Discovery {
        Discovery {
            issuer: ISSUER.into(),
            authorization_endpoint: format!("{ISSUER}/api/accounts/authorize"),
            token_endpoint: format!("{ISSUER}/api/accounts/oauth/token"),
            jwks_uri: format!("{ISSUER}/.well-known/jwks.json"),
            revocation_endpoint: format!("{ISSUER}/api/accounts/oauth/revoke"),
        }
    }

    #[test]
    fn authorization_requests_plan_permission_with_fresh_pkce_and_correct_hints() {
        let first = Attempt::new("http://127.0.0.1:12345/auth/callback".into()).unwrap();
        let second = Attempt::new(first.redirect_uri.clone()).unwrap();
        assert_ne!(first.state, second.state);
        assert_ne!(first.nonce, second.nonce);
        assert_ne!(first.verifier, second.verifier);
        assert_eq!(first.verifier.len(), 43);
        let mut install = installation();
        let url = authorization_url(&discovery(), &install, None, &first).unwrap();
        let query: HashMap<_, _> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(query["client_id"], "dynamic_agent_client");
        assert_eq!(query["agent_name_hint"], "Quick Ask");
        assert_eq!(query["resource"], "https://api.openai.com/v1");
        assert_eq!(
            query["redirect_uri"],
            "http://127.0.0.1:12345/auth/callback"
        );
        assert!(
            query["scope"]
                .split_whitespace()
                .any(|s| s == "chatgpt.tokens.use.direct")
        );
        assert!(!query.contains_key("id_token_hint"));
        install.registration = Some(Registration {
            client_id: "oaiapp_saved".into(),
            subject: "subject".into(),
            email: "person@example.test".into(),
        });
        let url = authorization_url(&discovery(), &install, None, &first).unwrap();
        let query: HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(query["client_id"], "oaiapp_saved");
        assert_eq!(query["login_hint"], "person@example.test");
        assert!(!query.contains_key("agent_name_hint"));
    }

    #[test]
    fn discovered_endpoints_cannot_redirect_credentials_to_another_origin() {
        assert!(trusted_endpoint("https://auth.openai.com/api/accounts/oauth/token").is_ok());
        for endpoint in [
            "http://auth.openai.com/token",
            "https://evil.test/token",
            "https://auth.openai.com.evil.test/token",
            "https://user@auth.openai.com/token",
            "https://auth.openai.com:444/token",
            "https://auth.openai.com/token?access_token=secret",
            "https://auth.openai.com/token#fragment",
        ] {
            assert!(trusted_endpoint(endpoint).is_err(), "{endpoint}");
        }
    }

    #[test]
    fn host_identity_and_account_mapping_survive_logout_without_tokens_on_disk() {
        let directory = tempfile::tempdir().unwrap();
        let mut install = load_installation(directory.path()).unwrap();
        assert!(install.ext_agent_host_id.starts_with("urn:uuid:"));
        assert_eq!(
            load_installation(directory.path())
                .unwrap()
                .ext_agent_host_id,
            install.ext_agent_host_id
        );
        install.registration = Some(Registration {
            client_id: "oaiapp_saved".into(),
            subject: "subject".into(),
            email: "person@example.test".into(),
        });
        install.signed_in = false;
        save_installation(directory.path(), &install).unwrap();
        let restored = load_installation(directory.path()).unwrap();
        assert_eq!(restored.registration.unwrap().client_id, "oaiapp_saved");
        let metadata = fs::read_to_string(directory.path().join("chatgpt-account.json")).unwrap();
        assert!(!metadata.contains("access_token"));
        assert!(!metadata.contains("refresh_token"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(directory.path().join("chatgpt-account.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    fn token_response() -> TokenResponse {
        TokenResponse {
            access_token: "new-access".into(),
            refresh_token: Some("new-refresh".into()),
            id_token: Some("new-id".into()),
            token_type: "Bearer".into(),
            expires_in: 3600,
            scope: Some("openid offline_access resource.invoke chatgpt.tokens.use.direct".into()),
        }
    }

    #[test]
    fn token_sets_require_plan_scope_and_rotate_together() {
        let identity = IdentityClaims {
            iss: ISSUER.into(),
            sub: "subject".into(),
            aud: serde_json::json!("oaiapp_saved"),
            exp: 4600,
            iat: 1000,
            nonce: Some("nonce".into()),
            email: Some("person@example.test".into()),
            azp: None,
        };
        let prior = credentials_from_response(
            token_response(),
            None,
            &identity,
            "oaiapp_saved",
            "host",
            "nonce",
            1000,
        )
        .unwrap();
        assert_eq!(prior.expires_at, 4600);
        let mut response = token_response();
        response.access_token = "rotated-access".into();
        response.refresh_token = Some("rotated-refresh".into());
        response.id_token = None;
        response.scope = None;
        let rotated = credentials_from_response(
            response,
            Some(&prior),
            &identity,
            "oaiapp_saved",
            "host",
            "nonce",
            2000,
        )
        .unwrap();
        assert_eq!(rotated.access_token, "rotated-access");
        assert_eq!(rotated.refresh_token, "rotated-refresh");
        assert_eq!(rotated.id_token, "new-id");
        assert_eq!(rotated.expires_at, 5600);
        assert!(
            rotated
                .scopes
                .iter()
                .any(|scope| scope == "chatgpt.tokens.use.direct")
        );
        let mut response = token_response();
        response.scope = Some("openid email".into());
        assert!(
            credentials_from_response(
                response,
                None,
                &identity,
                "oaiapp_saved",
                "host",
                "nonce",
                1000
            )
            .is_err()
        );
        let mut response = token_response();
        response.refresh_token = None;
        assert!(
            credentials_from_response(
                response,
                Some(&prior),
                &identity,
                "oaiapp_saved",
                "host",
                "nonce",
                1000
            )
            .is_err()
        );
        assert!(!classify_token_failure(503, "invalid_grant").terminal);
        assert!(!classify_token_failure(429, "invalid_grant").terminal);
    }

    #[test]
    fn loopback_ignores_a_wrong_state_then_receives_the_legitimate_code() {
        let listener = Loopback::bind().unwrap();
        let url = Url::parse(&listener.redirect_uri).unwrap();
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        let port = url.port().unwrap();
        let requests = thread::spawn(move || {
            for state in ["wrong", "expected"] {
                let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                write!(stream, "GET /auth/callback?state={state}&code=code&client_id=oaiapp_saved HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").unwrap();
                let mut response = String::new();
                stream.read_to_string(&mut response).unwrap();
                assert!(response.contains(if state == "wrong" {
                    "400 Bad Request"
                } else {
                    "200 OK"
                }));
                assert!(!response.contains("code=code"));
            }
        });
        let result = listener
            .wait_cancellable(
                "expected",
                None,
                Duration::from_secs(3),
                &AtomicBool::new(false),
            )
            .unwrap();
        requests.join().unwrap();
        assert_eq!(result, ("code".into(), "oaiapp_saved".into()));
    }

    #[test]
    fn loopback_wait_is_bounded() {
        let listener = Loopback::bind().unwrap();
        let start = Instant::now();
        assert!(
            listener
                .wait_cancellable(
                    "expected",
                    None,
                    Duration::from_millis(50),
                    &AtomicBool::new(false)
                )
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn loopback_deadline_also_bounds_a_slow_partial_request() {
        let listener = Loopback::bind().unwrap();
        let port = Url::parse(&listener.redirect_uri).unwrap().port().unwrap();
        let requester = thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            for _ in 0..50 {
                if stream.write_all(b"G").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(15));
            }
        });
        let start = Instant::now();
        assert!(
            listener
                .wait_cancellable(
                    "expected",
                    None,
                    Duration::from_millis(80),
                    &AtomicBool::new(false)
                )
                .is_err()
        );
        let elapsed = start.elapsed();
        requester.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(400),
            "partial request exceeded the deadline: {elapsed:?}"
        );
    }

    #[test]
    fn loopback_accepts_a_legitimate_request_split_across_packets() {
        let listener = Loopback::bind().unwrap();
        let port = Url::parse(&listener.redirect_uri).unwrap().port().unwrap();
        let requester = thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream.write_all(b"GET /auth/callback?state=expected&code=code&client_id=oaiapp_saved HTTP/1.1\r\n").unwrap();
            thread::sleep(Duration::from_millis(40));
            let _ = write!(stream, "Host: 127.0.0.1:{port}\r\n\r\n");
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
            response
        });
        let received = listener.wait_cancellable(
            "expected",
            None,
            Duration::from_secs(1),
            &AtomicBool::new(false),
        );
        let response = requester.join().unwrap();
        assert!(
            received.is_ok(),
            "fragmented legitimate callback was rejected: {response}"
        );
    }

    #[test]
    fn cancellation_stops_waiting_promptly_and_releases_the_listener() {
        use std::sync::{Arc, atomic::Ordering};
        let listener = Loopback::bind().unwrap();
        let port = Url::parse(&listener.redirect_uri).unwrap().port().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancel);
        let requester = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            signal.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let error = listener
            .wait_cancellable("expected", None, Duration::from_secs(2), &cancel)
            .unwrap_err();
        requester.join().unwrap();
        assert!(error.to_string().contains("canceled"), "{error}");
        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
    }

    #[test]
    fn cancellation_interrupts_an_open_partial_callback_socket() {
        use std::sync::{Arc, atomic::Ordering};
        let listener = Loopback::bind().unwrap();
        let port = Url::parse(&listener.redirect_uri).unwrap().port().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancel);
        let requester = thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream.write_all(b"GET /auth/callback?").unwrap();
            thread::sleep(Duration::from_millis(30));
            signal.store(true, Ordering::Relaxed);
            let mut body = Vec::new();
            stream.read_to_end(&mut body).unwrap();
        });
        let started = Instant::now();
        let error = listener
            .wait_cancellable("expected", None, Duration::from_secs(2), &cancel)
            .unwrap_err();
        requester.join().unwrap();
        assert!(error.to_string().contains("canceled"), "{error}");
        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    }

    #[test]
    fn cancellation_before_sign_in_preserves_the_active_session_and_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let mut manager = AuthManager::new(directory.path().into()).unwrap();
        let identity = IdentityClaims {
            iss: ISSUER.into(),
            sub: "subject".into(),
            aud: serde_json::json!("oaiapp_saved"),
            exp: 4600,
            iat: 1000,
            nonce: Some("nonce".into()),
            email: Some("person@example.test".into()),
            azp: None,
        };
        manager.credentials = Some(
            credentials_from_response(
                token_response(),
                None,
                &identity,
                "oaiapp_saved",
                "host",
                "nonce",
                1000,
            )
            .unwrap(),
        );
        let metadata = fs::read(directory.path().join("chatgpt-account.json")).unwrap();
        let error = manager
            .sign_in_cancellable(
                |_| panic!("Canceled sign-in emitted progress"),
                &AtomicBool::new(true),
            )
            .unwrap_err();
        assert!(error.to_string().contains("canceled"), "{error}");
        assert_eq!(manager.account().unwrap().subject, "subject");
        assert_eq!(
            manager.credentials.as_ref().unwrap().access_token,
            "new-access"
        );
        assert_eq!(
            manager.credentials.as_ref().unwrap().refresh_token,
            "new-refresh"
        );
        assert_eq!(
            fs::read(directory.path().join("chatgpt-account.json")).unwrap(),
            metadata
        );
    }
}
