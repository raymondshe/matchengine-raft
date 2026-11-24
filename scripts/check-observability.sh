#!/usr/bin/env bash
# Verify the OrbStack lab observability stack end to end.
# Exits non-zero if any check fails. Read-only: no cluster changes.
set -uo pipefail

NAMESPACE="${OBS_NAMESPACE:-match-eu-1}"
OBS_NS="${OBSERVABILITY_NAMESPACE:-match-observability}"
fail=0

check() {
  local name="$1" result="$2" expected="$3"
  if [ "$result" = "$expected" ]; then
    printf 'ok   %-28s %s\n' "$name" "$result"
  else
    printf 'FAIL %-28s got %q, want %q\n' "$name" "$result" "$expected"
    fail=1
  fi
}

echo "== HPA metrics source =="
check "metrics.k8s.io APIService" \
  "$(kubectl get apiservice v1beta1.metrics.k8s.io -o jsonpath='{.status.conditions[0].status}' 2>/dev/null)" "True"
unknown=$(kubectl get hpa -n "$NAMESPACE" -o jsonpath='{range .items[*]}{.status.currentMetrics[*].resource.current.averageUtilization}{"\n"}{end}' 2>/dev/null | grep -c '^<none>$' || true)
check "HPAs with no CPU metric" "$unknown" "0"
kubectl get hpa -n "$NAMESPACE" --no-headers 2>/dev/null | sed 's/^/     /'

echo "== Prometheus =="
targets=$(kubectl -n "$OBS_NS" exec deploy/prometheus -- wget -qO- \
  'http://127.0.0.1:9090/api/v1/targets' 2>/dev/null \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print(" ".join(sorted({t["labels"]["job"]+":"+t["health"] for t in d["data"]["activeTargets"]})))' 2>/dev/null)
check "scrape targets" "$targets" "json-bridge:up match-operator:up prometheus:up"
rules=$(kubectl -n "$OBS_NS" exec deploy/prometheus -- wget -qO- \
  'http://127.0.0.1:9090/api/v1/rules' 2>/dev/null \
  | python3 -c 'import json,sys; print(sum(len(g["rules"]) for g in json.load(sys.stdin)["data"]["groups"]))' 2>/dev/null)
check "alert rules" "$rules" "7"

echo "== Grafana =="
dash=$(kubectl -n "$OBS_NS" exec deploy/grafana -- wget -qO- \
  --header='Authorization: Basic YWRtaW46YWRtaW4=' \
  'http://127.0.0.1:3000/api/dashboards/uid/matching-lab' 2>/dev/null \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["dashboard"]["title"])' 2>/dev/null)
check "provisioned dashboard" "$dash" "MatchEngine · Lab Observability"

echo "== Loki =="
labels=$(kubectl -n "$OBS_NS" exec deploy/loki -- wget -qO- \
  'http://127.0.0.1:3100/loki/api/v1/labels' 2>/dev/null \
  | python3 -c 'import json,sys; print(len(json.load(sys.stdin).get("data") or []))' 2>/dev/null)
if [ "${labels:-0}" -gt 0 ] 2>/dev/null; then
  printf 'ok   %-28s %s labels\n' "container log labels" "$labels"
else
  printf 'FAIL %-28s no logs shipped\n' "container log labels"
  fail=1
fi

if [ "$fail" -eq 0 ]; then
  echo "observability checks passed"
else
  echo "observability checks FAILED"
fi
exit "$fail"
