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
        assert!(INDEX.contains("id=\"tab-plan\""));
        assert!(INDEX.contains(">preview plan</button>"));
        assert!(INDEX.contains("Uses the selected synthetic observation"));
        assert!(INDEX.contains("advanced-note"));
        assert!(INDEX.contains("Apply simulated status"));
        assert!(INDEX.contains("does not change desired state"));
        assert!(INDEX.contains("id=\"releaseBanner\""));
        assert!(INDEX.contains("Release rollout blocked"));
        assert!(INDEX.contains("No pods were deleted"));
        assert!(INDEX.contains("supervisor image or resource change"));
        assert!(INDEX.contains("id=\"capacityBanner\""));
        assert!(INDEX.contains("Capacity preflight blocked"));
        assert!(INDEX.contains("Free the requested capacity"));
        assert!(INDEX.contains("id=\"upgradeBtn\""));
        assert!(INDEX.contains("Upgrade"));
        assert!(INDEX.contains("Write the target image and release"));
        assert!(INDEX.contains("upgradeShard"));
        assert!(INDEX.contains("function syncUpgradeTargets"));
        assert!(INDEX.contains("field.value === field.dataset.autoValue"));
        assert!(!INDEX.contains("$(\"targetImage\").value = detail.shard.spec.image;"));
    }

    #[test]
    fn upgrade_targets_survive_live_refresh() {
        assert!(INDEX.contains(
            "const shardChanged = $(\"targetImage\").dataset.shard !== detail.shard.name;"
        ));
        assert!(INDEX.contains("[\"targetImage\", image]"));
        assert!(INDEX.contains("[\"wireVersion\", wire]"));
        assert!(INDEX.contains("[\"snapshotVersion\", snapshot]"));
        assert!(INDEX.contains("[\"mixedSafe\", mixed]"));
    }

    #[test]
    fn upgrade_image_picker_lists_cluster_images_and_marks_running() {
        assert!(INDEX.contains("id=\"targetImageSelect\""));
        assert!(INDEX.contains("id=\"imageHint\""));
        assert!(INDEX.contains("/api/v1/images?shard="));
        assert!(INDEX.contains("const IMAGE_REFRESH_MS = 15000;"));
        assert!(INDEX.contains("function renderImageOptions"));
        assert!(INDEX.contains("function syncImageSelect"));
        assert!(INDEX.contains("image.runningHere"));
        assert!(INDEX.contains("parts.push(\"running here\")"));
        assert!(INDEX.contains("◆ configured"));
        assert!(INDEX.contains("not cached"));
        assert!(INDEX.contains("custom reference"));
        assert!(INDEX.contains("cluster inventory:"));
        assert!(!INDEX.contains("$(\"targetImage\").dataset.autoValue = value;"));
    }

    #[test]
    fn upgrade_and_preview_plans_are_separate_tabs() {
        assert!(INDEX.contains("data-tab=\"upgrade\" title="));
        assert!(INDEX.contains(">upgrade</button>"));
        assert!(INDEX.contains("data-tab=\"plan\" title="));
        assert!(INDEX.contains(">preview plan</button>"));
        assert!(INDEX.contains("id=\"tab-upgrade\""));
        assert!(INDEX.contains("id=\"tab-plan\""));
        assert!(INDEX.contains("if (tab.dataset.tab === \"plan\") loadPlan();"));
        assert!(!INDEX.contains(">reconcile plan</button>"));
        assert!(!INDEX.contains("id=\"planDetails\""));
    }

    #[test]
    fn upgrade_tab_keeps_one_primary_action_and_collapses_the_custom_field() {
        assert!(INDEX.contains("class=\"card upgrade-card\""));
        assert!(INDEX.contains("class=\"upgrade-form\""));
        assert!(INDEX.contains("<details class=\"advanced plain\" id=\"customImageDetails\">"));
        assert!(INDEX.contains(">Custom image reference</summary>"));
        assert!(INDEX.contains("Preview the operator's reconcile steps"));
        assert!(!INDEX.contains("Preview: the plan uses"));
        assert!(INDEX.contains("function planVisible()"));
        assert!(INDEX.contains("if (planVisible()) await loadPlan();"));
        assert!(INDEX.contains("$(\"customImageDetails\").open = true;"));
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
