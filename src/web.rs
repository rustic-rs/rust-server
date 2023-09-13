// mod web
//
// implements a REST server as specified by
// https://restic.readthedocs.io/en/stable/REST_backend.html?highlight=Rest%20API
//
// uses the modules
// storage - to access the file system
// auth    - for user authentication
// acl     - for access control
use http_range::HttpRange;
use std::collections::HashMap;
use std::{
    convert::TryInto, error::Error, future::Future, marker::Unpin, path::Path as StdPath, sync::Arc,
};

use axum::{
    body::Body,
    extract::{FromRequest, FromRequestParts, Host, Path as PathExtract, Query, State},
    handler::HandlerWithoutStateExt,
    http::{request::Parts, Request, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    BoxError, RequestExt, Router,
};
use axum_auth::AuthBasic;
use axum_server::{accept::Accept, tls_rustls::RustlsConfig};

use serde_derive::{Deserialize, Serialize};
use std::{io, net::SocketAddr, path::PathBuf};

use crate::{
    acl::{AccessType, Acl, AclChecker},
    auth::{Auth, AuthChecker},
    error::StatusError,
    error::{Result, StatusResult},
    helpers::IteratorAdapter,
    storage::{LocalStorage, Storage},
};

#[derive(Clone)]
struct TpeState(pub String);

#[derive(Clone, Copy)]
pub struct Ports {
    http: u16,
    https: u16,
}

#[derive(Clone)]
pub struct AppState {
    auth: Arc<dyn AuthChecker>,
    acl: Arc<dyn AclChecker>,
    storage: Arc<dyn Storage>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            auth: Arc::new(Auth::default()),
            acl: Arc::new(Acl::default()),
            storage: Arc::new(LocalStorage::default()),
        }
    }
}

// TODO!
// #[async_trait::async_trait]
// impl tide_http_auth::Storage<String, BasicAuthRequest> for State {
//     async fn get_user(&self, request: BasicAuthRequest) -> Result<Option<String>> {
//         let user = request.username;
//         match self.auth.verify(&user, &request.password) {
//             true => Ok(Some(user)),
//             false => Ok(None),
//         }
//     }
// }

impl AppState {
    pub fn new(auth: impl AuthChecker, acl: impl AclChecker, storage: impl Storage) -> Self {
        Self {
            storage: Arc::new(storage),
            auth: Arc::new(auth),
            acl: Arc::new(acl),
        }
    }
}

const TYPES: [&str; 5] = ["data", "keys", "locks", "snapshots", "index"];
const DEFAULT_PATH: &str = "";
const CONFIG_TYPE: &str = "config";
const CONFIG_NAME: &str = "";

fn check_string_sha256(name: &str) -> bool {
    if name.len() != 64 {
        return false;
    }
    for c in name.chars() {
        if !c.is_ascii_digit() && !('a'..='f').contains(&c) {
            return false;
        }
    }
    true
}

fn check_name(tpe: &str, name: &str) -> StatusResult<()> {
    match tpe {
        "config" => Ok(()),
        _ if check_string_sha256(name) => Ok(()),
        _ => Err(StatusError {
            status: StatusCode::FORBIDDEN,
            message: format!("filename {} not allowed", name).into(),
        }),
    }
}

fn check_auth_and_acl(
    state: &AppState,
    path: &StdPath,
    tpe: &str,
    append: AccessType,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // don't allow paths that includes any of the defined types
    for part in path.iter() {
        if let Some(part) = part.to_str() {
            for tpe in TYPES.iter() {
                if &part == tpe {
                    return Err((
                        StatusCode::FORBIDDEN,
                        format!("path {} not allowed", path.display()),
                    ));
                }
            }
        }
    }

    let empty = String::new();
    let user: &str = state.ext::<String>().unwrap_or(&empty);
    let Some(path) = path.to_str() else {
        return Err((
            StatusCode::FORBIDDEN,
            format!("path {} is non-unicode", path.display()),
        ));
    };
    let allowed = state.acl.allowed(user, path, tpe, append);
    tracing::debug!("[auth] user: {user}, path: {path}, tpe: {tpe}, allowed: {allowed}");

    match allowed {
        true => Ok(StatusCode::OK),
        false => Err((StatusCode::FORBIDDEN, format!("path {path} not allowed"))),
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Create {
    create: bool,
}

async fn create_dirs(
    Query(params): Query<Create>,
    State(state): State<AppState>,
    path: Option<PathExtract<String>>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let path = if let Some(PathExtract(path_ext)) = path {
        StdPath::new(&path_ext)
    } else {
        StdPath::new(DEFAULT_PATH)
    };

    tracing::debug!("[create_dirs] path: {path:?}");

    check_auth_and_acl(&state, path, "", AccessType::Append)?;
    let c: Create = params;
    match c.create {
        true => {
            for tpe in TYPES.iter() {
                match state.storage.create_dir(path, tpe) {
                    Ok(_) => (),
                    Err(e) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("error creating dir: {:?}", e),
                        )
                    }
                };
            }

            return (
                StatusCode::OK,
                format!("Called create_files with path {:?}\n", path),
            );
        }
        false => {
            return (
                StatusCode::OK,
                format!("Called create_files with path {:?}, create=false\n", path),
            )
        }
    }
}

