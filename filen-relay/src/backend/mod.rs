// todo: better technical documentation

use std::str::FromStr;

use axum_core::body::Body;
use axum_core::response::IntoResponse;
use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use dioxus::fullstack::Redirect;
use dioxus::logger::tracing;
use dioxus::prelude::*;
use dioxus::server::axum;
use dioxus::server::axum::middleware::Next;
use dioxus::server::axum::{
    extract::{Path, Request},
    response::Response,
};
use http::Uri;

use crate::api::SKIP_UPDATE_CHECKER;
use crate::backend::rclone_auth_proxy::handle_rclone_remote_config_request;
use crate::backend::server_manager::{ServerManager, RCLONE_BASE_URL_TO_SUBSTITUTE};
use crate::common::ServerType;
use crate::{
    backend::{
        auth::ADMIN_EMAIL,
        db::{DbViaOfflineOrRemoteFile, DB},
    },
    Args,
};

pub(crate) mod auth;
pub(crate) mod db;
pub(crate) mod rclone_auth_proxy;
pub(crate) mod server_manager;
pub(crate) mod util;

pub(crate) fn serve(args: Args) {
    dioxus::serve(move || {
        let args = args.clone();
        async move {
            let (admin_email, db) = match (
                    args.admin_email,
                    args.admin_password,
                    args.admin_2fa_code,
                    args.admin_auth_config,
                    args.db_dir,
                ) {
                    (Some(email), _, _, _, Some(db_dir)) => {
                        let db = DbViaOfflineOrRemoteFile::new_from_offline_location(Some(&db_dir)).await;
                        db.map(|db| (email, db))
                    }
                    (_, _, _, Some(auth_config), _) => {
                        DbViaOfflineOrRemoteFile::new_from_auth_config(auth_config).await
                    }
                    (Some(email), Some(password), two_fa_code, _, _) => {
                        let db = DbViaOfflineOrRemoteFile::new_from_email_and_password(
                            email.clone(),
                            &password,
                            two_fa_code.as_deref(),
                        )
                        .await;
                        db.map(|db| (email, db))
                    }
                    _ => panic!(
                        "Either admin email and local db dir, email/password or auth config must be provided"
                    ),
                }.expect("Failed to initialize database");
            ADMIN_EMAIL.set(admin_email).unwrap();
            DB.init(db);
            SKIP_UPDATE_CHECKER.set(args.skip_update_checker).unwrap();

            let self_port = std::env::var("PORT")
                .map(|port| port.parse::<u16>().unwrap_or(8080))
                .context("Failed to parse content of PORT env var")?;
            let server_manager = std::sync::Arc::new(
                ServerManager::start_servers(self_port)
                    .await
                    .context("Failed to start Rclone servers")
                    .unwrap(),
            );

            let (server_manager_1, server_manager_2) =
                (server_manager.clone(), server_manager.clone());
            let router = dioxus::server::router(crate::frontend::App);
            let router = auth::initialize_session_manager(router);
            Ok(
                router
                    .route(
                        "/{server_type}",
                        axum::routing::any(
                            |Path(_server_type): Path<ServerType>, _req: Request| async move {
                                Response::builder()
                                    .status(axum::http::StatusCode::NOT_FOUND)
                                    .body(Body::from("No share id provided"))
                                    .unwrap()
                                // WebDAV clients such as Windows Explorer might otherwise call routes such as /webdav, get assigned a session and be confused somehow
                            },
                        ),
                    )
                    .route(
                        "/{server_type}/{id}",
                        axum::routing::any(
                            |Path((_server_type, _id)): Path<(ServerType, String)>,
                             req: Request| async move {
                                Redirect::permanent(&format!("{}/", req.uri()))
                            },
                        ),
                    )
                    .route(
                        "/{server_type}/{id}/",
                        axum::routing::any(
                            move |Path((server_type, id)): Path<(ServerType, String)>,
                                  req: Request| async move {
                                handle_rclone_request(
                                    &self_port,
                                    server_manager_1,
                                    &server_type,
                                    id,
                                    req,
                                )
                                .await
                            },
                        ),
                    )
                    .route(
                        "/{server_type}/{id}/{*rest}",
                        axum::routing::any(
                            move |Path((server_type, id, _rest)): Path<(
                                ServerType,
                                String,
                                String,
                            )>,
                                  req: Request| async move {
                                handle_rclone_request(
                                    &self_port,
                                    server_manager_2,
                                    &server_type,
                                    id,
                                    req,
                                )
                                .await
                            },
                        ),
                    )
                    .route(
                        "/rclone-auth-proxy/remote-config/{share_id}",
                        axum::routing::get(
                            |Path(share_id): Path<String>, req: Request| async move {
                                handle_rclone_remote_config_request(share_id, req)
                            },
                        ),
                    )
                    .layer(axum::middleware::from_fn(
                        move |req: Request, next: Next| async move {
                            dioxus::logger::tracing::trace!(
                                "Received request: {} {}",
                                req.method(),
                                req.uri()
                            );
                            next.run(req).await
                        },
                    )),
            )
            // todo: add info somewhere that these routes exist
        }
    });
}

async fn handle_rclone_request(
    self_port: &u16,
    server_manager: std::sync::Arc<server_manager::ServerManager>,
    server_type: &ServerType,
    id: String,
    req: Request,
) -> Response {
    handle_rclone_request_(self_port, server_manager, server_type, id, req)
        .await
        .unwrap_or_else(|e| {
            Response::builder()
                .status(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from(format!("Failed to process request: {}", e)))
                .unwrap()
        })
}

