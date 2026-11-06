"""Servicio HTTP del agente de consultas (el contenedor que despliega Kubernetes).

  POST /query    {"student_id": "...", "question": "..."}   requiere token
  GET  /healthz  vida del proceso
  GET  /readyz   listo para atender (configuración cargada)

Seguridad: este servicio se llama desde un backend de confianza (portal del
estudiante, chatbot) que ya autenticó al estudiante y le pasa SU propio
student_id. El token AETHERLYNX_AGENT_TOKEN identifica a ese backend; no lo
exponga a navegadores.

Ejecutar:  uvicorn service:app --host 0.0.0.0 --port 5000
"""

from __future__ import annotations

import hmac
import logging
import os
from contextlib import asynccontextmanager
from typing import Annotated

from fastapi import Depends, FastAPI, HTTPException, status
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from pydantic import BaseModel, Field

from student_queries_agent import MAX_QUESTION_CHARS, InvalidRequest, StudentQueryAgent, build_agent

log = logging.getLogger("aetherlynx.agent.service")


class _State:
    agent: StudentQueryAgent | None = None
    token: bytes = b""


state = _State()


@asynccontextmanager
async def lifespan(_app: FastAPI):
    token = os.environ.get("AETHERLYNX_AGENT_TOKEN", "")
    if len(token) < 32:
        raise RuntimeError("AETHERLYNX_AGENT_TOKEN no está definido o tiene menos de 32 caracteres")
    if "REEMPLAZAR" in token.upper() or len(set(token)) < 8:
        raise RuntimeError("AETHERLYNX_AGENT_TOKEN es un valor de ejemplo o trivial; genere uno aleatorio")
    state.token = token.encode()
    if state.agent is None:
        state.agent = build_agent()
    yield


app = FastAPI(title="AetherLynx Student Agent", version="0.2.0", lifespan=lifespan)
_bearer = HTTPBearer(auto_error=False)


def require_token(creds: Annotated[HTTPAuthorizationCredentials | None, Depends(_bearer)]) -> None:
    if creds is None or not hmac.compare_digest(creds.credentials.encode(), state.token):
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "token inválido", headers={"WWW-Authenticate": "Bearer"})


class Query(BaseModel):
    student_id: Annotated[str, Field(min_length=1, max_length=32)]
    question: Annotated[str, Field(min_length=1, max_length=MAX_QUESTION_CHARS)]


class Answer(BaseModel):
    answer: str


@app.get("/healthz", include_in_schema=False)
def healthz() -> dict:
    return {"status": "ok"}


@app.get("/readyz", include_in_schema=False)
def readyz() -> dict:
    if state.agent is None:
        raise HTTPException(status.HTTP_503_SERVICE_UNAVAILABLE, "no listo")
    return {"status": "ready"}


@app.post("/query", response_model=Answer, dependencies=[Depends(require_token)])
def query(body: Query) -> Answer:
    assert state.agent is not None
    try:
        return Answer(answer=state.agent.answer(body.question, body.student_id))
    except InvalidRequest as e:
        raise HTTPException(422, str(e)) from None