const API_V1: &str = "application/vnd.x.restic.rest.v1";
const API_V2: &str = "application/vnd.x.restic.rest.v2";

#[derive(Serialize)]
struct RepoPathEntry {
    name: String,
    size: u64,
}
// (DEFAULT_PATH, tpe, &req)
async fn list_files(
    PathExtract(path): PathExtract<String>,
    State(tpe_state): State<TpeState>,
    State(state): State<AppState>,
    req: Request<AppState>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let tpe = &tpe_state.0;
    tracing::debug!("[list_files] path: {path}, tpe: {tpe}");

    let path = StdPath::new(&path);
    check_auth_and_acl(&state, path, tpe, AccessType::Read)?;

    let read_dir = state.storage.read_dir(path, tpe);
    let mut res = Response::new(StatusCode::OK);

    // TODO: error handling
    match req.headers().get("Accept") {
        Some(a) if a.as_str() == API_V2 => {
            res.set_content_type(API_V2);
            let read_dir_version = read_dir.map(|e| RepoPathEntry {
                name: e.file_name().to_str().unwrap().to_string(),
                size: e.metadata().unwrap().len(),
            });
            res.set_body(Body::from_json(&IteratorAdapter::new(read_dir_version))?);
        }
        _ => {
            res.set_content_type(API_V1);
            let read_dir_version = read_dir.map(|e| e.file_name().to_str().unwrap().to_string());
            res.set_body(Body::from_json(&IteratorAdapter::new(read_dir_version))?);
        }
    };
    Ok(res)
}

async fn length(path: &str, tpe: &str, name: &str, req: &Request<AppState>) -> Result<()> {
    tracing::debug!("[length] path: {path}, tpe: {tpe}, name: {name}");

    check_name(tpe, name)?;
    let path = Path::new(path);
    check_auth_and_acl(req, path, tpe, AccessType::Read)?;

    let _file = req.state().storage.filename(path, tpe, name);
    Err(axum::Error::from_str(
        StatusCode::NotImplemented,
        "not yet implemented",
    ))
}

async fn get_file(path: &str, tpe: &str, name: &str, req: &Request<AppState>) -> Result<Response> {
    tracing::debug!("[get_file] path: {path}, tpe: {tpe}, name: {name}");

    check_name(tpe, name)?;
    let path = Path::new(path);
    check_auth_and_acl(req, path, tpe, AccessType::Read)?;

    let mut file = req.state().storage.open_file(path, tpe, name).await?;
    let mut len = file.metadata().await?.len();

    let mut res;
    match req.header("Range") {
        None => {
            res = Response::new(StatusCode::Ok);
        }
        Some(r) => match HttpRange::parse(r.as_str(), len) {
            Ok(range) if range.len() == 1 => {
                file.seek(Start(range[0].start)).await?;
                len = range[0].length;
                res = Response::new(StatusCode::PartialContent);
            }
            Ok(_) => {
                return Err(axum::Error::from_str(
                    StatusCode::NotImplemented,
                    "multipart range not implemented",
                ))
            }
            Err(_) => {
                return Err(axum::Error::from_str(
                    StatusCode::InternalServerError,
                    "range error",
                ))
            }
        },
    };

    let file = io::BufReader::new(file);
    res.set_body(Body::from_reader(file, Some(len.try_into()?)));
    Ok(res)
}

#[async_trait::async_trait]
pub trait Finalizer {
    async fn finalize(&mut self) -> Result<()>;
}

async fn save_body(
    req: &mut Request<AppState>,
    mut file: impl io::Write + Unpin + Finalizer,
) -> Result<Response> {
    let bytes_written = io::copy(req, &mut file).await?;
    tracing::debug!("[file written] bytes: {bytes_written}");
    file.finalize().await?;
    Ok(Response::new(StatusCode::Ok))
}

async fn get_save_file(
    path: &str,
    tpe: &str,
    name: &str,
    req: &Request<AppState>,
) -> Result<impl io::Write + Unpin + Finalizer> {
    tracing::debug!("[get_save_file] path: {path}, tpe: {tpe}, name: {name}");

    check_name(tpe, name)?;
    let path = Path::new(path);
    check_auth_and_acl(req, path, tpe, AccessType::Append)?;

    Ok(req.state().storage.create_file(path, tpe, name).await?)
}

async fn delete_file(
    path: &str,
    tpe: &str,
    name: &str,
    req: &Request<AppState>,
) -> Result<Response> {
    check_name(tpe, name)?;
    let path = Path::new(path);
    check_auth_and_acl(req, path, tpe, AccessType::Modify)?;
    req.state().storage.remove_file(path, tpe, name)?;
    Ok(Response::new(StatusCode::Ok))
}