async fn handle_rclone_request_(
    self_port: &u16,
    server_manager: std::sync::Arc<server_manager::ServerManager>,
    server_type: &ServerType,
    id: String,
    mut req: Request,
) -> Result<Response, anyhow::Error> {
    let shares = DB
        .get_shares()
        .map_err(|e| anyhow::anyhow!("Failed to get shares from database: {}", e))?;
    if let Some(share) = shares.into_iter().find(|s| s.id.short() == id) {
        // check password
        if let Some(password) = &share.password {
            let auth_header = req
                .headers()
                .get("authorization")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("");
            let auth_header = auth_header.strip_prefix("Basic ").unwrap_or("");
            let auth_header = BASE64_STANDARD.decode(auth_header).unwrap_or_default();
            let auth_header = String::from_utf8_lossy(&auth_header);
            if !auth_header.ends_with(password) {
                return Ok(Response::builder()
                    .status(axum::http::StatusCode::UNAUTHORIZED)
                    .header(
                        "WWW-Authenticate",
                        format!("Basic realm=\"share {}\"", share.id),
                    )
                    .body(Body::from(
                        "This share needs a password to access. No username is needed.",
                    ))
                    .unwrap());
            }
        }

        // insert share id for auth proxy
        req.headers_mut().insert(
            "Authorization",
            format!("Basic {}", BASE64_STANDARD.encode(format!("{}:", share.id)))
                .parse()
                .unwrap(),
        );

        // find proxy target
        let port = server_manager
            .get_port_for_forwarded_request(server_type)
            .await
            .context("Failed to find requested server type")?;
        let proxy =
            axum_reverse_proxy::ReverseProxy::new("", &format!("http://127.0.0.1:{}", port));

        // transform urls (request uri and several headers)
        // this converts between e.g. /webdav/fc127fba/directory/file.txt -> /RCLONE_BASE_URL_TO_SUBSTITUTE/directory/file.txt
        // and back in XML responses from WebDAV
        let (transform_uri_fordward, transform_uri_backward) = {
            let base_path = {
                let request_path = req.uri().to_string();
                request_path
                    .find(&id)
                    .map(|id_idx| &request_path[..(id_idx + id.len())])
                    .unwrap()
                    .to_string()
                // e.g. base_path = "/webdav/fc127fba"
            };
            let self_host = req
                .headers()
                .get("host")
                .and_then(|h| h.to_str().ok())
                .unwrap_or(&format!("127.0.0.1:{}", self_port))
                .to_string();
            let (base_path_1, self_host_1) = (base_path.clone(), self_host.clone());
            (
                move |uri: &str| -> String {
                    uri.replace(&base_path, RCLONE_BASE_URL_TO_SUBSTITUTE)
                        .replace(&self_host, &format!("127.0.0.1:{}", port))
                },
                move |uri: &str| -> String {
                    uri.replace(RCLONE_BASE_URL_TO_SUBSTITUTE, &base_path_1)
                        .replace(&format!("127.0.0.1:{}", port), &self_host_1)
                },
            )
        };

        // transform request url
        *req.uri_mut() =
            Uri::from_str(&transform_uri_fordward(&req.uri().to_string())).unwrap_or_default();

        // transform request url in "Host", "Destination" and "Referer" header (used in WebDAV protocol)
        if let Some(host) = req.headers().get("host") {
            let host = host
                .to_str()
                .context("Failed to parse Host header")?
                .to_string();
            req.headers_mut()
                .insert("host", transform_uri_fordward(&host).parse().unwrap());
        }
        if let Some(destination) = req.headers().get("destination") {
            let destination = destination
                .to_str()
                .context("Failed to parse Destination header")?
                .to_string();
            req.headers_mut().insert(
                "destination",
                transform_uri_fordward(&destination).parse().unwrap(),
            );
        }
        if let Some(referer) = req.headers().get("referer") {
            let referer = referer
                .to_str()
                .context("Failed to parse Referer header")?
                .to_string();
            req.headers_mut()
                .insert("referer", transform_uri_fordward(&referer).parse().unwrap());
        }

        match proxy.proxy_request(req).await {
            Ok(response) => {
                if server_type == &ServerType::Webdav
                    && response
                        .headers()
                        .get("content-type")
                        .and_then(|h| h.to_str().ok())
                        .is_some_and(|h| h.contains("xml"))
                {
                    tracing::info!("Transforming response body for WebDAV share with id {} to replace base url since content-type indicates it's an XML response", id);
                    // replace base url in response body if needed (e.g. for WebDAV directory listings)
                    let (mut parts, body) = response.into_parts();
                    let body = axum::body::to_bytes(body, usize::MAX)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to read response body for transformation (it might be too long; this might be a bug worth looking at): {}", e))?;
                    let body = String::from_utf8_lossy(&body).to_string();
                    let body = transform_uri_backward(&body);
                    tracing::info!(
                        "Original Content-Length: {:?}, new Content-Length: {}",
                        parts.headers.get("content-length"),
                        body.len()
                    );
                    parts.headers.insert("content-length", body.len().into());
                    Ok(Body::from(body).into_response())
                } else {
                    tracing::info!(
                        "Not transforming response body for share with id {} since it's not WebDAV",
                        id
                    );
                    Ok(response)
                }
            }
            Err(e) => Ok(Response::builder()
                .status(axum::http::StatusCode::BAD_GATEWAY)
                .body(Body::from(format!("Failed to proxy request: {}", e)))
                .unwrap()),
        }
    } else {
        Ok(Response::builder()
            .status(axum::http::StatusCode::NOT_FOUND)
            .body(Body::from(format!("No share found for id: {}", id)))
            .unwrap())
    }
}

#[get("/api/ready")]
pub(crate) async fn ready() -> Result<(), axum::http::StatusCode> {
    let ready = true; // todo: check if all servers are ready?
    if ready {
        Ok(())
    } else {
        Err(axum::http::StatusCode::SERVICE_UNAVAILABLE)
    }
}
