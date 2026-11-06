//! Autenticación del API gRPC con token bearer (`authorization: Bearer <token>`).
//! La comparación es de tiempo constante para no filtrar el token por timing.

use std::sync::Arc;

use subtle::ConstantTimeEq;
use tonic::{service::Interceptor, Request, Status};

pub const MIN_TOKEN_LEN: usize = 32;

#[derive(Clone)]
pub struct TokenAuth {
    token: Option<Arc<[u8]>>,
}

impl TokenAuth {
    /// Exige el token dado. Debe tener al menos [`MIN_TOKEN_LEN`] caracteres.
    pub fn required(token: &str) -> Result<Self, String> {
        if token.len() < MIN_TOKEN_LEN {
            return Err(format!(
                "el token del API debe tener al menos {MIN_TOKEN_LEN} caracteres"
            ));
        }
        Ok(Self {
            token: Some(Arc::from(token.as_bytes())),
        })
    }

    /// Sin autenticación. Solo debe usarse en loopback para desarrollo.
    pub fn disabled() -> Self {
        Self { token: None }
    }

    pub fn is_enabled(&self) -> bool {
        self.token.is_some()
    }
}

impl Interceptor for TokenAuth {
    fn call(&mut self, req: Request<()>) -> Result<Request<()>, Status> {
        let Some(expected) = &self.token else {
            return Ok(req);
        };
        let presented = req
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| {
                crate::telemetry::metrics().auth_rejected.add(1, &[]);
                Status::unauthenticated("falta el token de acceso")
            })?;
        if bool::from(presented.as_bytes().ct_eq(expected)) {
            Ok(req)
        } else {
            crate::telemetry::metrics().auth_rejected.add(1, &[]);
            tracing::warn!("petición gRPC rechazada: token inválido");
            Err(Status::unauthenticated("token de acceso inválido"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn req(auth: Option<&str>) -> Request<()> {
        let mut r = Request::new(());
        if let Some(a) = auth {
            r.metadata_mut().insert("authorization", a.parse().unwrap());
        }
        r
    }

    #[test]
    fn short_tokens_are_rejected_at_startup() {
        assert!(TokenAuth::required("short").is_err());
    }

    #[test]
    fn accepts_only_the_right_token() {
        let mut a = TokenAuth::required(TOKEN).unwrap();
        assert!(a.call(req(Some(&format!("Bearer {TOKEN}")))).is_ok());
        assert_eq!(
            a.call(req(None)).unwrap_err().code(),
            tonic::Code::Unauthenticated
        );
        assert!(a.call(req(Some("Bearer wrong"))).is_err());
        assert!(a.call(req(Some(TOKEN))).is_err(), "sin prefijo Bearer");
    }

    #[test]
    fn disabled_lets_everything_through() {
        let mut a = TokenAuth::disabled();
        assert!(a.call(req(None)).is_ok());
    }
}