async fn auth_handler(AuthBasic((id, password)): AuthBasic) -> Result<String> {
    tracing::debug!("[auth_handler] id: {id}, password: {password}");
    match id.as_str() {
        "user" if password == "password" => Ok(id),
        _ => Err(axum::Error::from_str(StatusCode::Forbidden, "not allowed")),
    }
}

// TODO!: https://github.com/tokio-rs/axum/blob/main/examples/tls-rustls/src/main.rs
// TODO!: https://github.com/tokio-rs/axum/blob/main/examples/readme/src/main.rs
pub async fn main(
    state: AppState,
    addr: String,
    ports: Ports,
    tls: bool,
    cert: Option<String>,
    key: Option<String>,
) -> StatusResult<()> {
    // let mid = tide_http_auth::Authentication::new(BasicAuthScheme);
    // let mut app = tide::with_state(state);
    // app.with(mid);

    let mut app = Router::new().with_state(state);

    app.route("/", post(create_dirs));
    app.route("/:path/", post(create_dirs));

    for tpe in TYPES.into_iter() {
        let path = &("/".to_string() + tpe + "/");
        tracing::debug!("add path: {path}");
        let tpe_state = TpeState(tpe.into());

        app.route(path, get(list_files).with_state(tpe));

        let path = &("/".to_string() + tpe + "/:name");
        tracing::debug!("add path: {path}");
        app.route(path)
            .head(move |req: Request<AppState>| async move {
                length(DEFAULT_PATH, tpe, req.param("name")?, &req).await
            })
            .get(move |req: Request<AppState>| async move {
                get_file(DEFAULT_PATH, tpe, req.param("name")?, &req).await
            })
            .post(move |mut req: Request<AppState>| async move {
                let file = get_save_file(DEFAULT_PATH, tpe, req.param("name")?, &req).await?;
                save_body(&mut req, file).await
            })
            .delete(move |req: Request<AppState>| async move {
                delete_file(DEFAULT_PATH, tpe, req.param("name")?, &req).await
            });

        let path = &("/:path/".to_string() + tpe + "/");
        tracing::debug!("add path: {path}");
        app.route(path)
            .get(move |req: Request<AppState>| async move {
                list_files(req.param("path")?, tpe, &req).await
            });

        let path = &("/:path/".to_string() + tpe + "/:name");
        tracing::debug!("add path: {path}");
        app.route(path)
            .head(move |req: Request<AppState>| async move {
                length(req.param("path")?, tpe, req.param("name")?, &req).await
            })
            .get(move |req: Request<AppState>| async move {
                get_file(req.param("path")?, tpe, req.param("name")?, &req).await
            })
            .post(move |mut req: Request<AppState>| async move {
                let file = get_save_file(req.param("path")?, tpe, req.param("name")?, &req).await?;
                save_body(&mut req, file).await
            })
            .delete(move |req: Request<AppState>| async move {
                delete_file(req.param("path")?, tpe, req.param("name")?, &req).await
            });
    }

    app.route("config")
        .get(|req| async move { get_file(DEFAULT_PATH, CONFIG_TYPE, CONFIG_NAME, &req).await })
        .post(|mut req| async move {
            let file = get_save_file(DEFAULT_PATH, CONFIG_TYPE, CONFIG_NAME, &req).await?;
            save_body(&mut req, file).await
        })
        .delete(
            |req| async move { delete_file(DEFAULT_PATH, CONFIG_TYPE, CONFIG_NAME, &req).await },
        );

    app.route("/:path/config")
        .get(|req: Request<AppState>| async move {
            get_file(req.param("path")?, CONFIG_TYPE, CONFIG_NAME, &req).await
        })
        .post(|mut req: Request<AppState>| async move {
            let file = get_save_file(req.param("path")?, CONFIG_TYPE, CONFIG_NAME, &req).await?;
            save_body(&mut req, file).await
        })
        .delete(|req: Request<AppState>| async move {
            delete_file(req.param("path")?, CONFIG_TYPE, CONFIG_NAME, &req).await
        });

    // configure certificate and private key used by https
    let config = match tls {
        true => {
            Some(
                RustlsConfig::from_pem_file(
                    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("self_signed_certs")
                        .join("cert.pem"),
                    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("self_signed_certs")
                        .join("key.pem"),
                )
                .await
                .unwrap(),
            );
        }
        false => None,
    };

    // match tls {
    //     false => app.listen(addr).await?,
    //     true => {
    //         app.listen(
    //             TlsListener::build()
    //                 .addrs(addr)
    //                 .cert(cert.expect("--cert not given"))
    //                 .key(key.expect("--key not given")),
    //         )
    //         .await?
    //     }
    // };

    // run https server
    let addr = SocketAddr::from(([127, 0, 0, 1], ports.https));
    tracing::debug!("listening on {}", addr);
    axum_server::bind_rustls(addr, config)
        .serve(app.into_make_service())
        .await
        .unwrap();
    Ok(())
}
