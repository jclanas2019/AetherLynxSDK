import requests
import json

BASE_URL = "http://localhost:8000"

def register_user():
    username = input("Ingrese el nombre de usuario: ")
    password = input("Ingrese la contraseña: ")
    role = input("Ingrese el rol: ")
    url = f"{BASE_URL}/register"
    data = {"username": username, "password": password, "role": role}
    response = requests.post(url, json=data)
    return response.json()

def authenticate_user():
    username = input("Ingrese el nombre de usuario: ")
    password = input("Ingrese la contraseña: ")
    resource = input("Ingrese el recurso: ")
    url = f"{BASE_URL}/authenticate"
    data = {"username": username, "password": password, "resource": resource}
    response = requests.post(url, json=data)
    return response.json()

def get_user_info():
    username = input("Ingrese el nombre de usuario: ")
    url = f"{BASE_URL}/users/{username}"
    response = requests.get(url)
    return response.json()

def assign_role():
    username = input("Ingrese el nombre de usuario: ")
    role = input("Ingrese el rol a asignar: ")
    url = f"{BASE_URL}/assign-role/{username}"
    data = {"role": role}
    response = requests.post(url, json=data)
    return response.json()

def add_resource():
    resource = input("Ingrese el nombre del recurso: ")
    allowed_role = input("Ingrese el rol permitido: ")
    url = f"{BASE_URL}/add-resource"
    params = {"resource": resource, "allowed_role": allowed_role}
    response = requests.post(url, params=params)
    return response.json()

def list_accessible_resources():
    username = input("Ingrese el nombre de usuario: ")
    url = f"{BASE_URL}/accessible-resources/{username}"
    response = requests.get(url)
    return response.json()

def verify_token():
    token = input("Ingrese el token a verificar: ")
    url = f"{BASE_URL}/verify-token"
    response = requests.post(url, json=token)
    return response.json()

def print_menu():
    print("\n--- Menú ---")
    print("1. Registrar usuario")
    print("2. Autenticar usuario")
    print("3. Obtener información de usuario")
    print("4. Asignar rol")
    print("5. Añadir recurso")
    print("6. Listar recursos accesibles")
    print("7. Verificar token")
    print("8. Salir")

def main():
    while True:
        print_menu()
        choice = input("Seleccione una opción: ")
        
        try:
            if choice == '1':
                result = register_user()
            elif choice == '2':
                result = authenticate_user()
            elif choice == '3':
                result = get_user_info()
            elif choice == '4':
                result = assign_role()
            elif choice == '5':
                result = add_resource()
            elif choice == '6':
                result = list_accessible_resources()
            elif choice == '7':
                result = verify_token()
            elif choice == '8':
                print("Saliendo del programa...")
                break
            else:
                print("Opción no válida. Por favor, intente de nuevo.")
                continue

            print(json.dumps(result, indent=2))
        except requests.exceptions.RequestException as e:
            print(f"Error en la solicitud: {e}")
        except json.JSONDecodeError:
            print("Error al decodificar la respuesta JSON")
        
        input("Presione Enter para continuar...")

if __name__ == "__main__":
    main()