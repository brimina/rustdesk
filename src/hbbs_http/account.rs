use super::HbbHttpResponse;
use hbb_common::{
    config::{Config, LocalConfig},
    log, ResultType,
};
use reqwest::blocking::Client;
use serde::ser::SerializeStruct;
use serde_derive::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use url::Url;

lazy_static::lazy_static! {
    static ref API_SERVER: String = crate::get_api_server(
        Config::get_option("api-server"), Config::get_option("custom-rendezvous-server"));
    static ref OIDC_SESSION: Arc<RwLock<OidcSession>> = Arc::new(RwLock::new(OidcSession::new()));
}

const QUERY_INTERVAL_SECS: f32 = 1.0;
const QUERY_TIMEOUT_SECS: u64 = 60 * 3;
const REQUESTING_ACCOUNT_AUTH: &str = "Requesting account auth";
const WAITING_ACCOUNT_AUTH: &str = "Waiting account auth";
const LOGIN_ACCOUNT_AUTH: &str = "Login account auth";

#[derive(Deserialize, Clone, Debug)]
pub struct OidcAuthUrl {
    code: String,
    url: Url,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct DeviceInfo {
    /// Linux , Windows , Android ...
    #[serde(default)]
    pub os: String,

    /// `browser` or `client`
    #[serde(default)]
    pub r#type: String,

    /// device name from rustdesk client,
    /// browser info(name + version) from browser
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WhitelistItem {
    data: String, // ip / device uuid
    info: DeviceInfo,
    exp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserInfo {
    #[serde(default)]
    pub settings: UserSettings,
    #[serde(default)]
    pub login_device_whitelist: Vec<WhitelistItem>,
    #[serde(default)]
    pub other: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserSettings {
    #[serde(default)]
    pub email_verification: bool,
    #[serde(default)]
    pub email_alarm_notification: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize_repr, Deserialize_repr)]
#[repr(i64)]
pub enum UserStatus {
    Disabled = 0,
    Normal = 1,
    Unverified = -1,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserPayload {
    pub name: String,
    pub email: Option<String>,
    pub note: Option<String>,
    pub status: UserStatus,
    pub info: UserInfo,
    pub is_admin: bool,
    pub third_auth_type: Option<String>,
    // helper field for serialize
    #[serde(default)]
    pub ser_store_local: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthBody {
    pub access_token: String,
    pub r#type: String,
    pub user: UserPayload,
}

pub struct OidcSession {
    client: Client,
    state_msg: &'static str,
    failed_msg: String,
    code_url: Option<OidcAuthUrl>,
    auth_body: Option<AuthBody>,
    keep_querying: bool,
    running: bool,
    query_timeout: Duration,
}

#[derive(Serialize)]
pub struct AuthResult {
    pub state_msg: String,
    pub failed_msg: String,
    pub url: Option<String>,
    pub auth_body: Option<AuthBody>,
}

impl serde::Serialize for UserPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if self.ser_store_local {
            let mut state = serializer.serialize_struct("UserPayload", 1)?;
            state.serialize_field("name", &self.name)?;
            state.serialize_field("status", &self.status)?;
            state.end()
        } else {
            let mut state = serializer.serialize_struct("UserPayload", 7)?;
            state.serialize_field("name", &self.name)?;
            state.serialize_field("email", &self.email)?;
            state.serialize_field("note", &self.note)?;
            state.serialize_field("status", &self.status)?;
            state.serialize_field("info", &self.info)?;
            state.serialize_field("is_admin", &self.is_admin)?;
            state.serialize_field("third_auth_type", &self.third_auth_type)?;
            state.end()
        }
    }
}

impl OidcSession {
    fn new() -> Self {
        Self {
            client: Client::new(),
            state_msg: REQUESTING_ACCOUNT_AUTH,
            failed_msg: "".to_owned(),
            code_url: None,
            auth_body: None,
            keep_querying: false,
            running: false,
            query_timeout: Duration::from_secs(QUERY_TIMEOUT_SECS),
        }
    }

    fn auth(op: &str, id: &str, uuid: &str) -> ResultType<HbbHttpResponse<OidcAuthUrl>> {
        Ok(OIDC_SESSION
            .read()
            .unwrap()
            .client
            .post(format!("{}/api/oidc/auth", *API_SERVER))
            .json(&HashMap::from([("op", op), ("id", id), ("uuid", uuid)]))
            .send()?
            .try_into()?)
    }

    fn query(code: &str, id: &str, uuid: &str) -> ResultType<HbbHttpResponse<AuthBody>> {
        let url = reqwest::Url::parse_with_params(
            &format!("{}/api/oidc/auth-query", *API_SERVER),
            &[("code", code), ("id", id), ("uuid", uuid)],
        )?;
        Ok(OIDC_SESSION
            .read()
            .unwrap()
            .client
            .get(url)
            .send()?
            .try_into()?)
    }

    fn reset(&mut self) {
        self.state_msg = REQUESTING_ACCOUNT_AUTH;
        self.failed_msg = "".to_owned();
        self.keep_querying = true;
        self.running = false;
        self.code_url = None;
        self.auth_body = None;
    }

    fn before_task(&mut self) {
        self.reset();
        self.running = true;
    }

    fn after_task(&mut self) {
        self.running = false;
    }

    fn sleep(secs: f32) {
        std::thread::sleep(std::time::Duration::from_secs_f32(secs));
    }

    fn auth_task(op: String, id: String, uuid: String, remember_me: bool) {
        let auth_request_res = Self::auth(&op, &id, &uuid);
        log::info!("Request oidc auth result: {:?}", &auth_request_res);
        let code_url = match auth_request_res {
            Ok(HbbHttpResponse::<_>::Data(code_url)) => code_url,
            Ok(HbbHttpResponse::<_>::Error(err)) => {
                OIDC_SESSION
                    .write()
                    .unwrap()
                    .set_state(REQUESTING_ACCOUNT_AUTH, err);
                return;
            }
            Ok(_) => {
                OIDC_SESSION
                    .write()
                    .unwrap()
                    .set_state(REQUESTING_ACCOUNT_AUTH, "Invalid auth response".to_owned());
                return;
            }
            Err(err) => {
                OIDC_SESSION
                    .write()
                    .unwrap()
                    .set_state(REQUESTING_ACCOUNT_AUTH, err.to_string());
                return;
            }
        };

        OIDC_SESSION
            .write()
            .unwrap()
            .set_state(WAITING_ACCOUNT_AUTH, "".to_owned());
        OIDC_SESSION.write().unwrap().code_url = Some(code_url.clone());

        let begin = Instant::now();
        let query_timeout = OIDC_SESSION.read().unwrap().query_timeout;
        while OIDC_SESSION.read().unwrap().keep_querying && begin.elapsed() < query_timeout {
            match Self::query(&code_url.code, &id, &uuid) {
                Ok(HbbHttpResponse::<_>::Data(mut auth_body)) => {
                    if remember_me {
                        LocalConfig::set_option(
                            "access_token".to_owned(),
                            auth_body.access_token.clone(),
                        );
                        auth_body.user.ser_store_local = true;
                        LocalConfig::set_option(
                            "user_info".to_owned(),
                            serde_json::to_string(&auth_body.user).unwrap_or_default(),
                        );
                        auth_body.user.ser_store_local = false;
                    }
                    OIDC_SESSION
                        .write()
                        .unwrap()
                        .set_state(LOGIN_ACCOUNT_AUTH, "".to_owned());
                    OIDC_SESSION.write().unwrap().auth_body = Some(auth_body);
                    return;
                }
                Ok(HbbHttpResponse::<_>::Error(err)) => {
                    if err.contains("No authed oidc is found") {
                        // ignore, keep querying
                    } else {
                        OIDC_SESSION
                            .write()
                            .unwrap()
                            .set_state(WAITING_ACCOUNT_AUTH, err);
                        return;
                    }
                }
                Ok(_) => {
                    // ignore
                }
                Err(err) => {
                    log::trace!("Failed query oidc {}", err);
                    // ignore
                }
            }
            Self::sleep(QUERY_INTERVAL_SECS);
        }

        if begin.elapsed() >= query_timeout {
            OIDC_SESSION
                .write()
                .unwrap()
                .set_state(WAITING_ACCOUNT_AUTH, "timeout".to_owned());
        }

        // no need to handle "keep_querying == false"
    }

    fn set_state(&mut self, state_msg: &'static str, failed_msg: String) {
        self.state_msg = state_msg;
        self.failed_msg = failed_msg;
    }

    fn wait_stop_querying() {
        let wait_secs = 0.3;
        while OIDC_SESSION.read().unwrap().running {
            Self::sleep(wait_secs);
        }
    }

    pub fn account_auth(op: String, id: String, uuid: String, remember_me: bool) {
        Self::auth_cancel();
        Self::wait_stop_querying();
        OIDC_SESSION.write().unwrap().before_task();
        std::thread::spawn(move || {
            Self::auth_task(op, id, uuid, remember_me);
            OIDC_SESSION.write().unwrap().after_task();
        });
    }

    fn get_result_(&self) -> AuthResult {
        AuthResult {
            state_msg: self.state_msg.to_string(),
            failed_msg: self.failed_msg.clone(),
            url: self.code_url.as_ref().map(|x| x.url.to_string()),
            auth_body: self.auth_body.clone(),
        }
    }

    pub fn auth_cancel() {
        OIDC_SESSION.write().unwrap().keep_querying = false;
    }

    pub fn get_result() -> AuthResult {
        OIDC_SESSION.read().unwrap().get_result_()
    }
}
