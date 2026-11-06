"""Cliente interactivo para la API de autenticación de AetherLynx.

Uso:  AETHERLYNX_AUTH_URL=http://localhost:8000 python cliente.py
"""

from __future__ import annotations

import getpass
import json
import os

import requests

BASE_URL = os.environ.get("AETHERLYNX_AUTH_URL", "http://localhost:8000").rstrip("/")
TIMEOUT = 10


class Session:
    def __init__(self) -> None:
        self.token: str | None = None
        self.username: str | None = None

    def headers(self) -> dict:
        return {"Authorization": f"Bearer {self.token}"} if self.token else {}

    def request(self, method: str, path: str, **kw) -> dict:
        r = requests.request(method, f"{BASE_URL}{path}", headers=self.headers(), timeout=TIMEOUT, **kw)
        try:
            data = r.json()
        except ValueError:
            data = {"detail": r.text}
        if r.status_code >= 400:
            data = {"error": r.status_code, **(data if isinstance(data, dict) else {"detail": data})}
        return data


s = Session()


def register_user() -> dict:
    username = input("Usuario: ")
    password = getpass.getpass("Contraseña (mín. 12 caracteres): ")
    return s.request("POST", "/register", json={"username": username, "password": password})


def login() -> dict:
    username = input("Usuario: ")
    password = getpass.getpass("Contraseña: ")
    resource = input("Recurso a comprobar (opcional): ").strip() or None
    body = {"username": username, "password": password}
    if resource:
        body["resource"] = resource
    data = s.request("POST", "/authenticate", json=body)
    if "access_token" in data:
        s.token, s.username = data["access_token"], username.strip().lower()
        data = {**data, "access_token": data["access_token"][:16] + "…"}
    return data


def me() -> dict:
    return s.request("GET", "/me")


def get_user_info() -> dict:
    return s.request("GET", f"/users/{input('Usuario: ')}")


def assign_role() -> dict:
    username = input("Usuario: ")
    role = input("Rol: ")
    return s.request("POST", f"/assign-role/{username}", json={"role": role})


def add_resource() -> dict:
    resource = input("Recurso: ")
    role = input("Rol permitido: ")
    return s.request("POST", "/add-resource", json={"resource": resource, "allowed_role": role})


def list_accessible_resources() -> dict:
    username = input(f"Usuario [{s.username or ''}]: ") or (s.username or "")
    return s.request("GET", f"/accessible-resources/{username}")


def verify_token() -> dict:
    token = getpass.getpass("Token a verificar: ")
    return s.request("POST", "/verify-token", json={"token": token})


MENU = [
    ("Registrarse", register_user),
    ("Iniciar sesión", login),
    ("Ver mi cuenta", me),
    ("Ver usuario (admin)", get_user_info),
    ("Asignar rol (admin)", assign_role),
    ("Añadir recurso (admin)", add_resource),
    ("Listar recursos accesibles", list_accessible_resources),
    ("Verificar token", verify_token),
]


def main() -> None:
    while True:
        print(f"\n--- AetherLynx Auth ({BASE_URL}) — sesión: {s.username or 'ninguna'} ---")
        for i, (label, _) in enumerate(MENU, 1):
            print(f"{i}. {label}")
        print(f"{len(MENU) + 1}. Salir")
        choice = input("Opción: ").strip()
        if choice == str(len(MENU) + 1):
            break
        if not choice.isdigit() or not 1 <= int(choice) <= len(MENU):
            print("Opción no válida.")
            continue
        try:
            print(json.dumps(MENU[int(choice) - 1][1](), indent=2, ensure_ascii=False))
        except requests.RequestException as e:
            print(f"Error de conexión: {e}")


if __name__ == "__main__":
    main()
