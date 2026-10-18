use anyhow::{Context, Error};
use serde::Deserialize;
use serde_json::{Value, json};
use std::env;
use std::io::Read;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::app_settings;
use crate::store::{StoreAdapter, StoreListing, WordpressStore};
mod jobs;
pub(super) use jobs::{configured_ftp, delivery_connection, progress, run_job};

const DEFAULT_BIND: &str = "127.0.0.1:36061";
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

#[derive(Deserialize)]
struct SearchRequest {
    query: String,
}

#[derive(Deserialize)]
struct InspectRequest {
    listing: StoreListing,
}

pub(super) fn run() -> i32 {
    match serve() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("Store worker failed: {error:#}");
            1
        }
    }
}

fn serve() -> Result<(), Error> {
    let token = env::var("ISO2GOD_WORKER_TOKEN")
        .context("ISO2GOD_WORKER_TOKEN must be set before starting the store worker")?;
    if token.len() < 32 {
        anyhow::bail!("ISO2GOD_WORKER_TOKEN must contain at least 32 characters");
    }
    let bind = env::var("ISO2GOD_WORKER_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_owned());
    let server = Server::http(&bind).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let runtime = jobs::Runtime::start()?;
    eprintln!("ISO2GOD store worker listening on {bind}");
    for request in server.incoming_requests() {
        handle(request, &token, &runtime);
    }
    Ok(())
}

fn handle(mut request: Request, token: &str, runtime: &jobs::Runtime) {
    if let Ok(allowed) = env::var("DIGITAL360_ALLOWED_XBOX") {
        let is_console = request
            .remote_addr()
            .is_some_and(|remote| remote.ip().to_string() == allowed);
        let is_host_check = request.method() == &Method::Get && request.url() == "/v1/host";
        if !is_console && !is_host_check {
            respond_json(
                request,
                StatusCode(403),
                json!({ "error": "not_paired_console" }),
            );
            return;
        }
    }
    if request.method() == &Method::Get && request.url() == "/health" {
        respond_json(request, StatusCode(200), json!({ "status": "ok" }));
        return;
    }
    if !is_authorized(&request, token) {
        respond_json(request, StatusCode(401), json!({ "error": "unauthorized" }));
        return;
    }
    if request.method() == &Method::Get && request.url() == "/v1/host" {
        respond_json(request, StatusCode(200), json!({ "status": "ready" }));
        return;
    }
    if request.method() == &Method::Post && request.url() == "/v1/console" {
        let result =
            read_json::<jobs::ConsoleRequest>(&mut request).and_then(|body| runtime.console(body));
        let (code, text) = match result {
            Ok(text) => (200, text),
            Err(error) => (400, format!("{}\n", friendly_error(&error))),
        };
        let mut response = Response::from_string(text).with_status_code(StatusCode(code));
        response
            .add_header(Header::from_bytes("Content-Type", "text/plain; charset=utf-8").unwrap());
        let _ = request.respond(response);
        return;
    }

    let result = match (request.method(), request.url()) {
        (&Method::Post, "/v1/store/search") => read_json::<SearchRequest>(&mut request)
            .and_then(|body| {
                let query = body.query.trim();
                if query.is_empty() {
                    anyhow::bail!("search text is required");
                }
                store()?.search(query)
            })
            .map(|listings| json!({ "listings": listings })),
        (&Method::Post, "/v1/store/inspect") => read_json::<InspectRequest>(&mut request)
            .and_then(|body| store()?.inspect(&body.listing))
            .map(|listing| json!({ "listing": listing })),
        _ => {
            respond_json(request, StatusCode(404), json!({ "error": "not_found" }));
            return;
        }
    };

    match result {
        Ok(value) => respond_json(request, StatusCode(200), value),
        Err(error) => respond_json(
            request,
            StatusCode(400),
            json!({ "error": friendly_error(&error) }),
        ),
    }
}

fn store() -> Result<WordpressStore, Error> {
    let settings = app_settings::load_app_settings()?;
    Ok(WordpressStore::new(settings.store_base_url).with_cache(app_settings::config_dir()?))
}

fn read_json<T: for<'de> Deserialize<'de>>(request: &mut Request) -> Result<T, Error> {
    let declared = request.body_length().unwrap_or(0) as u64;
    if declared > MAX_REQUEST_BYTES {
        anyhow::bail!("request is too large");
    }
    let mut bytes = Vec::new();
    request
        .as_reader()
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_REQUEST_BYTES {
        anyhow::bail!("request is too large");
    }
    serde_json::from_slice(&bytes).context("request contains invalid JSON")
}

fn is_authorized(request: &Request, token: &str) -> bool {
    request.headers().iter().any(|header| {
        header.field.equiv("Authorization")
            && header
                .value
                .as_str()
                .strip_prefix("Bearer ")
                .is_some_and(|candidate| candidate == token)
    })
}

fn respond_json(request: Request, status: StatusCode, value: Value) {
    let body =
        serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"error\":\"internal_error\"}".to_vec());
    let mut response = Response::from_data(body).with_status_code(status);
    if let Ok(header) = Header::from_bytes("Content-Type", "application/json; charset=utf-8") {
        response.add_header(header);
    }
    let _ = request.respond(response);
}

fn friendly_error(error: &Error) -> String {
    let text = error.to_string();
    if text.contains("Search returned no results") {
        "Search returned no results".to_owned()
    } else if text.contains("Store could not be reached") {
        "Store could not be reached".to_owned()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_is_loopback_only_by_default() {
        assert_eq!(DEFAULT_BIND, "127.0.0.1:36061");
    }

    #[test]
    fn worker_rejects_large_payloads() {
        assert_eq!(MAX_REQUEST_BYTES, 1024 * 1024);
    }
}
