//! HTTP side of the Muse client: browser session, native OTP login, hatch
//! bootstrap and the REST (Stefi) surface.
//!
//! Measured against the live service on 2026-11-06:
//!
//! ```text
//! GET  /                              -> 200 HTML (bootstrap JSON is inline once signed in)
//! POST /api/auth/native/restart       -> 201 { csrf_token, flow_state, tos, password_encryption }
//! POST /api/auth/native/send-otp      -> 201 { registration_state: "otp_sent", flow, has_muse_account }
//! POST /api/auth/native/confirm-otp   -> sets the hatch_sess cookie
//! POST /api/auth/save-account         -> commits the account binding
//! POST /api/auth/check                -> 200 { ... } while signed in, 401 hatchAdmissionInvalidated otherwise
//! POST /api/hatch/vm/wake             -> 200 wake acknowledgement (401 when signed out)
//! POST /api/hatch/token               -> 200 { token, notary_token } admission credentials
//! ```

use anyhow::{anyhow, bail, Context, Result};
use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, ORIGIN, REFERER};
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const MUSE_BASE: &str = "https://muse.ai";
pub const HATCH_API: &str = "https://hatch-api.meta.ai";
pub const SHARED_INGRESS_HOSTS: [&str; 3] = ["hatch.metaaivm.com", "vm.meta.ai", "vm.muse.ai"];

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

/// Persisted browser cookies plus the hatch admission credentials. Stored with
/// owner-only permissions outside any repository by default.
#[derive(Default, Serialize, Deserialize)]
pub struct SessionState {
    pub cookies: Vec<StoredCookie>,
    /// Abra access token issued by `/api/auth/check`. Authenticates the Stefi
    /// REST surface (`hatch-api.meta.ai/hatch/*`).
    #[serde(default)]
    pub abra_token: Option<String>,
    /// Gateway admission credentials from `/api/hatch/token`. This is a
    /// different token from the abra one and must not overwrite it.
    #[serde(default)]
    pub hatch_credential: Option<Value>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub saved_at: Option<String>,
    /// Login flow id issued by `/api/auth/native/restart`.
    #[serde(default)]
    pub waterfall_id: Option<String>,
    /// CSRF token bound to `waterfall_id`; required by the native auth steps.
    #[serde(default)]
    pub csrf_token: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct StoredCookie {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default = "root_path")]
    pub path: String,
    #[serde(default)]
    pub secure: bool,
}

fn root_path() -> String {
    "/".to_string()
}

impl SessionState {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => SessionState::default(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(self)?;
        // Write to a sibling temp file and rename: an interrupted write must not
        // leave a half-serialized session, and the temp file is created with
        // restrictive permissions before any credential bytes land in it.
        let temp = path.with_extension("json.tmp");
        write_private(&temp, &raw)?;
        std::fs::rename(&temp, path)
            .map_err(|err| anyhow!("cannot replace {}: {err}", path.display()))?;
        Ok(())
    }
}

/// Create `path` with owner-only permissions, then write the bytes.
#[cfg(unix)]
fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents)?;
    Ok(())
}

/// Default state location: `$MUSE_MCP_STATE`, else the OS config directory.
pub fn default_state_path() -> PathBuf {
    if let Ok(explicit) = std::env::var("MUSE_MCP_STATE") {
        return PathBuf::from(explicit);
    }
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(dir).join("muse-mcp").join("session.json");
    }
    if cfg!(windows) {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata).join("muse-mcp").join("session.json");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("muse-mcp")
            .join("session.json");
    }
    PathBuf::from(".muse-mcp-session.json")
}

pub struct MuseHttp {
    client: Client,
    jar: Arc<Jar>,
    state_path: PathBuf,
    state: SessionState,
    waterfall_id: String,
    csrf_token: Option<String>,
}

