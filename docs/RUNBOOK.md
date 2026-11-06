# Runbook de operación

Qué hacer ante cada alerta del colector de telemetría (`aetherlynx-telemetry`).
Las alertas se evalúan cada 30 s (`--alert-interval`), se ven en la pestaña
**Resumen** del panel y con:

```bash
aetherlynx-cli telemetry alerts    # código de salida 2 si hay alguna crítica: útil en scripts
```

Cada alerta trae una pista breve; aquí está el procedimiento completo.

## Acceso

```bash
# Panel (Kubernetes): el certificado está emitido para el nombre del Service,
# así que el navegador avisará al entrar por 127.0.0.1.
kubectl -n aetherlynx port-forward svc/aetherlynx-telemetry 8088
# https://127.0.0.1:8088 → pegue el token de consulta (Secret aetherlynx-telemetry-query)

export AETHERLYNX_TELEMETRY_URL=https://127.0.0.1:8088
export AETHERLYNX_TELEMETRY_TOKEN=$(kubectl -n aetherlynx get secret aetherlynx-telemetry-query -o jsonpath='{.data.token}' | base64 -d)
export AETHERLYNX_TELEMETRY_CA_CERT=ca.crt    # la CA del colector
```

## Alertas

### ServiceSilent · crítica

**Qué significa.** Un servicio que antes enviaba telemetría lleva más de 5 min
(`--alert-silent-after`) sin enviar nada.

**Diagnóstico.**
1. ¿Está vivo? `kubectl -n aetherlynx get pods` y `kubectl describe pod` (reinicios, OOMKilled, CrashLoopBackOff).
2. Si está vivo, el problema es la exportación. En los logs locales del pod busque errores del exportador (`BatchSpanProcessor`, `Failed to export`, `401`, certificado).
   - `401`: el token de ingesta del pod no coincide con el del colector. Compare `aetherlynx-telemetry-ingest` (`token` y `otlp-headers` deben llevar el mismo valor).
   - Error de certificado: el ConfigMap `aetherlynx-telemetry-ca` no corresponde al certificado del colector, o el certificado no incluye `aetherlynx-telemetry.aetherlynx.svc`.
   - Conexión rechazada o tiempo agotado: revise que la NetworkPolicy del pod permita salida al puerto 4318 del colector.
3. Un servicio retirado a propósito deja de alertar cuando su última señal sale de la retención (24 h por defecto).

### ReconcileStalled · crítica

**Qué significa.** El orquestador sigue enviando telemetría, pero hace más de
3 min (`--alert-reconcile-stale`) que ningún ciclo de reconciliación hace
progreso: el runtime no respondió bien a ninguno de los agentes, o falló la
limpieza. También salta si el orquestador nunca llegó a completar un ciclo
con progreso desde que arrancó. Los cambios de réplicas no se aplican. (Si
solo falla un agente, la alerta es AgentsFailed.)

**Diagnóstico.**
1. `aetherlynx-cli telemetry logs --service aetherlynx-orchestrator --severity 13`.
2. `aetherlynx-cli telemetry traces --service aetherlynx-orchestrator --errors` y abra un `reconcile_cycle`: el span `runtime.apply` o `runtime.delete` con error dice qué agente y por qué.
3. Causas habituales: API de Kubernetes lento o caído, permisos RBAC retirados (`forbidden` en el mensaje), cuota agotada.

### HighErrorRate · aviso

**Qué significa.** Más del 5 % (`--alert-error-ratio`) de las operaciones de
un servicio en los últimos 15 min terminaron con error de servidor (al menos
20 operaciones en la ventana, para no alertar por un caso aislado). Una
operación es una petición que el servicio atiende (span de servidor de
entrada); el trabajo interno, como los ciclos del reconciliador, no cuenta.
Los errores del cliente (4xx, `InvalidArgument`, `NotFound`…) tampoco.

**Diagnóstico.** `aetherlynx-cli telemetry traces --service <servicio> --errors`,
luego `aetherlynx-cli telemetry trace <id>`: el span en rojo y su mensaje de
estado indican dónde falló. Los logs de esa traza: `aetherlynx-cli telemetry logs --trace <id>`.

