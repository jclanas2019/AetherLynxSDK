import requests
from langchain.llms import OpenAI
from langchain.prompts import PromptTemplate

# URLs de los sistemas transaccionales de la universidad
API_BASE_URL = "https://api.university.edu"
STUDENT_API = f"{API_BASE_URL}/students"
FINANCE_API = f"{API_BASE_URL}/finances"

# Obtener información del estudiante
def fetch_student_info(student_id):
    try:
        response = requests.get(f"{STUDENT_API}/{student_id}")
        response.raise_for_status()
        return response.json()
    except requests.exceptions.RequestException as e:
        return {"error": str(e)}

# Obtener el estado financiero del estudiante
def fetch_financial_status(student_id):
    try:
        response = requests.get(f"{FINANCE_API}/{student_id}")
        response.raise_for_status()
        return response.json()
    except requests.exceptions.RequestException as e:
        return {"error": str(e)}

# Generar la respuesta enriquecida
def generate_enriched_response(query, student_id):
    # Información del estudiante y financiera
    student_info = fetch_student_info(student_id)
    finance_info = fetch_financial_status(student_id)

    # Usar LLM para generar una respuesta enriquecida
    llm = OpenAI(model_name="gpt-4")
    prompt = PromptTemplate(input_variables=["input"], template="Responde lo siguiente: {input}")
    enriched_query = f"{query}\nInformación del Estudiante:\n{student_info}\nEstado Financiero:\n{finance_info}"
    
    response = llm(prompt.render(input=enriched_query))
    return response

if __name__ == "__main__":
    query = "¿Cuándo puedo inscribirme en los cursos para el próximo semestre?"
    student_id = "12345"
    response = generate_enriched_response(query, student_id)
    print(response)
