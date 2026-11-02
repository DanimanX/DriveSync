//! Cliente de Google Drive v3 sobre HTTP (reqwest bloqueante).

use std::cell::RefCell;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, LOCATION, RANGE};
use reqwest::StatusCode;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};

use crate::auth::TokenProvider;
use crate::util;

pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const FILE_FIELDS: &str =
    "id,name,mimeType,md5Checksum,size,modifiedTime,parents,trashed,shortcutDetails";
const CHUNK_SIZE: u64 = 8 * 1024 * 1024;
const MULTIPART_LIMIT: u64 = 4 * 1024 * 1024;
const MAX_ATTEMPTS: u32 = 5;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteFile {
    pub id: String,
    pub name: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    #[serde(rename = "md5Checksum", default)]
    pub md5_checksum: Option<String>,
    #[serde(default, deserialize_with = "opt_u64_from_value")]
    pub size: Option<u64>,
    #[serde(rename = "modifiedTime", default)]
    pub modified_time: Option<String>,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default)]
    pub trashed: bool,
}

impl RemoteFile {
    pub fn is_folder(&self) -> bool {
        self.mime_type == FOLDER_MIME
    }

    pub fn is_google_native(&self) -> bool {
        self.mime_type.starts_with("application/vnd.google-apps.") && !self.is_folder()
    }

    pub fn is_shortcut(&self) -> bool {
        self.mime_type == "application/vnd.google-apps.shortcut"
    }

    pub fn is_syncable(&self) -> bool {
        !self.is_folder() && !self.is_google_native() && !self.is_shortcut()
    }

    pub fn size_or_zero(&self) -> u64 {
        self.size.unwrap_or(0)
    }
}

fn opt_u64_from_value<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(Value::String(text)) => text.parse().ok(),
        Some(Value::Number(number)) => number.as_u64(),
        _ => None,
    })
}

#[derive(Debug, Deserialize)]
struct FileList {
    #[serde(default)]
    files: Vec<RemoteFile>,
    #[serde(rename = "nextPageToken", default)]
    next_page_token: Option<String>,
}

pub struct DriveClient {
    http: Client,
    api: String,
    upload: String,
    tokens: RefCell<TokenProvider>,
    verbose: bool,
}

/// Totales de un subárbol remoto.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FolderSize {
    pub bytes: u64,
    pub folders: usize,
    pub files: usize,
    /// Documentos nativos de Google (sin tamaño reportado).
    pub native: usize,
    /// Otros elementos sin `size` (accesos directos, etc.).
    pub unknown: usize,
}

impl FolderSize {
    pub fn human(&self) -> String {
        util::format_size(self.bytes)
    }

    /// Aviso cuando el total no cubre todo el contenido.
    pub fn caveat(&self) -> Option<String> {
        if self.native > 0 {
            Some(format!("{} documento(s) de Google sin tamaño", self.native))
        } else if self.unknown > 0 {
            Some(format!("{} elemento(s) sin tamaño reportado", self.unknown))
        } else {
            None
        }
    }
}

impl DriveClient {
    pub fn new(tokens: TokenProvider, api_base: Option<String>, verbose: bool) -> Result<Self> {
        let base = api_base
            .or_else(|| std::env::var("DRIVESYNC_API_BASE").ok())
            .map(|b| b.trim().trim_end_matches('/').to_string())
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| "https://www.googleapis.com".to_string());

        let http = Client::builder()
            .user_agent(concat!("drivesync/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            // Sin límite práctico: las transferencias grandes necesitan tiempo.
            .timeout(Duration::from_secs(6 * 3600))
            .build()
            .context("no se pudo crear el cliente HTTP")?;

        Ok(Self {
            http,
            api: format!("{base}/drive/v3"),
            upload: format!("{base}/upload/drive/v3"),
            tokens: RefCell::new(tokens),
            verbose,
        })
    }

    pub fn auth_mode(&self) -> &'static str {
        self.tokens.borrow().auth_mode()
    }

