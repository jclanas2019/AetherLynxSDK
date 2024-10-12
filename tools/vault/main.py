import rust_vault

def main():
    # Crear una nueva instancia del vault, especificando la ruta de la base de datos
    vault = rust_vault.RustVault("my_vault.db")

    # Agregar algunos secretos
    vault.add_secret("api_key", "mi_clave_api_super_secreta")
    vault.add_secret("database_password", "contraseña_muy_segura")

    # Listar los secretos
    secrets = vault.list_secrets()
    print("Secretos en el vault:", secrets)

    # Obtener y mostrar un secreto
    api_key = vault.get_secret("api_key")
    print("API Key recuperada:", api_key)

    # Generar un certificado
    cert = vault.generate_certificate("example.com")
    print("Certificado generado:", cert[:100] + "...") # Mostramos solo una parte para no saturar la salida

    # Eliminar un secreto
    vault.delete_secret("database_password")
    
    # Verificar que el secreto se eliminó
    remaining_secrets = vault.list_secrets()
    print("Secretos restantes:", remaining_secrets)

    # Encriptar y desencriptar datos arbitrarios
    data = b"Datos sensibles"
    encrypted = vault.encrypt(data)
    print("Datos encriptados:", encrypted)
    decrypted = vault.decrypt(encrypted)
    print("Datos desencriptados:", bytes(decrypted).decode())

if __name__ == "__main__":
    main()