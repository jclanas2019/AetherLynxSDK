use crate::integration::{get_student_info, get_financial_status};
use std::time::Duration;
use tokio::time;

pub struct UniversityController {
    // Aquí puedes definir configuraciones adicionales, como intervalos de monitoreo
}

impl UniversityController {
    pub async fn reconcile(&self) {
        loop {
            println!("Iniciando el ciclo de reconciliación...");

            // Lógica de monitoreo de los agentes
            match self.check_student_services().await {
                Ok(_) => println!("Servicios de estudiantes activos y funcionando."),
                Err(err) => println!("Error en los servicios de estudiantes: {}", err),
            };

            // Dormir por 10 segundos antes de la próxima reconciliación
            time::sleep(Duration::from_secs(10)).await;
        }
    }

    async fn check_student_services(&self) -> Result<(), String> {
        // Simula una comprobación de los servicios, integrando las APIs
        let student_id = "12345";

        let student_info = get_student_info(student_id).await.map_err(|e| e.to_string())?;
        let finance_info = get_financial_status(student_id).await.map_err(|e| e.to_string())?;

        if student_info.as_object().map_or(true, |obj| obj.is_empty()) || 
           finance_info.as_object().map_or(true, |obj| obj.is_empty()) {
            return Err("No se pudo recuperar la información del estudiante o financiera".into());
        }

        Ok(())
    }
}