    pub fn config(&self) -> crate::config::Config {
        self.tokens.borrow().config().clone()
    }

    /// Ejecuta la petición con reintentos, refresco de token y backoff.
    fn send<F>(&self, mut build: F) -> Result<Response>
    where
        F: FnMut() -> Result<RequestBuilder>,
    {
        let mut attempt = 1u32;
        let mut refreshed = false;

        loop {
            let token = self.tokens.borrow_mut().token()?;
            let request = build()?.bearer_auth(token);

            match request.send() {
                Ok(response) => {
                    let status = response.status();

                    if status == StatusCode::UNAUTHORIZED {
                        let can_refresh = self.tokens.borrow().can_refresh();
                        if can_refresh && !refreshed {
                            refreshed = true;
                            self.tokens.borrow_mut().refresh()?;
                            continue;
                        }
                    }

                    if is_retryable(status) && attempt < MAX_ATTEMPTS {
                        let wait = retry_delay(&response, attempt);
                        drop(response);
                        if self.verbose {
                            eprintln!(
                                "[reintento] HTTP {} (intento {}/{MAX_ATTEMPTS}), esperando {} s",
                                status.as_u16(),
                                attempt,
                                wait.as_secs_f32()
                            );
                        }
                        sleep(wait);
                        attempt += 1;
                        continue;
                    }

                    return Ok(response);
                }
                Err(error) => {
                    if attempt < MAX_ATTEMPTS && is_transient(&error) {
                        let wait = backoff(attempt);
                        if self.verbose {
                            eprintln!(
                                "[reintento] error de red (intento {attempt}/{MAX_ATTEMPTS}): {error}"
                            );
                        }
                        sleep(wait);
                        attempt += 1;
                        continue;
                    }
                    return Err(error).context("error de red al contactar Google Drive");
                }
            }
        }
    }

    fn ensure_ok(response: Response) -> Result<Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().unwrap_or_default();
        let message = extract_error_message(&body).unwrap_or_else(|| body.clone());