impl MuseHttp {
    pub fn new(state_path: PathBuf) -> Result<Self> {
        let jar = Arc::new(Jar::default());
        let state = SessionState::load(&state_path);
        for cookie in &state.cookies {
            if let Ok(url) = reqwest::Url::parse(&format!(
                "https://{}{}",
                if cookie.domain.is_empty() {
                    "muse.ai"
                } else {
                    cookie.domain.trim_start_matches('.')
                },
                cookie.path
            )) {
                jar.add_cookie_str(&format!("{}={}; Path=/", cookie.name, cookie.value), &url);
            }
        }
        let client = Client::builder()
            .cookie_provider(jar.clone())
            .user_agent(USER_AGENT)
            .gzip(true)
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        Ok(Self {
            client,
            jar,
            state_path,
            waterfall_id: state
                .waterfall_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            csrf_token: state.csrf_token.clone(),
            state,
        })
    }

    fn flow_headers(&self, csrf: bool) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
        headers.insert(ORIGIN, HeaderValue::from_static(MUSE_BASE));
        headers.insert(REFERER, HeaderValue::from_static("https://muse.ai/"));
        headers.insert(
            "Sec-Fetch-Site",
            HeaderValue::from_static("same-origin"),
        );
        headers.insert("Sec-Fetch-Mode", HeaderValue::from_static("cors"));
        headers.insert("Sec-Fetch-Dest", HeaderValue::from_static("empty"));
        headers.insert(
            "x-hatch-waterfall-id",
            HeaderValue::from_str(&self.waterfall_id).expect("ascii uuid"),
        );
        if csrf {
            if let Some(token) = &self.csrf_token {
                if let Ok(value) = HeaderValue::from_str(token) {
                    headers.insert("x-hatch-csrf-token", value);
                }
                headers.insert(
                    "x-hatch-caa-reg-entry-point",
                    HeaderValue::from_static("login_home"),
                );
            }
        }
        headers
    }

    /// Warm the browser session: the first navigation sets the `datr` style
    /// fingerprint cookies the later API calls expect.
    pub async fn prime(&mut self) -> Result<()> {
        let response = self
            .client
            .get(format!("{MUSE_BASE}/"))
            .header(
                ACCEPT,
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .send()
            .await
            .context("muse.ai bootstrap navigation failed")?;
        if !response.status().is_success() {
            bail!("muse.ai bootstrap navigation returned HTTP {}", response.status());
        }
        self.persist_cookies();
        Ok(())
    }

    fn persist_cookies(&mut self) {
        let mut cookies: Vec<StoredCookie> = Vec::new();
        for url in [
            "https://muse.ai/",
            "https://www.muse.ai/",
            "https://api.muse.ai/",
            "https://hatch-api.meta.ai/",
        ] {
            let parsed = match reqwest::Url::parse(url) {
                Ok(parsed) => parsed,
                Err(_) => continue,
            };
            // `Jar::cookies` yields one header value holding every cookie for
            // the domain, separated by `; `.
            let Some(header) = self.jar.cookies(&parsed) else {
                continue;
            };
            let Ok(raw) = header.to_str() else {
                continue;
            };
            for pair in raw.split(';') {
                let pair = pair.trim();
                let Some((name, value)) = pair.split_once('=') else {
                    continue;
                };
                if name.is_empty() || cookies.iter().any(|c| c.name == name) {
                    continue;
                }
                cookies.push(StoredCookie {
                    name: name.to_string(),
                    value: value.to_string(),
                    domain: parsed.host_str().unwrap_or("muse.ai").to_string(),
                    path: "/".to_string(),
                    secure: true,
                });
            }
        }
        if !cookies.is_empty() {
            self.state.cookies = cookies;
            let _ = self.state.save(&self.state_path);
        }
    }

    async fn expect_json(response: Response, what: &str) -> Result<Value> {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!(
                "{what} failed: HTTP {status}: {}",
                truncate(&text, 600)
            );
        }
        if text.trim().is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&text)
            .map_err(|err| anyhow!("{what} returned non-JSON payload ({err}): {}", truncate(&text, 400)))
    }

    pub async fn restart_login(&mut self) -> Result<Value> {
        self.prime().await?;
        self.waterfall_id = uuid::Uuid::new_v4().to_string();
        let response = self
            .client
            .post(format!("{MUSE_BASE}/api/auth/native/restart"))
            .headers(self.flow_headers(false))
            .json(&json!({}))
            .send()
            .await?;
        let value = Self::expect_json(response, "muse auth restart").await?;
        self.csrf_token = value
            .get("csrf_token")
            .and_then(Value::as_str)
            .map(str::to_string);
        if self.csrf_token.is_none() {
            bail!("muse auth restart did not return a csrf_token");
        }
        self.persist_cookies();
        self.persist_flow();
        Ok(value)
    }

    /// The native auth flow is stateful across processes: the waterfall id and
    /// CSRF token must survive a restart, otherwise `confirm-otp` is rejected
    /// with `csrfFailed`.
    fn persist_flow(&mut self) {
        self.state.waterfall_id = Some(self.waterfall_id.clone());
        self.state.csrf_token = self.csrf_token.clone();
        let _ = self.state.save(&self.state_path);
    }

    pub async fn send_otp(&mut self, contact_point: &str, region: &str) -> Result<Value> {
        let response = self
            .client
            .post(format!("{MUSE_BASE}/api/auth/native/send-otp"))
            .headers(self.flow_headers(true))
            .json(&json!({ "contact_point": contact_point, "region": region }))
            .send()
            .await?;
        let value = Self::expect_json(response, "muse send OTP").await?;
        self.persist_cookies();
        Ok(value)
    }

    pub async fn confirm_otp(&mut self, otp_code: &str) -> Result<Value> {
        let response = self
            .client
            .post(format!("{MUSE_BASE}/api/auth/native/confirm-otp"))
            .headers(self.flow_headers(true))
            .json(&json!({ "otp_code": otp_code }))
            .send()
            .await?;
        let value = Self::expect_json(response, "muse confirm OTP").await?;
        self.persist_cookies();
        Ok(value)
    }

    pub async fn save_account(&mut self) -> Result<Value> {
        let response = self
            .client
            .post(format!("{MUSE_BASE}/api/auth/save-account"))
            .headers(self.flow_headers(false))
            .send()
            .await?;
        let value = Self::expect_json(response, "muse save account").await?;
        self.persist_cookies();
        Ok(value)
    }

    pub async fn auth_check(&mut self) -> Result<Value> {
        let mut headers = self.flow_headers(false);
        headers.insert(REFERER, HeaderValue::from_static("https://muse.ai/thread/new"));
        let response = self
            .client
            .post(format!("{MUSE_BASE}/api/auth/check"))
            .headers(headers)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        self.persist_cookies();
        let mut value: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
        let derived_ok = status.is_success()
            && value
                .as_object()
                .map(|map| map.contains_key("viewer_id") || map.contains_key("access_token"))
                .unwrap_or(false);
        if let Some(token) = value.get("access_token").and_then(Value::as_str) {
            // The Stefi REST surface authenticates with this abra token, so keep
            // it alongside the cookies for later account calls.
            self.state.abra_token = Some(token.to_string());
            let _ = self.state.save(&self.state_path);
        }
        if let Value::Object(map) = &mut value {
            map.entry("ok").or_insert(json!(derived_ok));
            map.insert("status".into(), json!(status.as_u16()));
        }
        Ok(value)
    }


    /// Fetch the shell document and pull the inline bootstrap object from it.
    pub async fn bootstrap(&mut self) -> Result<Bootstrap> {
        let response = self
            .client
            .get(format!("{MUSE_BASE}/"))
            .header(
                ACCEPT,
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .send()
            .await?;
        if !response.status().is_success() {
            bail!("muse.ai bootstrap page returned HTTP {}", response.status());
        }
        let text = response.text().await?;
        self.persist_cookies();
        let gateway = find_json_string(&text, "gatewayUrl")
            .ok_or_else(|| anyhow!("bootstrap page did not expose gatewayUrl (is the session valid?)"))?;
        Ok(Bootstrap {
            gateway_url: gateway,
            vm_name: find_json_string(&text, "vmName").unwrap_or_default(),
            admission: find_json_string(&text, "authToken").unwrap_or_default(),
            notary: find_json_string(&text, "notaryToken"),
            locale: find_json_string(&text, "viewerLocale")
                .unwrap_or_else(|| "en-US".to_string())
                .replace('_', "-"),
        })
    }


    pub async fn hatch_token(&mut self, bootstrap: &Bootstrap) -> Result<Bootstrap> {
        let response = self
            .client
            .post(format!("{MUSE_BASE}/api/hatch/token"))
            .headers(self.api_headers())
            .json(&json!({
                "vmAddress": bootstrap.gateway_url,
                "vmName": bootstrap.vm_name,
            }))
            .send()
            .await?;
        let value = Self::expect_json(response, "muse hatch token").await?;
        self.persist_cookies();
        let mut next = bootstrap.clone();
        if let Some(token) = value.get("token").and_then(Value::as_str) {
            next.admission = token.to_string();
        }
        if let Some(notary) = value.get("notary_token").and_then(Value::as_str) {
            next.notary = Some(notary.to_string());
        }
        if next.admission.is_empty() {
            bail!("hatch admission credential is missing from both bootstrap and token response");
        }
        self.state.hatch_credential = Some(value);
        let _ = self.state.save(&self.state_path);
        Ok(next)
    }

    fn api_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
        headers.insert(ORIGIN, HeaderValue::from_static(MUSE_BASE));
        headers.insert(REFERER, HeaderValue::from_static("https://muse.ai/"));
        headers.insert("Sec-Fetch-Site", HeaderValue::from_static("same-origin"));
        headers.insert("Sec-Fetch-Mode", HeaderValue::from_static("cors"));
        headers.insert("Sec-Fetch-Dest", HeaderValue::from_static("empty"));
        headers
    }

    pub async fn logout(&mut self) -> Result<Value> {
        let response = self
            .client
            .post(format!("{MUSE_BASE}/api/logout"))
            .headers(self.api_headers())
            .send()
            .await?;
        let value = Self::expect_json(response, "muse logout").await?;
        self.state = SessionState::default();
        let _ = self.state.save(&self.state_path);
        Ok(value)
    }

    /// Arbitrary `muse.ai` call with the stored browser cookies. The CSRF header
    /// is attached only when `csrf` is requested, because the native auth flow
    /// rejects requests that carry it outside its own window.
    pub async fn web_api(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
        csrf: bool,
    ) -> Result<Value> {
        if !path.starts_with('/') {
            bail!("API path must start with '/'; got {path}");
        }
        if csrf && self.csrf_token.is_none() {
            bail!("no CSRF token cached; run muse_login_start first or set use_csrf=false");
        }
        let url = format!("{MUSE_BASE}{path}");
        let mut headers = if csrf {
            self.flow_headers(true)
        } else {
            self.api_headers()
        };
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let mut request = match method.to_ascii_uppercase().as_str() {
            "GET" => self.client.get(&url),
            "POST" => self.client.post(&url),
            "PUT" => self.client.put(&url),
            "PATCH" => self.client.patch(&url),
            "DELETE" => self.client.delete(&url),
            other => bail!("unsupported method {other}"),
        };
        request = request.headers(headers);
        if let Some(body) = body {
            request = request.header(CONTENT_TYPE, "application/json").json(&body);
        }
        let response = request.send().await?;
        let value =
            Self::expect_json(response, &format!("muse.ai {method} {path}")).await?;
        self.persist_cookies();
        Ok(value)
    }

    /// Stefi REST surface (`hatch-api.meta.ai/hatch/*`). Kept separate from the
    /// gateway because it only needs the abra bearer token, not Noise.
    pub async fn stefi_get(&self, path: &str) -> Result<Value> {
        self.stefi("GET", path, None).await
    }

    pub async fn stefi_post(&self, path: &str, body: Value) -> Result<Value> {
        self.stefi("POST", path, Some(body)).await
    }

    async fn stefi(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let token = self.state.abra_token.as_deref().ok_or_else(|| {
            anyhow!("no abra access token cached; run muse_login_verify or muse_status first")
        })?;
        let url = format!("{HATCH_API}{path}");
        let mut request = match method {
            "GET" => self.client.get(&url),
            "POST" => self.client.post(&url),
            other => bail!("unsupported Stefi method {other}"),
        };
        request = request
            .header("X-API-Version", "1.0.0")
            .header(ACCEPT, "application/json")
            .header(ACCEPT_LANGUAGE, "en-US")
            .header("Authorization", format!("Bearer {token}"));
        if let Some(body) = body {
            request = request.header(CONTENT_TYPE, "application/json").json(&body);
        }
        let response = request.send().await?;
        Self::expect_json(response, &format!("Stefi {method} {path}")).await
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push_str("…(truncated)");
    out
}

