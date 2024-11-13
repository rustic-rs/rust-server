use std::{collections::HashMap, fs, io, path::PathBuf};

use axum::{extract::FromRequestParts, http::request::Parts};
use axum_auth::AuthBasic;
use serde_derive::Deserialize;
use std::sync::OnceLock;

use crate::{
    config::HtpasswdSettings,
    error::{ApiErrorKind, ApiResult, AppResult},
};

//Static storage of our credentials
pub static AUTH: OnceLock<Auth> = OnceLock::new();

pub(crate) fn init_auth(auth: Auth) -> AppResult<()> {
    let _ = AUTH.get_or_init(|| auth);
    Ok(())
}

pub trait AuthChecker: Send + Sync + 'static {
    fn verify(&self, user: &str, passwd: &str) -> bool;
}

/// read_htpasswd is a helper func that reads the given file in .httpasswd format
/// into a Hashmap mapping each user to the whole passwd line
fn read_htpasswd(file_path: &PathBuf) -> AppResult<HashMap<&'static str, &'static str>> {
    let s = fs::read_to_string(file_path)?;
    // make the contents static in memory
    let s = Box::leak(s.into_boxed_str());

    let mut user_map = HashMap::new();
    for line in s.lines() {
        let user = line.split(':').collect::<Vec<&str>>()[0];
        user_map.insert(user, line);
    }
    Ok(user_map)
}

#[derive(Debug, Default, Clone)]
pub struct Auth {
    users: Option<HashMap<&'static str, &'static str>>,
}

impl Auth {
    pub fn from_file(disable_auth: bool, path: &PathBuf) -> AppResult<Self> {
        Ok(Self {
            users: if disable_auth {
                None
            } else {
                Some(read_htpasswd(path)?)
            },
        })
    }

    pub fn from_config(settings: &HtpasswdSettings) -> AppResult<Self> {
        let path = settings.htpasswd_file_or_default(&PathBuf::new());
        Self::from_file(settings.is_disabled(), &path)
    }
}

impl AuthChecker for Auth {
    // verify verifies user/passwd against the credentials saved in users.
    // returns true if Auth::users is None.
    fn verify(&self, user: &str, passwd: &str) -> bool {
        match &self.users {
            Some(users) => {
                matches!(users.get(user), Some(passwd_data) if htpasswd_verify::Htpasswd::from(*passwd_data).check(user, passwd))
            }
            None => true,
        }
    }
}

#[derive(Deserialize)]
pub struct AuthFromRequest {
    pub(crate) user: String,
    pub(crate) _password: String,
}

#[async_trait::async_trait]
impl<S: Send + Sync> FromRequestParts<S> for AuthFromRequest {
    type Rejection = ApiErrorKind;

    // FIXME: We also have a configuration flag do run without authentication
    // This must be handled here too ... otherwise we get an Auth header missing error.
    async fn from_request_parts(parts: &mut Parts, state: &S) -> ApiResult<Self> {
        let checker = AUTH.get().unwrap();

        let auth_result = AuthBasic::from_request_parts(parts, state).await;

        tracing::debug!("Got authentication result: {auth_result:?}");

        return match auth_result {
            Ok(auth) => {
                let AuthBasic((user, passw)) = auth;
                let password = passw.unwrap_or_else(|| "".to_string());
                if checker.verify(user.as_str(), password.as_str()) {
                    Ok(Self {
                        user,
                        _password: password,
                    })
                } else {
                    Err(ApiErrorKind::UserAuthenticationError(user))
                }
            }
            Err(_) => {
                let user = "".to_string();
                if checker.verify("", "") {
                    return Ok(Self {
                        user,
                        _password: "".to_string(),
                    });
                }
                Err(ApiErrorKind::AuthenticationHeaderError)
            }
        };
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test_helpers::{basic_auth_header_value, init_test_environment};
    use anyhow::Result;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use http_body_util::BodyExt;
    use std::env;
    use tower::ServiceExt;

    #[test]
    fn test_auth_passes() -> Result<()> {
        let cwd = env::current_dir()?;
        let htpasswd = PathBuf::new()
            .join(cwd)
            .join("tests")
            .join("fixtures")
            .join("test_data")
            .join(".htpasswd");
        let auth = Auth::from_file(false, &htpasswd)?;
        assert!(auth.verify("test", "test_pw"));
        assert!(!auth.verify("test", "__test_pw"));

        Ok(())
    }

    #[test]
    fn test_auth_from_file_passes() {
        let cwd = env::current_dir().unwrap();
        let htpasswd = PathBuf::new()
            .join(cwd)
            .join("tests")
            .join("fixtures")
            .join("test_data")
            .join(".htpasswd");

        dbg!(&htpasswd);

        let auth = Auth::from_file(false, &htpasswd).unwrap();
        init_auth(auth).unwrap();

        let auth = AUTH.get().unwrap();
        assert!(auth.verify("test", "test_pw"));
        assert!(!auth.verify("test", "__test_pw"));
    }

    async fn format_auth_basic(AuthBasic((id, password)): AuthBasic) -> String {
        format!("Got {} and {:?}", id, password)
    }

    async fn format_handler_from_auth_request(auth: AuthFromRequest) -> String {
        format!("User = {}", auth.user)
    }

    /// The requests which should be returned OK
    #[tokio::test]
    async fn test_authentication_passes() {
        init_test_environment();

        // -----------------------------------------
        // Try good basic
        // -----------------------------------------
        let app = Router::new().route("/basic", get(format_auth_basic));

        let request = Request::builder()
            .uri("/basic")
            .method(Method::GET)
            .header(
                "Authorization",
                basic_auth_header_value("My Username", Some("My Password")),
            )
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(request).await.unwrap();

        assert_eq!(resp.status().as_u16(), StatusCode::OK.as_u16());
        let body = resp.into_parts().1;
        let byte_vec = body.into_data_stream().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(byte_vec.to_vec()).unwrap();
        assert_eq!(
            body_str,
            String::from("Got My Username and Some(\"My Password\")")
        );

        // -----------------------------------------
        // Try good using auth struct
        // -----------------------------------------
        let app = Router::new().route("/rustic_server", get(format_handler_from_auth_request));

        let request = Request::builder()
            .uri("/rustic_server")
            .method(Method::GET)
            .header(
                "Authorization",
                basic_auth_header_value("test", Some("test_pw")),
            )
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(request).await.unwrap();

        assert_eq!(resp.status().as_u16(), StatusCode::OK.as_u16());
        let body = resp.into_parts().1;
        let byte_vec = body.collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(byte_vec.to_vec()).unwrap();
        assert_eq!(body_str, String::from("User = test"));
    }

    #[tokio::test]
    async fn test_fail_authentication_passes() {
        init_test_environment();

        // -----------------------------------------
        // Try wrong password rustic_server
        // -----------------------------------------
        let app = Router::new().route("/rustic_server", get(format_handler_from_auth_request));

        let request = Request::builder()
            .uri("/rustic_server")
            .method(Method::GET)
            .header(
                "Authorization",
                basic_auth_header_value("test", Some("__test_pw")),
            )
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(request).await.unwrap();

        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // -----------------------------------------
        // Try without authentication header
        // -----------------------------------------
        let app = Router::new().route("/rustic_server", get(format_handler_from_auth_request));

        let request = Request::builder()
            .uri("/rustic_server")
            .method(Method::GET)
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(request).await.unwrap();

        assert_eq!(resp.status().as_u16(), StatusCode::FORBIDDEN);
    }
}