### AgentsFailed · aviso

**Qué significa.** El orquestador reporta agentes en fase `Failed`.

**Diagnóstico.** `aetherlynx-cli agent list`: la columna MENSAJE indica la
causa (rollout atascado, cuota, nombre ocupado por un objeto ajeno, imagen
inexistente). Corrija la causa; el reconciliador reintenta solo.

### ReplicasBelowDesired · aviso

**Qué significa.** En todas las muestras de los últimos 10 min, que cubren al
menos 5 min, hubo menos réplicas listas que las deseadas. Un rollout normal
no la dispara; un pod que nunca llega a Ready, sí.

**Diagnóstico.** `kubectl -n aetherlynx get events --sort-by=.lastTimestamp`.
Las causas habituales son una imagen que no se puede descargar, recursos
insuficientes en los nodos o una sonda `/readyz` que falla (el agente no
consigue su configuración: revise `university-agent-config` y el Secret).

### LoginFailureSpike · aviso

**Qué significa.** La API de auth registra más de 30 inicios de sesión
fallidos por minuto (promedio de 5 min). Puede ser un ataque de fuerza bruta
o de relleno de credenciales.

**Diagnóstico.** La telemetría no guarda nombres de usuario ni IPs, a
propósito. Para ver quién: `GET /audit` de la API de auth (requiere rol
`admin`). El límite por IP (`AETHERLYNX_RATE_LIMIT_PER_MINUTE`) ya frena a un
solo origen; si los fallos vienen de muchas IPs, bloquee en el balanceador o
WAF y considere forzar el cambio de contraseña de las cuentas afectadas. La
métrica `aetherlynx.auth.logins{result="rate_limited"}` muestra cuántos
intentos frenó el límite.

### LlmErrors · aviso

**Qué significa.** Más del 20 % de las llamadas al proveedor LLM fallan (al
menos 10 en 10 min). Los estudiantes reciben la respuesta de «servicio no
disponible».

**Diagnóstico.** En la traza de una consulta fallida, el span `llm.complete`
lleva el tipo de excepción. Causas habituales: clave de API revocada o sin
saldo, límite de tasa del proveedor, salida HTTPS bloqueada por la
NetworkPolicy. Revise el estado del proveedor antes de tocar nada propio.

### UpstreamErrors · aviso

**Qué significa.** Más del 20 % de las llamadas al API de la universidad
(`/students`, `/finances`) fallan.

**Diagnóstico.** El span `upstream.<ruta>` muestra el código HTTP o la
excepción. `401/403`: token del API vencido. Tiempos agotados: el API está
lento o la NetworkPolicy no permite llegar a él (si es interno, añada su
CIDR).

## El colector

- **Disco.** La base no pasa de `--max-db-mib` (1536 MiB en el manifiesto,
  con un volumen de 2Gi). Al llegar al tope se descarta el 10 % más antiguo y
  se cuenta en «Filas descartadas» (panel) o `telemetry status`. Si eso pasa a
  menudo, la retención efectiva es menor que `--retention`: amplíe el PVC y
  el tope, o reduzca la retención.
- **Servicios descartados.** Por encima de `--max-services` (256) distintos,
  los datos de servicios nuevos se descartan y se cuentan. Suele indicar un
  cliente que genera nombres de servicio variables: corrija su
  `OTEL_SERVICE_NAME`.
- **Reinicio.** Los datos persisten en el volumen; el estado de las alertas
  (desde cuándo está activa cada una) se recalcula.
- **Rotación de tokens.** Genere el token nuevo, actualice el Secret
  (`token` y `otlp-headers` a la vez) y reinicie el colector y luego los
  clientes (`kubectl rollout restart`). Mientras tanto, los clientes con el
  token viejo reciben `401` y descartan sus datos: el corte dura lo que tarda
  el rollout.
- **Certificado.** El colector lee el certificado al arrancar: tras renovarlo,
  reinícielo.
