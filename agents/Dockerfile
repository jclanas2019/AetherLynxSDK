# Imagen del agente de consultas de estudiantes.
#   docker build -t <registro>/aetherlynx/university-agent:0.2.0 agents/
FROM python:3.12-slim-bookworm

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PIP_NO_CACHE_DIR=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1

WORKDIR /app
RUN useradd --uid 10001 --no-create-home --shell /usr/sbin/nologin agent

COPY requirements.txt .
RUN pip install -r requirements.txt

COPY student_queries_agent.py service.py ./

USER 10001
EXPOSE 5000
CMD ["uvicorn", "service:app", "--host", "0.0.0.0", "--port", "5000", "--no-server-header"]
