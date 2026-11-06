#!/usr/bin/env bash
# Despliega el agente de consultas en Kubernetes.
# Uso: scripts/deploy_k8s.sh [--dry-run]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
K8S="$ROOT/kubernetes"
NS="aetherlynx"
DRY=()
[[ "${1:-}" == "--dry-run" ]] && DRY=(--dry-run=server)

command -v kubectl >/dev/null || { echo "error: kubectl no está instalado" >&2; exit 1; }
kubectl version --request-timeout=5s >/dev/null || { echo "error: no hay conexión con el clúster" >&2; exit 1; }

echo "==> Namespace y RBAC"
kubectl apply "${DRY[@]}" -f "$K8S/namespace.yaml"
kubectl apply "${DRY[@]}" -f "$K8S/rbac.yaml"

if ! kubectl -n "$NS" get secret university-agent-secrets >/dev/null 2>&1; then
  echo "error: falta el secreto 'university-agent-secrets' en el namespace $NS." >&2
  echo "       Créelo siguiendo kubernetes/examples/agent-secret.example.yaml" >&2
  [[ ${#DRY[@]} -eq 0 ]] && exit 1
fi

echo "==> Configuración, red y despliegue"
kubectl apply "${DRY[@]}" -f "$K8S/agent-config.yaml"
kubectl apply "${DRY[@]}" -f "$K8S/networkpolicy.yaml"
kubectl apply "${DRY[@]}" -f "$K8S/agent-deployment.yaml"

if [[ ${#DRY[@]} -eq 0 ]]; then
  echo "==> Esperando el rollout"
  kubectl -n "$NS" rollout status deployment/university-agent --timeout=180s
fi
echo "Listo."
