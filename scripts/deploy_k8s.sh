#!/bin/bash

# Aplicar configuraciones de RBAC
kubectl apply -f config/rbac.yaml

# Desplegar los agentes en Kubernetes
kubectl apply -f kubernetes/agent-deployment.yaml

# Desplegar Prometheus para el monitoreo
kubectl apply -f kubernetes/prometheus-config.yaml
