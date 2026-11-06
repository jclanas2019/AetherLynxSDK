//! Reglas de formato para nombres de usuario, contraseñas, roles y recursos.

pub const PASSWORD_MIN: usize = 12;
// Limitar la longitud evita usar Argon2 como vector de DoS con cuerpos enormes.
pub const PASSWORD_MAX: usize = 256;

/// Normaliza y valida un nombre de usuario: 3-64 caracteres de `[a-z0-9._@-]`,
/// en minúsculas para que "Admin" y "admin" no sean dos cuentas distintas.
pub fn username(raw: &str) -> Result<String, String> {
    let u = raw.trim().to_ascii_lowercase();
    if !(3..=64).contains(&u.len()) {
        return Err("el usuario debe tener entre 3 y 64 caracteres".into());
    }
    if !u.bytes().all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'@' | b'-')
    }) {
        return Err("el usuario solo admite letras, dígitos y . _ @ -".into());
    }
    Ok(u)
}

pub fn password(p: &str) -> Result<(), String> {
    let n = p.chars().count();
    if n < PASSWORD_MIN {
        return Err(format!(
            "la contraseña debe tener al menos {PASSWORD_MIN} caracteres"
        ));
    }
    if n > PASSWORD_MAX {
        return Err(format!(
            "la contraseña no puede superar {PASSWORD_MAX} caracteres"
        ));
    }
    Ok(())
}

/// Roles y recursos: 1-64 caracteres de `[a-z0-9:._-]`, en minúsculas.
pub fn label(kind: &str, raw: &str) -> Result<String, String> {
    let v = raw.trim().to_ascii_lowercase();
    if !(1..=64).contains(&v.len()) {
        return Err(format!("{kind}: entre 1 y 64 caracteres"));
    }
    if !v.bytes().all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b':' | b'.' | b'_' | b'-')
    }) {
        return Err(format!("{kind}: solo letras, dígitos y : . _ -"));
    }
    Ok(v)
}

/// Versión segura para el registro de auditoría de un nombre que puede ser
/// basura (intentos de login con usuarios inexistentes o malformados).
pub fn for_audit(raw: &str) -> String {
    raw.chars().filter(|c| !c.is_control()).take(64).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames() {
        assert_eq!(username(" Alice@Uni.edu ").unwrap(), "alice@uni.edu");
        assert!(username("ab").is_err());
        assert!(username("bad name").is_err());
        assert!(username("x\ny").is_err());
    }

    #[test]
    fn passwords() {
        assert!(password("short").is_err());
        assert!(password("correct horse battery").is_ok());
        assert!(password(&"a".repeat(PASSWORD_MAX + 1)).is_err());
    }

    #[test]
    fn labels() {
        assert_eq!(label("rol", "Admin").unwrap(), "admin");
        assert!(label("rol", "").is_err());
        assert!(label("rol", "a b").is_err());
    }

    #[test]
    fn audit_strips_control_chars() {
        assert_eq!(for_audit("a\nb\u{1b}[31m"), "ab[31m");
        assert_eq!(for_audit(&"x".repeat(500)).len(), 64);
    }
}
