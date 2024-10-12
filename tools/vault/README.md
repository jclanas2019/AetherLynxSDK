
# AetherLynx SDK - Vault

Este subproyecto del **AetherLynx SDK** proporciona un **Vault** seguro para gestionar información sensible utilizando una combinación de **Rust** y **Python**. Está diseñado para proporcionar almacenamiento seguro con acceso controlado, asegurando confidencialidad y fácil integración con otros módulos.
![AetherLynxVault](/tools/vault/AetherLynxVault.png)


## Estructura del Proyecto

### Archivos de Código

- **`lib.rs`**: Define la lógica principal del vault en Rust, incluyendo el manejo de la encriptación y control de acceso.
- **`main.py`**: Aplicación Python que interactúa con el vault utilizando bindings de Rust y proporciona una API básica.
- **`my_vault.db`**: Base de datos SQLite que almacena los datos sensibles.
- **`Cargo.toml`**: Define las dependencias de Rust y las configuraciones necesarias para el proyecto.

## Requisitos

- **Python 3.11** o superior
- **Rust** y **Cargo** instalados
- **Maturin**: Para compilar los módulos Rust y enlazarlos con Python
- **Uvicorn**: Para ejecutar la API con FastAPI

### Instalación de Maturin
```bash
pip install maturin
```

## Configuración del Proyecto

1. Verifica las dependencias en el archivo `Cargo.toml`.
2. Asegúrate de tener el archivo de base de datos `my_vault.db` en el directorio adecuado.
3. Realiza las configuraciones necesarias en `lib.rs` si necesitas ajustes adicionales en la lógica del vault.

## Compilación y Desarrollo

Para compilar y desarrollar el proyecto, usa el siguiente comando:

```bash
maturin develop
```

Este comando compilará el código Rust e instalará los módulos como extensiones de Python para permitir su uso inmediato.

## Ejecución de la API

Después de compilar los módulos, ejecuta la API de la siguiente manera:

```bash
uvicorn main:app --reload
```

La API estará disponible en: `http://127.0.0.1:8000`.

## Pruebas del Sistema

1. Asegúrate de que los datos sensibles se almacenen y recuperen correctamente desde `my_vault.db`.
2. Prueba las funcionalidades del vault mediante solicitudes a la API utilizando herramientas como `Postman` o `curl`.

Ejemplo de prueba rápida:
```bash
python main.py
```

## Contribuciones

1. Realiza un **fork** del proyecto.
2. Crea una nueva **rama** para tus cambios.
3. Envía un **pull request** con una descripción detallada de los cambios realizados.
