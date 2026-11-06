"""Demostración del vault de AetherLynx.

    export AETHERLYNX_VAULT_PASSPHRASE='una passphrase larga y única'
    python main.py [ruta.db]

La passphrase nunca se escribe en el código ni se guarda en la base de datos.
"""

import sys

import rust_vault


def mask(value: str) -> str:
    return f"•••• ({len(value)} caracteres)"


def main() -> None:
    db_path = sys.argv[1] if len(sys.argv) > 1 else "my_vault.db"
    try:
        vault = rust_vault.RustVault.from_env(db_path)
    except rust_vault.WrongPassphraseError:
        sys.exit("La passphrase no corresponde a este vault.")
    except rust_vault.InvalidInputError as e:
        sys.exit(f"Configuración inválida: {e}")

    vault.add_secret("api_key", "mi_clave_api_super_secreta")
    vault.add_secret("database_password", "contraseña_muy_segura")
    print("Secretos:", vault.list_secrets())
    print("api_key recuperada:", mask(vault.get_secret("api_key")))

    cert = vault.generate_certificate("example.com", days=90, store_key_as="tls/example.com/key")
    print("Certificado:", cert["certificate_pem"].splitlines()[0], "… (clave privada guardada en el vault)")

    vault.delete_secret("database_password")
    print("Secretos restantes:", vault.list_secrets())

    token = vault.encrypt(b"Datos sensibles", context="demo")
    print("Descifrado:", bytes(vault.decrypt(token, context="demo")).decode())
    print("Vuelva a ejecutar el script: los secretos siguen ahí.")


if __name__ == "__main__":
    main()
