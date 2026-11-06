#!/usr/bin/env bash
# Despliega AetherLynx en Kubernetes.
#
# Uso: scripts/deploy_k8s.sh [--orchestrator] [--telemetry] [--dry-run]
#
#   (por defecto)    agente estático: un Deployment con réplicas fijas
#   --orchestrator   orquestador con runtime de Kubernetes: crea un Deployment
#                    por agente según lo que pida la CLI
#   --telemetry      además, el colector de telemetría propio (OTLP + panel).
#                    Sin él, los servicios arrancan igual y no exportan.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
K8S="$ROOT/kubernetes"
NS="aetherlynx"
DRY=()
MODE=static
TELEMETRY=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY=(--dry-run=server) ;;
    --orchestrator) MODE=orchestrator ;;
    --telemetry) TELEMETRY=1 ;;
    *) echo "opción desconocida: $arg" >&2; exit 2 ;;
  esac
done

require_object() {
  if ! kubectl -n "$NS" get "$1" "$2" >/dev/null 2>&1; then
    echo "error: falta $1/$2 en el namespace $NS. Vea $3" >&2
    [[ ${#DRY[@]} -eq 0 ]] && exit 1
  fi
  return 0
}
require_secret() { require_object secret "$1" "$2"; }

command -v kubectl >/dev/null || { echo "error: kubectl no está instalado" >&2; exit 1; }
kubectl version --request-timeout=5s >/dev/null || { echo "error: no hay conexión con el clúster" >&2; exit 1; }

echo "==> Namespace y RBAC"
kubectl apply "${DRY[@]}" -f "$K8S/namespace.yaml"
kubectl apply "${DRY[@]}" -f "$K8S/rbac.yaml"

require_secret university-agent-secrets kubernetes/examples/agent-secret.example.yaml

if [[ "$TELEMETRY" == 1 ]]; then
  EX=kubernetes/examples/telemetry-secret.example.yaml
  require_secret aetherlynx-telemetry-ingest "$EX"
  require_secret aetherlynx-telemetry-query "$EX"
  require_secret aetherlynx-telemetry-tls "$EX"
  require_object configmap aetherlynx-telemetry-ca "$EX"
  echo "==> Colector de telemetría"
  kubectl apply "${DRY[@]}" -f "$K8S/telemetry.yaml"
  if [[ ${#DRY[@]} -eq 0 ]]; then
    kubectl -n "$NS" rollout status deployment/aetherlynx-telemetry --timeout=180s
  fi
fi

echo "==> Configuración y red de los agentes"
kubectl apply "${DRY[@]}" -f "$K8S/agent-config.yaml"
kubectl apply "${DRY[@]}" -f "$K8S/networkpolicy.yaml"

if [[ "$MODE" == orchestrator ]]; then
  require_secret orchestrator-secrets kubernetes/examples/orchestrator-secret.example.yaml
  require_secret orchestrator-tls kubernetes/examples/orchestrator-secret.example.yaml
  echo "==> Orquestador"
  kubectl apply "${DRY[@]}" -f "$K8S/orchestrator.yaml"
  TARGET=deployment/aetherlynx-orchestrator
else
  echo "==> Agente estático"
  kubectl apply "${DRY[@]}" -f "$K8S/agent-deployment.yaml"
  TARGET=deployment/university-agent
fi

if [[ ${#DRY[@]} -eq 0 ]]; then
  echo "==> Esperando el rollout"
  kubectl -n "$NS" rollout status "$TARGET" --timeout=180s
fi
echo "Listo."
if [[ "$TELEMETRY" == 1 ]]; then
  echo "Panel: kubectl -n $NS port-forward svc/aetherlynx-telemetry 8088 y abra https://127.0.0.1:8088"
fi