        if status == StatusCode::UNAUTHORIZED {
            bail!(
                "Google Drive rechazó el token (401): {message}. \
                 Ejecuta `drivesync auth` o pasa un token válido con --token"
            );
        }
        if status == StatusCode::NOT_FOUND {
            bail!("no se encontró el recurso en Google Drive (404): {message}");
        }
        if status == StatusCode::FORBIDDEN {
            bail!(
                "Google Drive denegó el acceso (403): {message}. \
                 Revisa el scope del token y los permisos del archivo"
            );
        }
        bail!("Google Drive respondió {}: {}", status.as_u16(), message)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.api, path)
    }

    pub fn root(&self) -> RemoteFile {
        RemoteFile {
            id: "root".to_string(),
            name: "Mi unidad".to_string(),
            mime_type: FOLDER_MIME.to_string(),
            ..Default::default()
        }
    }

    pub fn list_children(&self, folder_id: &str) -> Result<Vec<RemoteFile>> {
        let mut files = Vec::new();
        let mut page_token: Option<String> = None;
        let fields = format!("nextPageToken,files({FILE_FIELDS})");
        let query = format!(
            "'{}' in parents and trashed = false",
            util::escape_query(folder_id)
        );

        loop {
            let mut params: Vec<(String, String)> = vec![
                ("q".into(), query.clone()),
                ("fields".into(), fields.clone()),
                ("pageSize".into(), "1000".into()),
                ("supportsAllDrives".into(), "true".into()),
                ("includeItemsFromAllDrives".into(), "true".into()),
            ];
            if let Some(token) = &page_token {
                params.push(("pageToken".into(), token.clone()));
            }
            let url = self.url("/files");
            let response = self.send(|| Ok(self.http.get(&url).query(&params)))?;
            let response = Self::ensure_ok(response)?;
            let page: FileList = response
                .json()
                .context("respuesta JSON inválida al listar archivos")?;
            files.extend(page.files);
            match page.next_page_token {
                Some(token) if !token.is_empty() => page_token = Some(token),
                _ => break,
            }
        }
        Ok(files)
    }

    pub fn get_file(&self, file_id: &str) -> Result<RemoteFile> {
        let url = self.url(&format!("/files/{file_id}"));
        let params = vec![("fields", FILE_FIELDS), ("supportsAllDrives", "true")];
        let response = self.send(|| Ok(self.http.get(&url).query(&params)))?;
        let response = Self::ensure_ok(response)?;
        response
            .json()
            .with_context(|| format!("respuesta inválida al consultar el archivo {file_id}"))
    }

    pub fn find_child(&self, parent_id: &str, name: &str) -> Result<Option<RemoteFile>> {
        let children = self.list_children(parent_id)?;
        if let Some(found) = children.iter().find(|f| f.name == name) {
            return Ok(Some(found.clone()));
        }
        Ok(children
            .into_iter()
            .find(|f| f.name.eq_ignore_ascii_case(name)))
    }

    pub fn search_by_name(&self, name: &str) -> Result<Vec<RemoteFile>> {
        let query = format!("name = '{}' and trashed = false", util::escape_query(name));
        let fields = format!("files({FILE_FIELDS})");
        let url = self.url("/files");
        let params = vec![
            ("q".to_string(), query),
            ("fields".to_string(), fields),
            ("pageSize".to_string(), "50".to_string()),
            ("supportsAllDrives".to_string(), "true".to_string()),
            ("includeItemsFromAllDrives".to_string(), "true".to_string()),
        ];
        let response = self.send(|| Ok(self.http.get(&url).query(&params)))?;
        let response = Self::ensure_ok(response)?;
        let list: FileList = response
            .json()
            .context("respuesta inválida al buscar por nombre")?;
        Ok(list.files)
    }

    pub fn create_folder(&self, name: &str, parent_id: &str) -> Result<RemoteFile> {
        let url = self.url("/files");
        let params = vec![("fields", FILE_FIELDS), ("supportsAllDrives", "true")];
        let metadata = json!({
            "name": name,
            "mimeType": FOLDER_MIME,
            "parents": [parent_id],
        });
        let response = self.send(|| Ok(self.http.post(&url).query(&params).json(&metadata)))?;
        let response = Self::ensure_ok(response)?;
        response
            .json()
            .with_context(|| format!("respuesta inválida al crear la carpeta '{name}'"))
    }

    pub fn create_file(
        &self,
        parent_id: &str,
        name: &str,
        mime: &str,
        path: &Path,
    ) -> Result<RemoteFile> {
        let size = fs::metadata(path)
            .with_context(|| format!("no se pudo leer {}", path.display()))?
            .len();
        let metadata = json!({
            "name": name,
            "parents": [parent_id],
            "mimeType": mime,
        });

        if size <= MULTIPART_LIMIT {
            self.multipart_create(&metadata, mime, path)
        } else {
            self.resumable_upload(None, &metadata, mime, path, size)
        }
    }

    pub fn update_file(&self, file_id: &str, mime: &str, path: &Path) -> Result<RemoteFile> {
        let size = fs::metadata(path)
            .with_context(|| format!("no se pudo leer {}", path.display()))?
            .len();

        if size <= MULTIPART_LIMIT {
            self.media_update(file_id, mime, path)
        } else {
            let metadata = json!({});
            self.resumable_upload(Some(file_id), &metadata, mime, path, size)
        }
    }

    fn multipart_create(&self, metadata: &Value, mime: &str, path: &Path) -> Result<RemoteFile> {
        let boundary = format!("drivesync-{}", util::unique_id());
        let contents =
            fs::read(path).with_context(|| format!("no se pudo leer {}", path.display()))?;

        let mut body = Vec::with_capacity(contents.len() + 1024);
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(&serde_json::to_vec(metadata)?);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{boundary}\r\nContent-Type: {mime}\r\n\r\n").as_bytes());
        body.extend_from_slice(&contents);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let url = self.upload.clone() + "/files";
        let params = vec![
            ("uploadType", "multipart"),
            ("fields", FILE_FIELDS),
            ("supportsAllDrives", "true"),
        ];
        let content_type = format!("multipart/related; boundary={boundary}");

        let response = self.send(|| {
            Ok(self
                .http
                .post(&url)
                .query(&params)
                .header(CONTENT_TYPE, &content_type)
                .body(body.clone()))
        })?;
        let response = Self::ensure_ok(response)?;
        response
            .json()
            .context("respuesta inválida al subir el archivo")
    }

    fn media_update(&self, file_id: &str, mime: &str, path: &Path) -> Result<RemoteFile> {
        let contents =
            fs::read(path).with_context(|| format!("no se pudo leer {}", path.display()))?;
        let url = format!("{}/files/{file_id}", self.upload);
        let params = vec![
            ("uploadType", "media"),
            ("fields", FILE_FIELDS),
            ("supportsAllDrives", "true"),
        ];

        let response = self.send(|| {
            Ok(self
                .http
                .patch(&url)
                .query(&params)
                .header(CONTENT_TYPE, mime)
                .body(contents.clone()))
        })?;
        let response = Self::ensure_ok(response)?;
        response
            .json()
            .context("respuesta inválida al actualizar el archivo")
    }

    /// Carga reanudable por bloques (necesaria para archivos > 4 MiB).
    fn resumable_upload(
        &self,
        file_id: Option<&str>,
        metadata: &Value,
        mime: &str,
        path: &Path,
        size: u64,
    ) -> Result<RemoteFile> {
        let is_create = file_id.is_none();
        let url = match file_id {
            Some(id) => format!("{}/files/{id}", self.upload),
            None => format!("{}/files", self.upload),
        };
        let params = vec![
            ("uploadType".to_string(), "resumable".to_string()),
            ("fields".to_string(), FILE_FIELDS.to_string()),
            ("supportsAllDrives".to_string(), "true".to_string()),
        ];
        let metadata_bytes = serde_json::to_vec(metadata)?;
        let size_text = size.to_string();

        let response = self.send(|| {
            let builder = if is_create {
                self.http.post(&url)
            } else {
                self.http.patch(&url)
            };
            Ok(builder
                .query(&params)
                .header(CONTENT_TYPE, "application/json; charset=UTF-8")
                .header("X-Upload-Content-Type", mime)
                .header("X-Upload-Content-Length", &size_text)
                .body(metadata_bytes.clone()))
        })?;
        let response = Self::ensure_ok(response)?;
        let session_url = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
            .context("Google no devolvió la URL de la sesión de carga")?;

        let mut file =
            File::open(path).with_context(|| format!("no se pudo abrir {}", path.display()))?;
        let mut offset = 0u64;

        while offset < size {
            let len = std::cmp::min(CHUNK_SIZE, size - offset);
            let mut buffer = vec![0u8; len as usize];
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut buffer)?;

            let start = offset;
            let end = offset + len - 1;
            let session = session_url.clone();

            let response = self.send(|| {
                Ok(self
                    .http
                    .put(&session)
                    .header(CONTENT_RANGE, format!("bytes {start}-{end}/{size}"))
                    .header(CONTENT_LENGTH, len.to_string())
                    .header(CONTENT_TYPE, mime)
                    .body(buffer.clone()))
            })?;

            let status = response.status();
            if status.as_u16() == 308 {
                offset = end + 1;
                if let Some(range) = response
                    .headers()
                    .get(RANGE)
                    .and_then(|value| value.to_str().ok())
                {
                    if let Some(next) = parse_range_end(range) {
                        offset = next + 1;
                    }
                }
                continue;
            }

            let response = Self::ensure_ok(response)?;
            return response
                .json()
                .context("respuesta inválida al completar la carga");
        }

        bail!("la sesión de carga terminó sin confirmación de Google")
    }

    pub fn download_to(&self, file_id: &str, destination: &Path) -> Result<u64> {
        let url = self.url(&format!("/files/{file_id}"));
        let params = vec![("alt", "media"), ("supportsAllDrives", "true")];
        let mut response = self.send(|| Ok(self.http.get(&url).query(&params)))?;
        response = Self::ensure_ok(response)?;

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("no se pudo crear {}", parent.display()))?;
        }
        let file_name = destination
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("archivo");
        let temp_path = destination.with_file_name(format!(".{file_name}.drivesync-part"));

        let result = (|| -> Result<u64> {
            let mut out = File::create(&temp_path)
                .with_context(|| format!("no se pudo crear {}", temp_path.display()))?;
            let copied = io::copy(&mut response, &mut out)
                .context("error escribiendo el archivo descargado")?;
            out.flush()?;
            Ok(copied)
        })();

        match result {
            Ok(copied) => {
                fs::rename(&temp_path, destination).with_context(|| {
                    format!(
                        "no se pudo mover {} a {}",
                        temp_path.display(),
                        destination.display()
                    )
                })?;
                Ok(copied)
            }
            Err(err) => {
                let _ = fs::remove_file(&temp_path);
                Err(err)
            }
        }
    }

    pub fn trash_file(&self, file_id: &str) -> Result<()> {
        let url = self.url(&format!("/files/{file_id}"));
        let params = vec![("fields", "id,trashed"), ("supportsAllDrives", "true")];
        let response = self.send(|| {
            Ok(self
                .http
                .patch(&url)
                .query(&params)
                .json(&json!({"trashed": true})))
        })?;
        Self::ensure_ok(response)?;
        Ok(())
    }

    /// Suma recursivamente el tamaño de una carpeta.
    ///
    /// Los documentos nativos de Google no reportan tamaño (no tienen
    /// contenido binario descargable), así que se cuentan aparte para poder
    /// avisar de que el total no los incluye.
    pub fn folder_size(&self, folder_id: &str) -> Result<FolderSize> {
        let mut size = FolderSize::default();
        let mut stack = vec![folder_id.to_string()];

        while let Some(current) = stack.pop() {
            for child in self.list_children(&current)? {
                if child.is_folder() {
                    size.folders += 1;
                    stack.push(child.id);
                } else if child.is_google_native() {
                    size.native += 1;
                } else if let Some(bytes) = child.size {
                    size.bytes = size.bytes.saturating_add(bytes);
                    size.files += 1;
                } else {
                    size.unknown += 1;
                }
            }
        }
        Ok(size)
    }

    pub fn about(&self) -> Result<Value> {
        let url = self.url("/about");
        let params = vec![(
            "fields",
            "user(displayName,emailAddress),storageQuota(limit,usage)",
        )];
        let response = self.send(|| Ok(self.http.get(&url).query(&params)))?;
        let response = Self::ensure_ok(response)?;
        response.json().context("respuesta inválida de /about")
    }

    /// Acepta un ID de Drive, una ruta `Carpeta/Sub/…` o un nombre.
    pub fn resolve_ref(&self, reference: &str) -> Result<RemoteFile> {
        let reference = reference.trim();
        if reference.is_empty()
            || reference == "/"
            || reference.eq_ignore_ascii_case("root")
            || reference.eq_ignore_ascii_case("mi unidad")
            || reference.eq_ignore_ascii_case("my drive")
        {
            return Ok(self.root());
        }
        if reference.contains('/') || reference.contains('\\') {
            match self.resolve_path(reference) {
                Ok(found) => return Ok(found),
                Err(error_ruta) => {
                    // Puede ser un nombre que contiene una barra: se intenta tal cual.
                    if let Some(found) = self.find_child("root", reference)? {
                        return Ok(found);
                    }
                    return Err(error_ruta);
                }
            }
        }
        if looks_like_id(reference) {
            if let Ok(found) = self.get_file(reference) {
                return Ok(found);
            }
        }
        if let Some(found) = self.find_child("root", reference)? {
            return Ok(found);
        }
        let mut matches = self.search_by_name(reference)?;
        match matches.len() {
            0 => bail!(
                "no se encontró '{reference}' en Google Drive (ni como ID, ni como ruta, ni como nombre)"
            ),
            1 => Ok(matches.remove(0)),
            _ => {
                let chosen = matches.remove(0);
                eprintln!(
                    "[aviso] hay {} elementos llamados '{reference}'; se usa el primero ({}). \
                     Para elegir otro, pasa su ID.",
                    matches.len() + 1,
                    chosen.id
                );
                Ok(chosen)
            }
        }
    }

    pub fn resolve_path(&self, path: &str) -> Result<RemoteFile> {
        let mut current = self.root();
        for part in util::split_folder_path(path)? {
            let children = self.list_children(&current.id)?;
            let found = children
                .iter()
                .find(|f| f.name == part)
                .or_else(|| children.iter().find(|f| f.name.eq_ignore_ascii_case(&part)));
            match found {
                Some(file) => current = file.clone(),
                None => bail!(
                    "no se encontró la carpeta '{part}' dentro de '{}'",
                    current.name
                ),
            }
        }
        Ok(current)
    }
}

fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::INTERNAL_SERVER_ERROR
        || status == StatusCode::BAD_GATEWAY
        || status == StatusCode::SERVICE_UNAVAILABLE
        || status == StatusCode::GATEWAY_TIMEOUT
}

fn is_transient(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request()
}

fn retry_delay(response: &Response, attempt: u32) -> Duration {
    if let Some(seconds) = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
    {
        return Duration::from_secs(seconds.min(60));
    }
    backoff(attempt)
}

fn backoff(attempt: u32) -> Duration {
    let millis = 500u64.saturating_mul(1u64 << attempt.min(6));
    Duration::from_millis(millis.min(30_000))
}

fn extract_error_message(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    if let Some(message) = error.get("message").and_then(|m| m.as_str()) {
        return Some(message.to_string());
    }
    error.as_str().map(str::to_string)
}

pub fn parse_range_end(range: &str) -> Option<u64> {
    let (_, end) = range.split_once('-')?;
    end.trim().parse().ok()
}

pub fn looks_like_id(value: &str) -> bool {
    value.len() >= 20
        && !value.contains(char::is_whitespace)
        && value.contains(|c: char| c.is_ascii_alphanumeric())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detecta_ids() {
        assert!(looks_like_id("1AbCdEfGhIjKlMnOpQrStUvWxYz0123456"));
        assert!(!looks_like_id("Carpeta"));
        assert!(!looks_like_id("Mi unidad"));
        assert!(!looks_like_id("carpeta/sub"));
    }

    #[test]
    fn lee_rango_de_sesion() {
        assert_eq!(parse_range_end("bytes=0-1023"), Some(1023));
        assert_eq!(parse_range_end("bytes=0-0"), Some(0));
        assert_eq!(parse_range_end("basura"), None);
    }

    #[test]
    fn distingue_tipos_de_archivo() {
        let folder = RemoteFile {
            mime_type: FOLDER_MIME.into(),
            ..Default::default()
        };
        assert!(folder.is_folder());
        assert!(!folder.is_google_native());

        let doc = RemoteFile {
            mime_type: "application/vnd.google-apps.document".into(),
            ..Default::default()
        };
        assert!(doc.is_google_native());
        assert!(!doc.is_syncable());

        let bin = RemoteFile {
            mime_type: "application/pdf".into(),
            md5_checksum: Some("abc".into()),
            ..Default::default()
        };
        assert!(bin.is_syncable());
    }

    #[test]
    fn tamano_desde_string() {
        let json = r#"{"id":"1","name":"a","mimeType":"text/plain","size":"1234"}"#;
        let file: RemoteFile = serde_json::from_str(json).unwrap();
        assert_eq!(file.size, Some(1234));
    }

    #[test]
    fn extrae_mensaje_de_error() {
        let body =
            r#"{"error":{"code":403,"message":"The user does not have sufficient permissions"}}"#;
        assert_eq!(
            extract_error_message(body).as_deref(),
            Some("The user does not have sufficient permissions")
        );
    }
}
