# AetherLynx SDK

![AetherLynx SDK Logo](AetherLynxLogo.png)

AetherLynx SDK es un kit de desarrollo de software diseñado para orquestar y gestionar agentes de modelos de lenguaje a gran escala (LLM) con un enfoque en alta escalabilidad, resiliencia y flexibilidad. Este SDK se inspira en la arquitectura de Kubernetes, pero está adaptado para ser extremadamente eficiente en la gestión de agentes de IA y operaciones de inferencia en entornos distribuidos.

## Características

- **Orquestación de Agentes LLM**: AetherLynx gestiona el ciclo de vida de los agentes, incluyendo la creación, escalado y terminación de instancias de modelos de lenguaje.
- **Reconciliación de Estados**: Basado en un enfoque declarativo, el sistema ajusta continuamente el estado actual para alinearlo con el estado deseado.
- **Alta Escalabilidad y Disponibilidad**: Diseñado para manejar grandes volúmenes de tráfico mediante la autoescalación y balanceo de carga.
- **API gRPC y REST**: Interfaz robusta para interactuar con los agentes y el orquestador.
- **Integración con Sistemas Transaccionales**: Se integra de forma nativa con sistemas transaccionales, permitiendo respuestas enriquecidas en tiempo real.

## Requisitos

- **Rust**: Versión >= 1.50
- **Python 3.8+**: Para los agentes basados en modelos de lenguaje.
- **Cargo**: Para la compilación y gestión de dependencias en Rust.
- **vcpkg**: Para la gestión de dependencias de sistema en Windows (opcional).

## Instalación

### Paso 1: Clonar el repositorio

```bash
git clone https://github.com/your_username/aetherlynxsdk.git
cd aetherlynxsdk

### Paso 2: Compilar el orquestador en Rust

```bash
cd agent_system
cargo build --release

### Paso 3: Ejecutar el orquestador

```bash
./target/release/aetherlynx-orchestrator

### Paso 4: Ejecutar los Agentes (Python)

```bash
cd agents
pip install -r requirements.txt
python3 student_queries_agent.py

Uso
### Crear un Agente LLM

```bash
aetherlynx-cli agent create --name "student-agent" --model "gpt-4"

Escalar un Agente

```bash
aetherlynx-cli agent scale --name "student-agent" --replicas 5


### Monitorear el Estado del Sistema

```bash
aetherlynx-cli orchestrator status


###  Arquitectura
La arquitectura de AetherLynx SDK sigue un enfoque de microservicios donde cada componente es gestionado de forma independiente, asegurando alta disponibilidad y fácil escalabilidad. Utiliza gRPC para la comunicación entre los diferentes servicios y agentes LLM, permitiendo interacciones rápidas y seguras.

###  Contribuir
Si deseas contribuir a este proyecto, por favor realiza un fork del repositorio y envía un pull request con tus cambios.

### Licencia
AetherLynx SDK está disponible bajo la licencia MIT. Ver el archivo LICENSE para más detalles.




