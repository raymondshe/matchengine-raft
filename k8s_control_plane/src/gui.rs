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
    fn rollout_banner_requires_an_incomplete_rollout() {
        assert!(
            INDEX.contains("const incomplete = !progress || progress.complete !== true;"),
            "a missing rollout.complete must count as incomplete so the banner still shows"
        );
        assert!(INDEX.contains(
            "const rollout = progressing && ((packing === \"containers\" && incomplete) || (packing === \"supervisor\" && progress && !progress.complete));"
        ));
        assert!(
            !INDEX.contains("(packing === \"containers\" ||"),
            "a stale Progressing=True with complete=true must not show the banner"
        );
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
    fn backup_tab_shows_published_status_and_signed_drill() {
        assert!(INDEX.contains("data-tab=\"backup\""));
        assert!(INDEX.contains("id=\"tab-backup\""));
        assert!(INDEX.contains("id=\"backupCards\""));
        assert!(INDEX.contains("id=\"backupRefresh\""));
        assert!(INDEX.contains("/api/v1/backup-status?shard="));
        assert!(INDEX.contains("function renderBackupStatus"));
        assert!(INDEX.contains("not-configured"));
        assert!(INDEX.contains("Newest signed drill"));
        assert!(INDEX.contains("deploy/backup.yaml"));
        assert!(INDEX.contains("backupTabVisible"));
    }

    #[test]
    fn market_data_is_top_level_with_terminal_and_fast_subtabs() {
        assert!(INDEX.contains("data-view=\"engine\""));
        assert!(INDEX.contains("data-view=\"marketdata\""));
        assert!(!INDEX.contains("data-view=\"marketdata-terminal\""));
        assert!(!INDEX.contains("data-view=\"marketdata-fast\""));
        assert!(INDEX.contains(">market data</button>"));
        assert!(INDEX.contains("data-md-tab=\"terminal\""));
        assert!(INDEX.contains("data-md-tab=\"fast\""));
        assert!(INDEX.contains(">terminal market data</button>"));
        assert!(INDEX.contains(">fast market data</button>"));
        assert!(INDEX.contains("id=\"engineView\""));
        assert!(INDEX.contains("id=\"marketDataView\""));
        assert!(INDEX.contains("id=\"marketDataTerminalPanel\""));
        assert!(INDEX.contains("id=\"marketDataFastPanel\""));
        assert!(INDEX.contains("id=\"terminalMarketDataCards\""));
        assert!(INDEX.contains("id=\"fastMarketDataCards\""));
        assert!(INDEX.contains("id=\"terminalMarketDataRows\""));
        assert!(INDEX.contains("id=\"fastMarketDataRows\""));
        assert!(INDEX.contains("id=\"marketDataRefresh\""));
        assert!(!INDEX.contains("id=\"terminalMarketDataView\""));
        assert!(!INDEX.contains("id=\"fastMarketDataView\""));
        assert!(!INDEX.contains("data-tab=\"marketdata\""));
        assert!(INDEX.contains("function setView"));
        assert!(INDEX.contains("function setMarketDataTab"));
        assert!(INDEX.contains("function marketDataTabFromView"));
        assert!(INDEX.contains("function loadMarketData"));
        assert!(INDEX.contains("function renderMarketData"));
        assert!(INDEX.contains("function renderMarketDataTable"));
        assert!(INDEX.contains("function renderMarketDataCards"));
        assert!(INDEX.contains("/api/v1/market-data-services"));
        assert!(INDEX.contains("/manifests"));
        assert!(INDEX.contains("lastFrameAgeMs"));
        assert!(INDEX.contains("subscribers"));
        assert!(INDEX.contains("wsPaths"));
        assert!(INDEX.contains("/stream?streams=..."));
        assert!(INDEX.contains("sourceShards"));
        assert!(INDEX.contains("spec.autoscaling?.minReplicas"));
        assert!(INDEX.contains("isMarketDataView"));
        assert!(INDEX.contains("currentView === \"marketdata\""));
        assert!(INDEX.contains("marketdata-terminal"));
        assert!(INDEX.contains("marketdata-fast"));
    }

    #[test]
    fn market_data_create_edit_scale_plan_and_rollback_controls_exist() {
        // Create/edit and scale entry points.
        assert!(INDEX.contains("id=\"newMarketDataBtn\""));
        assert!(INDEX.contains("id=\"marketDataEditModal\""));
        assert!(INDEX.contains("id=\"marketDataEditForm\""));
        assert!(INDEX.contains("id=\"mdEditName\""));
        assert!(INDEX.contains("id=\"mdEditCell\""));
        assert!(INDEX.contains("id=\"mdEditJson\""));
        assert!(INDEX.contains("function openMarketDataEdit"));
        assert!(INDEX.contains("function submitMarketDataEdit"));
        assert!(INDEX.contains("id=\"marketDataScaleModal\""));
        assert!(INDEX.contains("id=\"mdScaleReplicas\""));
        assert!(INDEX.contains("function submitMarketDataScale"));
        assert!(INDEX.contains("data-md-edit="));
        assert!(INDEX.contains("data-md-scale="));
        assert!(INDEX.contains("previousImage"));
        assert!(INDEX.contains("function rollbackMarketData"));
        assert!(INDEX.contains("/rollback"));

        // Read-only preview plan.
        assert!(INDEX.contains("data-md-tab=\"plan\""));
        assert!(INDEX.contains("id=\"marketDataPlanPanel\""));
        assert!(INDEX.contains("id=\"mdPlanService\""));
        assert!(INDEX.contains("id=\"mdPlanScenario\""));
        assert!(INDEX.contains("id=\"mdPlanSteps\""));
        assert!(INDEX.contains("function loadMarketDataPlan"));
        assert!(INDEX.contains("function renderMarketDataPlan"));
        assert!(INDEX.contains("/plan?scenario="));
        assert!(INDEX.contains("marketdata-plan"));
    }

    #[test]
    fn order_system_view_lists_user_shards_and_targets() {
        assert!(INDEX.contains("data-view=\"ordersystem\""));
        assert!(INDEX.contains(">order systems</button>"));
        assert!(INDEX.contains("id=\"orderSystemView\""));
        assert!(INDEX.contains("id=\"orderSystemCards\""));
        assert!(INDEX.contains("id=\"orderSystemRows\""));
        assert!(INDEX.contains("id=\"orderSystemRefresh\""));
        assert!(INDEX.contains("function loadOrderSystems"));
        assert!(INDEX.contains("function renderOrderSystems"));
        assert!(INDEX.contains("/api/v1/order-systems"));
        assert!(INDEX.contains("User-ID shards"));
        assert!(INDEX.contains("Engine targets"));
        assert!(INDEX.contains("currentView === \"ordersystem\""));
        assert!(!INDEX.contains("orderTicketForm"));
        assert!(!INDEX.contains("submitOrderTicket"));
        assert!(!INDEX.contains("/shards/${encodeURIComponent(selection.shard.name)}/orders"));
        assert!(INDEX.contains("function orderSystemWebuiUrl"));
        assert!(INDEX.contains("/demo/"));
        assert!(INDEX.contains("<th>Order desk</th>"));
        assert!(INDEX.contains("Demo order desk"));
        assert!(INDEX.contains("the manager does not proxy"));
        assert!(INDEX.contains("data-order-upgrade="));
        assert!(INDEX.contains("function upgradeOrderSystem"));
        assert!(INDEX.contains("id=\"orderSystemUpgradeModal\""));
        assert!(INDEX.contains("id=\"osUpgradeImageSelect\""));
        assert!(INDEX.contains("/api/v1/images?orderSystem="));
        assert!(!INDEX.contains("window.prompt"));
        assert!(INDEX.contains("/upgrade`"));
        assert!(INDEX.contains("hpa ${esc(minReplicas)}"));
    }

    #[test]
    fn trading_terminal_view_links_to_the_webui_without_an_order_ticket() {
        // Fourth top-level view and its panel.
        assert!(INDEX.contains("data-view=\"tradingterminal\""));
        assert!(INDEX.contains(">trading terminal</button>"));
        assert!(INDEX.contains("id=\"tradingTerminalView\""));
        assert!(INDEX.contains("id=\"tradingTerminalCards\""));
        assert!(INDEX.contains("id=\"tradingTerminalRows\""));
        assert!(INDEX.contains("id=\"tradingTerminalRefresh\""));
        // State, loaders and the table renderer.
        assert!(INDEX.contains("let tradingTerminalItems = [];"));
        assert!(INDEX.contains("function loadTradingTerminals"));
        assert!(INDEX.contains("function renderTradingTerminals"));
        assert!(INDEX.contains("function tradingTerminalWebuiUrl"));
        assert!(INDEX.contains("/api/v1/trading-terminals"));
        assert!(INDEX.contains("data-terminal-upgrade="));
        assert!(INDEX.contains("/manifests"));
        assert!(INDEX.contains("Proxied upstreams"));
        assert!(INDEX.contains("Trading terminal</a>"));
        assert!(INDEX.contains("/md/*"));
        assert!(INDEX.contains("/demo/api/*"));
        // View switching, hash boot mapping and live refresh.
        assert!(INDEX.contains("currentView === \"tradingterminal\""));
        assert!(INDEX.contains("view === \"tradingterminal\""));
        assert!(INDEX.contains("location.hash === \"#tradingterminal\""));
        assert!(INDEX.contains("requestedView === \"tradingterminal\""));
        assert!(INDEX.contains("if (currentView === \"tradingterminal\" && liveTicks % 3 === 0) await loadTradingTerminals();"));
        // Stateless image roll via the scoped image inventory.
        assert!(INDEX.contains("id=\"tradingTerminalUpgradeModal\""));
        assert!(INDEX.contains("id=\"ttUpgradeImageSelect\""));
        assert!(INDEX.contains("function upgradeTradingTerminal"));
        assert!(INDEX.contains("function submitTradingTerminalUpgrade"));
        assert!(INDEX.contains("/api/v1/images?tradingTerminal="));
        assert!(INDEX.contains("maxUnavailable: 0"));
        // The GUI only links to the terminal WebUI; order entry stays in the terminal/BFF.
        assert!(!INDEX.contains("/write"));
        assert!(!INDEX.contains("orderTicketForm"));
        assert!(!INDEX.contains("submitOrderTicket"));
        assert!(!INDEX.contains("fetch(\"/api/v1/shards/"));
    }

    #[test]
    fn market_data_upgrade_uses_the_stateless_rolling_action() {
        assert!(INDEX.contains("id=\"marketDataUpgradeModal\""));
        assert!(INDEX.contains("id=\"marketDataUpgradeForm\""));
        assert!(INDEX.contains("id=\"mdUpgradeImageSelect\""));
        assert!(INDEX.contains("id=\"mdUpgradeWire\""));
        assert!(INDEX.contains("id=\"mdUpgradeSnapshot\""));
        assert!(INDEX.contains("data-md-upgrade="));
        assert!(INDEX.contains("function openMarketDataUpgrade"));
        assert!(INDEX.contains("function submitMarketDataUpgrade"));
        assert!(INDEX.contains("/api/v1/images?marketDataService="));
        assert!(INDEX.contains("encodeURIComponent(name)}/upgrade`"));
        assert!(INDEX.contains("marketDataUpgradeName"));
        assert!(INDEX.contains("operator rolling"));
        assert!(INDEX.contains("sourceShards?.length"));
        assert!(INDEX.contains("maxUnavailable: 0"));
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
