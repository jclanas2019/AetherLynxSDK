//! Validación de entradas del API. Todo lo que llega por la red pasa por aquí
//! antes de tocar el estado o los logs.

/// Máximo de réplicas por agente.
pub const MAX_REPLICAS: u32 = 100;
/// Máximo de agentes registrados.
pub const MAX_AGENTS: usize = 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError(pub String);

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ValidationError {}

/// Nombre de agente: etiqueta DNS-1123 (como los nombres de Kubernetes).
/// Minúsculas, dígitos y guiones; 1-63 caracteres; empieza y termina en alfanumérico.
pub fn agent_name(name: &str) -> Result<(), ValidationError> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 63 {
        return Err(ValidationError(
            "el nombre debe tener entre 1 y 63 caracteres".into(),
        ));
    }
    let ok_char = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-';
    if !bytes.iter().all(ok_char) {
        return Err(ValidationError(
            "el nombre solo admite minúsculas, dígitos y '-'".into(),
        ));
    }
    if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
        return Err(ValidationError(
            "el nombre debe empezar y terminar con letra o dígito".into(),
        ));
    }
    Ok(())
}

/// Identificador de modelo, p. ej. `gpt-4o`, `claude-sonnet-4.5`, `org/model:tag`.
pub fn model_id(model: &str) -> Result<(), ValidationError> {
    if model.is_empty() || model.len() > 128 {
        return Err(ValidationError(
            "el modelo debe tener entre 1 y 128 caracteres".into(),
        ));
    }
    let ok = model
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b':' | b'/'));
    if !ok {
        return Err(ValidationError(
            "el modelo solo admite letras, dígitos y - _ . : /".into(),
        ));
    }
    Ok(())
}

pub fn replicas(n: u32) -> Result<(), ValidationError> {
    if n > MAX_REPLICAS {
        return Err(ValidationError(format!(
            "réplicas fuera de rango (0-{MAX_REPLICAS})"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names() {
        for n in ["a", "student-agent", "agent-01", &"a".repeat(63)] {
            assert!(agent_name(n).is_ok(), "{n}");
        }
    }

    #[test]
    fn invalid_names() {
        for n in [
            "",
            "-a",
            "a-",
            "Upper",
            "with space",
            "line\nbreak",
            "x\u{0}",
            "ñ",
            &"a".repeat(64),
        ] {
            assert!(agent_name(n).is_err(), "{n:?}");
        }
    }

    #[test]
    fn models() {
        assert!(model_id("gpt-4o").is_ok());
        assert!(model_id("meta-llama/Llama-3.1-8B:instruct").is_ok());
        assert!(model_id("").is_err());
        assert!(model_id("bad\nmodel").is_err());
        assert!(model_id("rm -rf").is_err());
    }

    #[test]
    fn replica_bounds() {
        assert!(replicas(0).is_ok());
        assert!(replicas(MAX_REPLICAS).is_ok());
        assert!(replicas(MAX_REPLICAS + 1).is_err());
    }
}
