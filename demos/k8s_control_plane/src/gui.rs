//! Embedded manager GUI. One HTML file, no build step, no external assets.

use axum::response::{Html, IntoResponse};

const INDEX: &str = include_str!("../static/index.html");

pub async fn index() -> impl IntoResponse {
    Html(INDEX)
}

#[cfg(test)]
mod tests {
    use super::INDEX;

    #[test]
    fn lifecycle_guards_use_the_api_error_code() {
        assert!(INDEX.contains("error.code = body.error ||"));
        assert_eq!(
            INDEX.matches("error.code === \"restart_required\"").count(),
            2,
            "both stop and start must branch on the API error code"
        );
        assert!(
            !INDEX.contains("String(error.message).includes(\"restart_required\")"),
            "the human-readable message never contains the restart_required code"
        );
    }

    #[test]
    fn rollout_progress_is_visible_and_actions_are_blocked() {
        assert!(INDEX.contains("id=\"rolloutBanner\""));
        assert!(INDEX.contains("Operator rollout in progress"));
        assert!(INDEX.contains("stopping…"));
        assert!(INDEX.contains("Operator rollout in progress; wait for the shard to settle"));
        assert!(INDEX.contains("class=\"rollout-progress\""));
        assert!(INDEX.contains("pods updated"));
        assert!(INDEX.contains("progress.percent"));
    }

    #[test]
    fn live_refresh_updates_the_catalog_and_pauses_for_modals() {
        assert!(INDEX.contains("id=\"liveState\""));
        assert!(INDEX.contains("const LIVE_REFRESH_MS = 2000;"));
        assert!(INDEX.contains("setInterval(refreshLive, LIVE_REFRESH_MS)"));
        assert!(INDEX.contains("document.querySelector(\".modal.open\")"));
        assert!(INDEX.contains("document.visibilityState !== \"visible\""));
    }
}
