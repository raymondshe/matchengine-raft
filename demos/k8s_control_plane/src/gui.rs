//! Embedded manager GUI. One HTML file, no build step, no external assets.

use axum::response::{Html, IntoResponse};

const INDEX: &str = include_str!("../static/index.html");

pub async fn index() -> impl IntoResponse {
    Html(INDEX)
}
