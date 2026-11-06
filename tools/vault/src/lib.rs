//! Bindings de Python del vault de AetherLynx.

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};

pub mod vault;

use vault::{KdfParams, Vault, VaultError};

create_exception!(
    rust_vault,
    VaultError_,
    PyException,
    "Error base del vault."
);
create_exception!(
    rust_vault,
    InvalidInputError,
    VaultError_,
    "Entrada inválida."
);
create_exception!(
    rust_vault,
    WrongPassphraseError,
    VaultError_,
    "La passphrase no abre este vault."
);
create_exception!(
    rust_vault,
    SecretNotFoundError,
    VaultError_,
    "El secreto no existe."
);
create_exception!(
    rust_vault,
    LegacyVaultError,
    VaultError_,
    "Base de datos de la versión 0.1, indescifrable."
);
create_exception!(
    rust_vault,
    DecryptionError,
    VaultError_,
    "Datos corruptos o manipulados."
);
create_exception!(
    rust_vault,
    StaleVaultError,
    VaultError_,
    "Otro proceso cambió la passphrase; reabra el vault."
);

fn to_py(e: VaultError) -> PyErr {
    let msg = e.to_string();
    match e {
        VaultError::Invalid(_) => InvalidInputError::new_err(msg),
        VaultError::WrongPassphrase => WrongPassphraseError::new_err(msg),
        VaultError::NotFound => SecretNotFoundError::new_err(msg),
        VaultError::Legacy => LegacyVaultError::new_err(msg),
        VaultError::Crypto => DecryptionError::new_err(msg),
        VaultError::Stale => StaleVaultError::new_err(msg),
        VaultError::Db(_) => VaultError_::new_err(msg),
    }
}

const PASSPHRASE_ENV: &str = "AETHERLYNX_VAULT_PASSPHRASE";

#[pyclass(frozen)]
struct RustVault {
    inner: Vault,
}

#[pymethods]
impl RustVault {
    /// RustVault(db_path, passphrase, *, m_cost_kib=65536, t_cost=3, p_cost=1)
    ///
    /// Los parámetros de Argon2id solo se aplican al crear un vault nuevo.
    #[new]
    #[pyo3(signature = (db_path, passphrase, *, m_cost_kib = 65536, t_cost = 3, p_cost = 1))]
    fn new(
        py: Python<'_>,
        db_path: &str,
        passphrase: &str,
        m_cost_kib: u32,
        t_cost: u32,
        p_cost: u32,
    ) -> PyResult<Self> {
        let (path, pass) = (db_path.to_owned(), passphrase.to_owned());
        let params = KdfParams {
            m_cost_kib,
            t_cost,
            p_cost,
        };
        // La derivación tarda ~0,1-0,5 s: se libera el GIL mientras tanto.
        let inner = py
            .detach(move || Vault::open_with_params(&path, &pass, params))
            .map_err(to_py)?;
        Ok(Self { inner })
    }

    /// Abre el vault con la passphrase de la variable AETHERLYNX_VAULT_PASSPHRASE.
    #[staticmethod]
    fn from_env(py: Python<'_>, db_path: &str) -> PyResult<Self> {
        let pass = std::env::var(PASSPHRASE_ENV)
            .map_err(|_| InvalidInputError::new_err(format!("defina {PASSPHRASE_ENV}")))?;
        Self::new(py, db_path, &pass, 65536, 3, 1)
    }

    /// Crea o actualiza un secreto.
    fn add_secret(&self, name: &str, value: &str) -> PyResult<()> {
        self.inner.set_secret(name, value).map_err(to_py)
    }

    fn get_secret(&self, name: &str) -> PyResult<String> {
        self.inner.get_secret(name).map_err(to_py)
    }

    fn list_secrets(&self) -> PyResult<Vec<String>> {
        self.inner.list_secrets().map_err(to_py)
    }

    /// Devuelve True si el secreto existía.
    fn delete_secret(&self, name: &str) -> PyResult<bool> {
        self.inner.delete_secret(name).map_err(to_py)
    }

    #[pyo3(signature = (data, context = ""))]
    fn encrypt(&self, data: &[u8], context: &str) -> PyResult<String> {
        self.inner.encrypt(data, context).map_err(to_py)
    }

    #[pyo3(signature = (token, context = ""))]
    fn decrypt<'py>(
        &self,
        py: Python<'py>,
        token: &str,
        context: &str,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let plain = self.inner.decrypt(token, context).map_err(to_py)?;
        Ok(PyBytes::new(py, &plain))
    }

    fn change_passphrase(&self, py: Python<'_>, new_passphrase: &str) -> PyResult<()> {
        let pass = new_passphrase.to_owned();
        let inner = &self.inner;
        py.detach(|| inner.change_passphrase(&pass)).map_err(to_py)
    }

    /// Genera un certificado autofirmado ECDSA P-256.
    ///
    /// Devuelve {"certificate_pem", "private_key_pem"}. Con `store_key_as`,
    /// la clave privada se guarda además en el vault con ese nombre.
    #[pyo3(signature = (common_name, days = 365, store_key_as = None))]
    fn generate_certificate<'py>(
        &self,
        py: Python<'py>,
        common_name: &str,
        days: u32,
        store_key_as: Option<&str>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let (cert, key) = vault::generate_certificate(common_name, days).map_err(to_py)?;
        if let Some(name) = store_key_as {
            self.inner.set_secret(name, &key).map_err(to_py)?;
        }
        let d = PyDict::new(py);
        d.set_item("certificate_pem", cert)?;
        d.set_item("private_key_pem", key)?;
        Ok(d)
    }
}

#[pymodule]
fn rust_vault(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add_class::<RustVault>()?;
    m.add("VaultError", py.get_type::<VaultError_>())?;
    m.add("InvalidInputError", py.get_type::<InvalidInputError>())?;
    m.add(
        "WrongPassphraseError",
        py.get_type::<WrongPassphraseError>(),
    )?;
    m.add("SecretNotFoundError", py.get_type::<SecretNotFoundError>())?;
    m.add("LegacyVaultError", py.get_type::<LegacyVaultError>())?;
    m.add("DecryptionError", py.get_type::<DecryptionError>())?;
    m.add("StaleVaultError", py.get_type::<StaleVaultError>())?;
    Ok(())
}
