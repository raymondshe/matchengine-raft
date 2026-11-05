use actix_web::HttpRequest;
use actix_web::HttpResponse;
use actix_web::get;
use actix_web::post;
use actix_web::web::Data;
use actix_web::web::Json;
use actix_web::web::Query;
use serde::Deserialize;

use matchengine_raft::match_result::EventId;
use matchengine_raft::network::api::MAX_EVENTS_PAGE;
use matchengine_raft::order_book::SymbolConfig;

use crate::freeze::Product;
use crate::state::AmendTicket;
use crate::state::ApiError;
use crate::state::CancelTicket;
use crate::state::DemoApp;
use crate::state::PlaceTicket;

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = concat!(
    include_str!("../../../web/console/app.css"),
    "\n",
    include_str!("../web/app.css")
);

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
                   connect-src 'self'; img-src 'self' data:; base-uri 'none'; \
                   form-action 'self'; frame-ancestors 'none'";
const ACTION_HEADER: &str = "x-demo-action";

#[get("/")]
pub async fn redirect() -> HttpResponse {
    HttpResponse::Found()
        .insert_header(("Location", "/demo/"))
        .finish()
}

#[get("/demo")]
pub async fn redirect_demo() -> HttpResponse {
    HttpResponse::Found()
        .insert_header(("Location", "/demo/"))
        .finish()
}

#[get("/demo/")]
pub async fn index() -> HttpResponse {
    asset("text/html; charset=utf-8", INDEX_HTML)
}

#[get("/demo/app.js")]
pub async fn js() -> HttpResponse {
    asset("application/javascript; charset=utf-8", APP_JS)
}

#[get("/demo/app.css")]
pub async fn css() -> HttpResponse {
    asset("text/css; charset=utf-8", APP_CSS)
}

#[derive(Deserialize, Default)]
pub struct ProductQuery {
    pub product: Option<Product>,
}

impl ProductQuery {
    fn product(&self) -> Product {
        self.product.unwrap_or_default()
    }
}

#[derive(Deserialize, Default)]
pub struct StateQuery {
    pub product: Option<Product>,
    pub depth: Option<usize>,
}

#[get("/demo/api/backends")]
pub async fn backends(app: Data<DemoApp>) -> HttpResponse {
    HttpResponse::Ok().json(app.backends())
}

#[get("/demo/api/state")]
pub async fn state(app: Data<DemoApp>, query: Query<StateQuery>) -> HttpResponse {
    let product = query.product.unwrap_or_default();
    match app.state(product, query.depth.unwrap_or(20)).await {
        Ok(state) => HttpResponse::Ok().json(state),
        Err(error) => api_error(error),
    }
}

#[derive(Deserialize, Default)]
pub struct BookQuery {
    pub product: Option<Product>,
    pub limit: Option<usize>,
}

#[get("/demo/api/book")]
pub async fn book(app: Data<DemoApp>, query: Query<BookQuery>) -> HttpResponse {
    let limit = query.limit.unwrap_or(usize::MAX);
    if limit == 0 {
        return api_error(ApiError::bad_request("limit must be at least 1"));
    }
    match app.book(query.product.unwrap_or_default(), limit).await {
        Ok(book) => HttpResponse::Ok().json(book),
        Err(error) => api_error(error),
    }
}

#[derive(Deserialize)]
pub struct EventsQuery {
    pub product: Option<Product>,
    pub after_log_index: Option<u64>,
    pub after_ordinal: Option<u32>,
    pub limit: Option<usize>,
}

#[get("/demo/api/events")]
pub async fn events(app: Data<DemoApp>, query: Query<EventsQuery>) -> HttpResponse {
    let after = match (query.after_log_index, query.after_ordinal) {
        (None, None) => None,
        (Some(log_index), Some(ordinal)) => Some(EventId { log_index, ordinal }),
        _ => {
            return api_error(ApiError::bad_request(
                "after_log_index and after_ordinal must be provided together",
            ));
        }
    };
    let limit = query.limit.unwrap_or(crate::state::DEFAULT_EVENTS_LIMIT);
    if !(1..=MAX_EVENTS_PAGE).contains(&limit) {
        return api_error(ApiError::bad_request(format!(
            "limit must be 1..={MAX_EVENTS_PAGE}"
        )));
    }
    match app
        .events(query.product.unwrap_or_default(), after, limit)
        .await
    {
        Ok(page) => HttpResponse::Ok().json(page),
        Err(error) => api_error(error),
    }
}

