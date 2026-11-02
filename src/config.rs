//! Configuración persistente: credenciales OAuth y tokens de Google.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_SCOPE: &str = "https://www.googleapis.com/auth/drive";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    /// Segundos desde epoch. `None` significa "caducidad desconocida".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token_expiry: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Carpeta remota usada por `sync` cuando no se pasa `--remote`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_remote: Option<String>,
}

impl Config {
    pub fn has_refresh_flow(&self) -> bool {
        self.refresh_token.is_some() && self.client_id.is_some() && self.client_secret.is_some()
    }
}

pub fn default_config_path() -> Result<PathBuf> {
    if let Some(dir) = dirs::config_dir() {
        return Ok(dir.join("drivesync").join("config.json"));
    }
    bail!("no se pudo determinar el directorio de configuración; usa --config <archivo>")
}

pub fn load_config(path: Option<&Path>) -> Result<(Config, PathBuf)> {
    let path = match path {
        Some(p) => p.to_path_buf(),
        None => default_config_path()?,
    };
    if !path.exists() {
        return Ok((Config::default(), path));
    }
    let text = fs::read_to_string(&path)
        .with_context(|| format!("no se pudo leer la configuración {}", path.display()))?;
    let config: Config = serde_json::from_str(&text)
        .with_context(|| format!("configuración inválida en {}", path.display()))?;
    Ok((config, path))
}

pub fn save_config(config: &Config, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("no se pudo crear el directorio {}", parent.display()))?;
            restrict_permissions(parent, true)?;
        }
    }
    let json = serde_json::to_string_pretty(config)?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = create_private_file(&tmp)
            .with_context(|| format!("no se pudo escribir {}", tmp.display()))?;
        file.write_all(json.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("no se pudo guardar la configuración {}", path.display()))?;
    restrict_permissions(path, false)?;
    Ok(())
}

/// Aplica credenciales desde el entorno sin pisar lo que venga por CLI.
pub fn apply_env(config: &mut Config) {
    if config.client_id.is_none() {
        config.client_id = non_empty_env("GOOGLE_CLIENT_ID");
    }
    if config.client_secret.is_none() {
        config.client_secret = non_empty_env("GOOGLE_CLIENT_SECRET");
    }
    if config.refresh_token.is_none() {
        config.refresh_token = non_empty_env("GOOGLE_REFRESH_TOKEN");
    }
    if config.access_token.is_none() {
        config.access_token = non_empty_env("GOOGLE_ACCESS_TOKEN");
    }
}

pub fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<fs::File> {
    fs::File::create(path)
}

#[cfg(unix)]
fn restrict_permissions(path: &Path, is_dir: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if is_dir { 0o700 } else { 0o600 };
    let permissions = fs::Permissions::from_mode(mode);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("no se pudieron ajustar los permisos de {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path, _is_dir: bool) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ida_y_vuelta_json() {
        let config = Config {
            client_id: Some("id".into()),
            refresh_token: Some("refresh".into()),
            scope: Some(DEFAULT_SCOPE.into()),
            ..Default::default()
        };
        let text = serde_json::to_string(&config).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.client_id.as_deref(), Some("id"));
        assert_eq!(back.refresh_token.as_deref(), Some("refresh"));
        assert!(!back.has_refresh_flow(), "falta client_secret");
    }

    #[test]
    fn guarda_con_permisos_privados() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.json");
        save_config(&Config::default(), &path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains('{'));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
