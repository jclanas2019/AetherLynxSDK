
# AetherLynx SDK - PAM/PIM

Este proyecto forma parte del subproyecto **AetherLynx SDK** y proporciona funcionalidades de autenticación y gestión de usuarios mediante módulos de PAM/PIM. Está diseñado para sistemas que necesitan una arquitectura desacoplada y segura para manejar usuarios y permisos, utilizando componentes en **Rust** y **Python**.

## Estructura del Proyecto

### Archivos de Código

- **`audit.rs`**: Implementa los módulos de auditoría para capturar y registrar eventos críticos del sistema.
- **`auth.rs`**: Proporciona las funciones de autenticación utilizando clave y token con expiración.
- **`db.rs`**: Gestiona la conexión con la base de datos SQLite, evitando duplicación de código en la inserción de usuarios.
- **`ipa.rs`**: Ofrece integración con servicios IPA para la gestión de identidades.
- **`pam.rs`**: Implementa la interacción con módulos PAM para autenticación.
- **`lib.rs`**: Contiene las configuraciones principales y funciones compartidas entre módulos.
- **`app.py`**: Aplicación en Python que proporciona una API utilizando **FastAPI**.
- **`cliente.py`**: Cliente que interactúa con la API de FastAPI para probar la funcionalidad del sistema.
- **`Cargo.toml`**: Define las dependencias de Rust y la configuración del proyecto.
- **`config.toml`**: Archivo de configuración del sistema.

## Requisitos

- **Python 3.11** o superior
- **Rust** y **Cargo** instalados
- **Maturin**: Para compilar los módulos Rust y enlazarlos con Python
- **FastAPI** y **Uvicorn**: Para ejecutar la API web

### Instalación de Maturin
```bash
pip install maturin
```

## Configuración del Proyecto

1. Asegúrate de que tienes todas las dependencias definidas en `Cargo.toml`.
2. Configura el archivo `config.toml` según los parámetros de tu entorno.
3. Verifica que no haya duplicación de lógica en la inserción de usuarios en los módulos `db.rs` y `app.py`.

## Compilación y Desarrollo

Para compilar los módulos en desarrollo, usa el siguiente comando:

```bash
maturin develop
```

Este comando compilará el código Rust e instalará los módulos como extensiones de Python, permitiendo la interacción directa.

## Ejecución de la API

Una vez que los módulos estén compilados y disponibles, puedes iniciar la API:

```bash
uvicorn app:app --reload
```

Esto iniciará el servidor en `http://127.0.0.1:8000`.

## Pruebas del Sistema

Puedes utilizar el cliente Python proporcionado en `cliente.py` para probar la API.

```bash
python cliente.py
```

Asegúrate de que todas las funcionalidades de autenticación y gestión de usuarios funcionan correctamente.

## Contribuciones

1. Realiza un **fork** del proyecto.
2. Crea una nueva **rama** para tus cambios.
3. Envía un **pull request** con una descripción detallada de los cambios realizados.