/// Extract a JSON string field from an HTML document without a full parse.
fn find_json_string(html: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let mut cursor = 0usize;
    while let Some(found) = html[cursor..].find(&needle) {
        let start = cursor + found + needle.len();
        let rest = &html[start..];
        let rest = rest.trim_start();
        let rest = rest.strip_prefix(':')?;
        let rest = rest.trim_start();
        if let Some(body) = rest.strip_prefix('"') {
            let mut out = String::new();
            let mut chars = body.char_indices();
            while let Some((index, ch)) = chars.next() {
                match ch {
                    '\\' => {
                        if let Some((_, escaped)) = chars.next() {
                            match escaped {
                                'n' => out.push('\n'),
                                't' => out.push('\t'),
                                'r' => out.push('\r'),
                                '"' => out.push('"'),
                                '\\' => out.push('\\'),
                                '/' => out.push('/'),
                                'u' => {
                                    let hex: String = body[index + 2..]
                                        .chars()
                                        .take(4)
                                        .collect();
                                    let _ = hex;
                                }
                                other => out.push(other),
                            }
                        }
                    }
                    '"' => {
                        if !out.is_empty() {
                            return Some(out);
                        }
                    }
                    other => out.push(other),
                }
            }
        }
        cursor = start;
    }
    None
}