#[post("/demo/api/orders")]
pub async fn place(
    app: Data<DemoApp>,
    request: HttpRequest,
    body: Json<PlaceTicket>,
) -> HttpResponse {
    if !action_headers_ok(&request) {
        return api_error(ApiError::bad_request(
            "missing X-Demo-Action header or cross-origin request",
        ));
    }
    match app.place(body.into_inner()).await {
        Ok(result) => HttpResponse::Ok().json(result),
        Err(error) => api_error(error),
    }
}

#[post("/demo/api/freeze")]
pub async fn freeze_preview(
    app: Data<DemoApp>,
    request: HttpRequest,
    body: Json<PlaceTicket>,
) -> HttpResponse {
    if !action_headers_ok(&request) {
        return api_error(ApiError::bad_request(
            "missing X-Demo-Action header or cross-origin request",
        ));
    }
    match app.preview_freeze(body.into_inner()).await {
        Ok(preview) => HttpResponse::Ok().json(preview),
        Err(error) => api_error(error),
    }
}

#[post("/demo/api/orders/cancel")]
pub async fn cancel(
    app: Data<DemoApp>,
    request: HttpRequest,
    body: Json<CancelTicket>,
) -> HttpResponse {
    if !action_headers_ok(&request) {
        return api_error(ApiError::bad_request(
            "missing X-Demo-Action header or cross-origin request",
        ));
    }
    match app.cancel(body.into_inner()).await {
        Ok(result) => HttpResponse::Ok().json(result),
        Err(error) => api_error(error),
    }
}

#[post("/demo/api/orders/amend")]
pub async fn amend(
    app: Data<DemoApp>,
    request: HttpRequest,
    body: Json<AmendTicket>,
) -> HttpResponse {
    if !action_headers_ok(&request) {
        return api_error(ApiError::bad_request(
            "missing X-Demo-Action header or cross-origin request",
        ));
    }
    match app.amend(body.into_inner()).await {
        Ok(result) => HttpResponse::Ok().json(result),
        Err(error) => api_error(error),
    }
}

#[post("/demo/api/symbol")]
pub async fn configure_symbol(
    app: Data<DemoApp>,
    request: HttpRequest,
    query: Query<ProductQuery>,
    body: Json<SymbolConfig>,
) -> HttpResponse {
    if !action_headers_ok(&request) {
        return api_error(ApiError::bad_request(
            "missing X-Demo-Action header or cross-origin request",
        ));
    }
    match app
        .configure_symbol(query.product(), body.into_inner())
        .await
    {
        Ok(result) => HttpResponse::Ok().json(result),
        Err(error) => api_error(error),
    }
}

fn action_headers_ok(request: &HttpRequest) -> bool {
    if request.headers().get(ACTION_HEADER).is_none() {
        return false;
    }
    if let Some(origin) = request.headers().get("origin") {
        let origin = origin.to_str().unwrap_or_default();
        let origin_host = origin
            .split_once("://")
            .map(|(_, rest)| rest.split('/').next().unwrap_or(rest))
            .unwrap_or(origin);
        let host = request.connection_info().host().to_string();
        if origin_host != host {
            return false;
        }
    }
    true
}

fn api_error(error: ApiError) -> HttpResponse {
    HttpResponse::build(error.status).json(serde_json::json!({
        "error": {
            "kind": error.kind,
            "message": error.message,
            "leader_id": error.leader_id,
            "leader_addr": error.leader_addr,
        }
    }))
}

fn asset(content_type: &'static str, body: &'static str) -> HttpResponse {
    HttpResponse::Ok()
        .content_type(content_type)
        .insert_header(("Content-Security-Policy", CSP))
        .insert_header(("X-Content-Type-Options", "nosniff"))
        .insert_header(("Referrer-Policy", "no-referrer"))
        .insert_header(("Cache-Control", "no-cache"))
        .body(body)
}
