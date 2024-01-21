use crate::acl::{init_acl, Acl};
use crate::auth::{init_auth, Auth};
use crate::storage::{init_storage, LocalStorage};
use axum::body::Body;
use axum::http::{HeaderValue, Method};
use once_cell::sync::OnceCell;
use std::env;
use std::path::PathBuf;
use std::sync::Mutex;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// For testing we do repeated requests, using a password that should
/// be recognized as OK for the repository we are trying to access.
pub fn request_uri_for_test(uri: &str, method: Method) -> axum::http::Request<Body> {
    axum::http::Request::builder()
        .uri(uri)
        .method(method)
        .header(
            "Authorization",
            basic_auth_header_value("test", Some("test_pw")),
        )
        .body(Body::empty())
        .unwrap()
}

pub fn test_init_static_htaccess() {
    let cwd = env::current_dir().unwrap();
    let htaccess = PathBuf::new().join(cwd).join("test_data").join("htaccess");

    let auth = Auth::from_file(false, &htaccess).unwrap();
    init_auth(auth).unwrap();
}

pub fn test_init_static_auth() {
    let cwd = env::current_dir().unwrap();
    let acl_path = PathBuf::new().join(cwd).join("test_data").join("acl.toml");

    let acl = Acl::from_file(false, true, Some(acl_path)).unwrap();
    init_acl(acl).unwrap();
}

pub fn test_init_static_storage() {
    let cwd = env::current_dir().unwrap();
    let repo_path = PathBuf::new()
        .join(cwd)
        .join("test_data")
        .join("test_repos");

    let local_storage = LocalStorage::try_new(&repo_path).unwrap();
    init_storage(local_storage).unwrap();
}

/// When we initialise the global tracing subscriber, this must only happen once.
/// During tests, each test will initialise, to make sure we have at least tracing once.
/// This means that the init() call must be robust for this.
/// Since we do not need this in production code, it is located in the test code.
static TRACER: OnceCell<Mutex<usize>> = OnceCell::new();
pub(crate) fn init_mutex() {
    TRACER.get_or_init(|| {
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "RUSTIC_SERVER_LOG_LEVEL=debug".into()),
            )
            .with(tracing_subscriber::fmt::layer())
            .init();
        Mutex::new(0)
    });
}

pub fn init_tracing() {
    init_mutex();
}

pub fn init_test_environment() {
    init_mutex();

    test_init_static_htaccess();
    test_init_static_auth();
    test_init_static_storage();
}

pub fn basic_auth_header_value<U, P>(username: U, password: Option<P>) -> HeaderValue
where
    U: std::fmt::Display,
    P: std::fmt::Display,
{
    use base64::prelude::BASE64_STANDARD;
    use base64::write::EncoderWriter;
    use std::io::Write;

    let mut buf = b"Basic ".to_vec();
    {
        let mut encoder = EncoderWriter::new(&mut buf, &BASE64_STANDARD);
        let _ = write!(encoder, "{}:", username);
        if let Some(password) = password {
            let _ = write!(encoder, "{}", password);
        }
    }
    let mut header = HeaderValue::from_bytes(&buf).expect("base64 is always valid HeaderValue");
    header.set_sensitive(true);
    header
}