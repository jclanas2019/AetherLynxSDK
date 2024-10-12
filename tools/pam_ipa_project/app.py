from fastapi import FastAPI, HTTPException, Body
from pydantic import BaseModel
from pam_ipa_project import PyAuthManager, PyIPA

app = FastAPI()

# Inicialización de PyAuthManager y PyIPA
auth_manager = PyAuthManager()
ipa_manager = PyIPA()

# Modelo para la solicitud de registro de usuario
class UserRegister(BaseModel):
    username: str
    password: str
    role: str

# Modelo para la autenticación de usuario
class UserAuth(BaseModel):
    username: str
    password: str
    resource: str

# Modelo para la asignación de roles
class RoleAssign(BaseModel):
    role: str

# Ruta para registrar un nuevo usuario
@app.post("/register")
async def register_user(user: UserRegister):
    try:
        # Registrar al usuario en PyAuthManager
        auth_manager.register_user(user.username, user.password)
        # Asignar rol al usuario en PyIPA
        ipa_manager.add_role(user.username, user.role)
        return {"message": "User registered successfully"}
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"User registration failed: {str(e)}")

# Ruta para autenticar un usuario
@app.post("/authenticate")
async def authenticate_user(user: UserAuth):
    try:
        # Autenticar usando PyAuthManager
        token = auth_manager.authenticate_user(user.username, user.password)
        
        # Verificar si el usuario tiene acceso al recurso
        has_access = ipa_manager.has_access(user.username, user.resource)
        if has_access:
            return {"message": "Authenticated successfully", "token": token, "resource": user.resource}
        else:
            raise HTTPException(status_code=403, detail="Access denied to the resource")
    except Exception as e:
        raise HTTPException(status_code=401, detail=f"Authentication failed: {str(e)}")

# Ruta para obtener información del usuario
@app.get("/users/{username}")
async def get_user(username: str):
    try:
        # Obtener los roles desde PyIPA
        roles = ipa_manager.get_roles(username)
        if roles is None:
            raise HTTPException(status_code=404, detail="User not found")
        return {"username": username, "roles": roles}
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"Failed to retrieve user: {str(e)}")

# Ruta para asignar roles a un usuario
@app.post("/assign-role/{username}")
async def assign_role(username: str, role_assign: RoleAssign):
    try:
        ipa_manager.add_role(username, role_assign.role)
        return {"message": "Role assigned successfully"}
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"Failed to assign role: {str(e)}")

# Ruta para verificar un token
@app.post("/verify-token")
async def verify_token(token: str = Body(...)):
    try:
        is_valid = auth_manager.verify_token(token)
        return {"is_valid": is_valid}
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"Token verification failed: {str(e)}")

# Ruta para agregar un recurso
@app.post("/add-resource")
async def add_resource(resource: str, allowed_role: str):
    try:
        ipa_manager.add_resource(resource, allowed_role)
        return {"message": f"Resource {resource} added successfully for role {allowed_role}"}
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"Failed to add resource: {str(e)}")

# Ruta para listar recursos accesibles para un usuario
@app.get("/accessible-resources/{username}")
async def list_accessible_resources(username: str):
    try:
        resources = ipa_manager.list_accessible_resources(username)
        return {"username": username, "accessible_resources": resources}
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"Failed to list accessible resources: {str(e)}")