#[derive(Clone, Debug)]
pub struct Bootstrap {
    pub gateway_url: String,
    pub vm_name: String,
    pub admission: String,
    pub notary: Option<String>,
    pub locale: String,
}

impl Bootstrap {
    pub fn vm_id(&self) -> String {
        if !self.vm_name.is_empty() {
            return self.vm_name.clone();
        }
        reqwest::Url::parse(&self.gateway_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .map(|host| host.split('.').next().unwrap_or_default().to_string())
            .unwrap_or_default()
    }

    /// Build the WebSocket admission URL. The bootstrap hands back a per-VM
    /// gateway host (`<vm-id>.metaaivm.com`); shared ingress hosts are accepted
    /// as-is. Both are valid origins for `/v1/noise`.
    pub fn websocket_candidates(&self) -> Vec<String> {
        let host = reqwest::Url::parse(&self.gateway_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .or_else(|| {
                self.gateway_url
                    .split("://")
                    .nth(1)
                    .map(|rest| rest.split('/').next().unwrap_or(rest).to_string())
            })
            .filter(|host| !host.is_empty());

        let mut hosts: Vec<String> = Vec::new();
        if let Some(host) = host {
            hosts.push(host);
        }
        for shared in SHARED_INGRESS_HOSTS {
            if !hosts.iter().any(|existing| existing == shared) {
                hosts.push(shared.to_string());
            }
        }
        hosts
            .into_iter()
            .map(|host| {
                let mut url = reqwest::Url::parse(&format!("wss://{host}/v1/noise"))
                    .expect("static ingress url");
                {
                    let mut query = url.query_pairs_mut();
                    query.append_pair("vm_id", &self.vm_id());
                    query.append_pair("auth_token", &self.admission);
                    query.append_pair("app_id", "hatch-web");
                    query.append_pair("request_id", &uuid::Uuid::new_v4().to_string());
                    if let Some(notary) = &self.notary {
                        query.append_pair("notary_token", notary);
                    }
                }
                url.to_string()
            })
            .collect()
    }
}
