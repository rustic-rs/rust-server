use axum::{
    extract::{Path as PathExtract},
    http::{header, StatusCode},
    response::{IntoResponse},
    Json,
};
use axum_macros::debug_handler;
use serde_derive::{Serialize, Deserialize};
use std::path::{Path};
use axum::http::header::AUTHORIZATION;
use axum_auth::AuthBasic;
use axum_extra::headers::HeaderMap;
use crate::{
    acl::{AccessType},
    error::{Result},
    handlers::file_helpers::IteratorAdapter
};
use crate::handlers::access_check::check_auth_and_acl;
use crate::handlers::path_analysis::{ArchivePathEnum, decompose_path};
use crate::storage::{STORAGE};
use crate::web::{API_V1, API_V2, DEFAULT_PATH};



//==============================================================================
// List files
// Interface: GET {path}/{type}/
//==============================================================================
#[derive(Serialize, Deserialize)]
struct RepoPathEntry {
    name: String,
    size: u64,
}

#[debug_handler]
async fn list_files(
    AuthBasic((user, _password)): AuthBasic,
    path: Option<PathExtract<String>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse> {

    let path_string = path.map_or(DEFAULT_PATH.to_string(), |PathExtract(path_ext)| path_ext);
    let archive_path = decompose_path(path_string)?;
    let p_str = archive_path.path;
    let tpe = archive_path.tpe;
    assert_ne!( archive_path.path_type, ArchivePathEnum::CONFIG);
    assert_eq!( archive_path.name, "".to_string());
    tracing::debug!("[list_files] path: {p_str}, tpe: {tpe}");


    let pth = Path::new(&p_str);
    check_auth_and_acl(user, tpe.as_str(), pth, AccessType::Read)?;

    let storage = STORAGE.get().unwrap();
    let read_dir = storage.read_dir(pth, tpe.as_str());

    let mut res = match headers.get(header::ACCEPT)
        .and_then(|header| header.to_str().ok())
    {
        Some(API_V2) => {
            let read_dir_version = read_dir.map(|e| {
                RepoPathEntry {
                    name: e.file_name().to_str().unwrap().to_string(),
                    size: e.metadata().unwrap().len(),
                    // FIXME:  return Err(ErrorKind::GettingFileMetadataFailed.into());
                }
            });
            let mut response = Json(&IteratorAdapter::new(read_dir_version)).into_response();
            tracing::debug!("[list_files::dir_content(V2)] {:?}",response.body());
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static(API_V2),
            );
            let status = response.status_mut();
            *status = StatusCode::OK;
            response
        },
        _ => {
            let read_dir_version = read_dir.map(|e| e.file_name().to_str().unwrap().to_string());
            let mut response = Json(&IteratorAdapter::new(read_dir_version)).into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static(API_V1),
            );
            let status = response.status_mut();
            *status = StatusCode::OK;
            response
        }
    };
    res.headers_mut().insert(AUTHORIZATION, headers.get(AUTHORIZATION).unwrap().clone());
    Ok(res)
}

#[cfg(test)]
mod test {
    use http_body_util::BodyExt;
    use axum::{ middleware, Router};
    use axum::routing::get;
    use crate::handlers::files_list::{list_files, RepoPathEntry};
    use crate::test_server::{basic_auth, init_test_environment, print_request_response};
    use crate::web::{API_V1, API_V2};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use axum::http::header::{ACCEPT, CONTENT_TYPE};
    use tower::{ServiceExt}; // for `call`, `oneshot`, and `ready`

    #[tokio::test]
    async fn get_list_files_test() {
        init_test_environment();

        // V1
        let app = Router::new()
            .route( "/*path",get(list_files) )
            .layer(middleware::from_fn(print_request_response));

        let request = Request::builder()
            .uri("/test_repo/keys")
            .header(ACCEPT, API_V1)
            .header("Authorization",  basic_auth("test", Some("test_pw")))
            .body(Body::empty()).unwrap();

        let resp = app
            .oneshot(request)
            .await
            .unwrap();

        assert_eq!( resp.status(), StatusCode::OK );

        assert_eq!( resp.headers().get(CONTENT_TYPE).unwrap().to_str().unwrap(), API_V1);
        let b = resp.into_body().collect().await.unwrap().to_bytes().to_vec();
        assert!(!b.is_empty());
        let body = std::str::from_utf8(&b).unwrap();
        let r : Vec<String> = serde_json::from_str(body).unwrap();
        let rr = r.first().unwrap();
        assert_eq!( rr, "2e734da3fccb98724ece44efca027652ba7a335c224448a68772b41c0d9229d5");

        // V2
        let app = Router::new()
            .route( "/*path",get(list_files) )
            .layer(middleware::from_fn(print_request_response));

        let requrest = Request::builder()
            .uri("/test_repo/keys")
            .header(ACCEPT, API_V2)
            .header("Authorization",  basic_auth("test", Some("test_pw")))
            .body(Body::empty()).unwrap();

        let resp = app
            .oneshot(requrest)
            .await
            .unwrap();

        assert_eq!( resp.status(), StatusCode::OK );

        assert_eq!( resp.headers().get(CONTENT_TYPE).unwrap().to_str().unwrap(), API_V2);
        let b = resp.into_body().collect().await.unwrap().to_bytes().to_vec();
        let body = std::str::from_utf8(&b).unwrap();
        let r : Vec<RepoPathEntry> = serde_json::from_str(body).unwrap();
        assert!(!r.is_empty());
        let rr = r.first().unwrap();
        assert_eq!( rr.name, "2e734da3fccb98724ece44efca027652ba7a335c224448a68772b41c0d9229d5");
        assert_eq!(rr.size, 363);
    }
}