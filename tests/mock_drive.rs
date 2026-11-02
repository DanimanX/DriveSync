//! Prueba de extremo a extremo del binario contra un Drive simulado en memoria.
//!
//! Se ejecuta el CLI real (`CARGO_BIN_EXE_drivesync`) apuntando con
//! `DRIVESYNC_API_BASE` a un servidor HTTP local que implementa el subconjunto
//! de la API de Drive usado por la herramienta.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use md5::{Digest, Md5};
use percent_encoding::percent_decode_str;
use serde_json::{json, Value};

const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

fn md5_hex(bytes: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Clone)]
struct MockFile {
    id: String,
    name: String,
    parent: String,
    mime: String,
    content: Vec<u8>,
    trashed: bool,
}

#[derive(Clone)]
struct Session {
    target: Option<String>,
    name: String,
    parent: String,
    mime: String,
    buffer: Vec<u8>,
}

#[derive(Default)]
struct State {
    files: HashMap<String, MockFile>,
    sessions: HashMap<String, Session>,
    next_file: u64,
    next_session: u64,
    /// Cuántos bloques de subida responderán 408 antes de aceptarse.
    chunk_faults: usize,
}

impl State {
    fn new_id(&mut self) -> String {
        self.next_file += 1;
        format!("file{:04}", self.next_file)
    }

    fn create(&mut self, name: &str, parent: &str, mime: &str, content: Vec<u8>) -> MockFile {
        let id = self.new_id();
        let file = MockFile {
            id: id.clone(),
            name: name.to_string(),
            parent: parent.to_string(),
            mime: mime.to_string(),
            content,
            trashed: false,
        };
        self.files.insert(id, file.clone());
        file
    }
}

struct MockDrive {
    base: String,
    state: Arc<Mutex<State>>,
}

impl MockDrive {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("no se pudo abrir el puerto");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        let server_base = base.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&shared);
                let base = server_base.clone();
                thread::spawn(move || {
                    let _ = handle_connection(stream, state, base);
                });
            }
        });
        Self { base, state }
    }

    fn base(&self) -> &str {
        &self.base
    }

    fn add_folder(&self, name: &str, parent: &str) -> String {
        let mut state = self.state.lock().unwrap();
        state.create(name, parent, FOLDER_MIME, Vec::new()).id
    }

    /// Hace que los próximos `n` bloques de subida fallen con 408 (timeout del
    /// frontend, como en los archivos grandes reales).
    fn fail_chunks(&self, n: usize) {
        self.state.lock().unwrap().chunk_faults = n;
    }

    fn add_native(&self, name: &str, parent: &str) -> String {
        let mut state = self.state.lock().unwrap();
        state
            .create(
                name,
                parent,
                "application/vnd.google-apps.document",
                Vec::new(),
            )
            .id
    }

    fn add_file(&self, name: &str, parent: &str, content: &[u8]) -> String {
        let mut state = self.state.lock().unwrap();
        state
            .create(name, parent, "application/octet-stream", content.to_vec())
            .id
    }

    fn set_content(&self, name: &str, content: &[u8]) {
        let mut state = self.state.lock().unwrap();
        for file in state.files.values_mut() {
            if file.name == name && !file.trashed {
                file.content = content.to_vec();
                return;
            }
        }
        panic!("no existe el archivo remoto '{name}'");
    }

    fn content_by_name(&self, name: &str) -> Option<Vec<u8>> {
        let state = self.state.lock().unwrap();
        state
            .files
            .values()
            .find(|file| file.name == name && !file.trashed)
            .map(|file| file.content.clone())
    }

    fn children(&self, parent: &str) -> Vec<MockFile> {
        let state = self.state.lock().unwrap();
        let mut files: Vec<MockFile> = state
            .files
            .values()
            .filter(|file| file.parent == parent && !file.trashed)
            .cloned()
            .collect();
        files.sort_by(|a, b| a.name.cmp(&b.name));
        files
    }

    fn is_trashed(&self, name: &str) -> bool {
        let state = self.state.lock().unwrap();
        state
            .files
            .values()
            .any(|file| file.name == name && file.trashed)
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn json(status: u16, value: &Value) -> Self {
        Self {
            status,
            headers: vec![(
                "Content-Type".into(),
                "application/json; charset=utf-8".into(),
            )],
            body: serde_json::to_vec(value).unwrap(),
        }
    }

    fn error(status: u16, message: &str) -> Self {
        Self::json(
            status,
            &json!({"error": {"code": status, "message": message}}),
        )
    }
}

fn handle_connection(
    stream: TcpStream,
    state: Arc<Mutex<State>>,
    base: String,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    while let Some(request) = read_request(&mut reader) {
        let response = route(request, &base, &state);
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n",
            response.status,
            reason(response.status),
            response.body.len()
        );
        for (name, value) in &response.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        writer.write_all(head.as_bytes())?;
        writer.write_all(&response.body)?;
        writer.flush()?;
    }
    Ok(())
}

fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Request> {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body).ok()?;
    }

    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    Some(Request {
        method,
        path: path.to_string(),
        query: query.to_string(),
        headers,
        body,
    })
}

fn route(request: Request, base: &str, state: &Arc<Mutex<State>>) -> HttpResponse {
    let params = parse_query(&request.query);
    let path = request.path.as_str();

    match request.method.as_str() {
        "POST" if path == "/token" => token_endpoint(&request),
        "GET" if path == "/drive/v3/about" => HttpResponse::json(
            200,
            &json!({
                "user": {"displayName": "Cuenta de Prueba", "emailAddress": "prueba@example.com"},
                "storageQuota": {"limit": "1073741824", "usage": "2048"}
            }),
        ),
        "GET" if path == "/drive/v3/files" => list_files(&params, state),
        "POST" if path == "/drive/v3/files" => create_metadata(&request, state),
        "POST" | "PATCH" if path.starts_with("/upload/drive/v3/files") => {
            let target = path
                .strip_prefix("/upload/drive/v3/files/")
                .filter(|rest| !rest.is_empty())
                .map(str::to_string);
            match params.get("uploadType").map(String::as_str) {
                Some("multipart") => multipart_create(&request, state),
                Some("media") => media_update(target.as_deref(), &request, state),
                Some("resumable") => start_session(&request, target.as_deref(), base, state),
                _ => HttpResponse::error(400, "uploadType no soportado"),
            }
        }
        "GET" if path.starts_with("/drive/v3/files/") => {
            let id = path.trim_start_matches("/drive/v3/files/");
            if params.get("alt").map(String::as_str) == Some("media") {
                download(id, state)
            } else {
                metadata(id, state)
            }
        }
        "PATCH" if path.starts_with("/drive/v3/files/") => {
            let id = path.trim_start_matches("/drive/v3/files/");
            patch_metadata(id, &request, state)
        }
        "PUT" if path.starts_with("/upload/session/") => {
            let id = path.trim_start_matches("/upload/session/");
            upload_chunk(id, &request, state)
        }
        _ => HttpResponse::error(
            404,
            &format!("ruta no implementada: {} {}", request.method, path),
        ),
    }
}

fn token_endpoint(request: &Request) -> HttpResponse {
    let params = parse_query(std::str::from_utf8(&request.body).unwrap_or(""));

    // Simula la comprobación de --check-client.
    if params.get("client_id").map(String::as_str) == Some("cliente-desconocido") {
        return HttpResponse::json(
            401,
            &json!({"error": "invalid_client", "error_description": "The OAuth client was not found."}),
        );
    }
    if params.get("code").map(String::as_str) == Some("codigo-de-prueba-drivesync") {
        return HttpResponse::json(
            400,
            &json!({"error": "invalid_grant", "error_description": "Bad Request"}),
        );
    }

    match params.get("grant_type").map(String::as_str) {
        Some("refresh_token") | Some("authorization_code") => HttpResponse::json(
            200,
            &json!({
                "access_token": "token-renovado",
                "expires_in": 3600,
                "scope": "https://www.googleapis.com/auth/drive",
                "token_type": "Bearer",
                "refresh_token": "refresh-nuevo"
            }),
        ),
        _ => HttpResponse::error(400, "grant_type no soportado"),
    }
}

fn list_files(params: &HashMap<String, String>, state: &Arc<Mutex<State>>) -> HttpResponse {
    let query = params.get("q").cloned().unwrap_or_default();
    let state = state.lock().unwrap();
    let mut files: Vec<MockFile> = state
        .files
        .values()
        .filter(|file| !file.trashed)
        .cloned()
        .collect();

    if query.contains(" in parents") {
        if let Some(parent) = first_quoted(&query) {
            files.retain(|file| file.parent == parent);
        }
    } else if query.starts_with("name = ") {
        if let Some(name) = first_quoted(&query) {
            files.retain(|file| file.name == name);
        }
    }
    files.sort_by(|a, b| a.id.cmp(&b.id));

    HttpResponse::json(
        200,
        &json!({"files": files.iter().map(file_json).collect::<Vec<_>>()}),
    )
}

fn create_metadata(request: &Request, state: &Arc<Mutex<State>>) -> HttpResponse {
    let metadata: Value = match serde_json::from_slice(&request.body) {
        Ok(value) => value,
        Err(_) => return HttpResponse::error(400, "JSON inválido"),
    };
    let name = metadata
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("sin-nombre");
    let parent = metadata
        .get("parents")
        .and_then(Value::as_array)
        .and_then(|parents| parents.first())
        .and_then(Value::as_str)
        .unwrap_or("root");
    let mime = metadata
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream");

    let mut state = state.lock().unwrap();
    let file = state.create(name, parent, mime, Vec::new());
    HttpResponse::json(200, &file_json(&file))
}

fn multipart_create(request: &Request, state: &Arc<Mutex<State>>) -> HttpResponse {
    let boundary = request
        .headers
        .get("content-type")
        .and_then(|value| value.split("boundary=").nth(1))
        .map(|value| value.trim_matches('"').to_string());
    let Some(boundary) = boundary else {
        return HttpResponse::error(400, "falta el boundary multipart");
    };
    let Some((metadata, content)) = parse_multipart(&request.body, &boundary) else {
        return HttpResponse::error(400, "multipart inválido");
    };

    let name = metadata
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("sin-nombre");
    let parent = metadata
        .get("parents")
        .and_then(Value::as_array)
        .and_then(|parents| parents.first())
        .and_then(Value::as_str)
        .unwrap_or("root");
    let mime = metadata
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream");

    let mut state = state.lock().unwrap();
    let file = state.create(name, parent, mime, content);
    HttpResponse::json(200, &file_json(&file))
}

fn media_update(
    target: Option<&str>,
    request: &Request,
    state: &Arc<Mutex<State>>,
) -> HttpResponse {
    let Some(id) = target else {
        return HttpResponse::error(400, "falta el ID del archivo a actualizar");
    };
    let mut state = state.lock().unwrap();
    match state.files.get_mut(id) {
        Some(file) => {
            file.content = request.body.clone();
            let response = file_json(file);
            HttpResponse::json(200, &response)
        }
        None => HttpResponse::error(404, "archivo no encontrado"),
    }
}

fn start_session(
    request: &Request,
    target: Option<&str>,
    base: &str,
    state: &Arc<Mutex<State>>,
) -> HttpResponse {
    let metadata: Value = serde_json::from_slice(&request.body).unwrap_or_else(|_| json!({}));
    let (name, parent, mime) = {
        let state = state.lock().unwrap();
        match target {
            Some(id) => match state.files.get(id) {
                Some(file) => (file.name.clone(), file.parent.clone(), file.mime.clone()),
                None => return HttpResponse::error(404, "archivo no encontrado"),
            },
            None => (
                metadata
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("sin-nombre")
                    .to_string(),
                metadata
                    .get("parents")
                    .and_then(Value::as_array)
                    .and_then(|parents| parents.first())
                    .and_then(Value::as_str)
                    .unwrap_or("root")
                    .to_string(),
                metadata
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("application/octet-stream")
                    .to_string(),
            ),
        }
    };

    let mut state = state.lock().unwrap();
    state.next_session += 1;
    let session_id = format!("session{}", state.next_session);
    state.sessions.insert(
        session_id.clone(),
        Session {
            target: target.map(str::to_string),
            name,
            parent,
            mime,
            buffer: Vec::new(),
        },
    );

    HttpResponse {
        status: 200,
        headers: vec![(
            "Location".to_string(),
            format!("{base}/upload/session/{session_id}"),
        )],
        body: b"{}".to_vec(),
    }
}

/// Página HTML como la que devuelve el frontend de Google en un 408.
fn timeout_page() -> HttpResponse {
    HttpResponse {
        status: 408,
        headers: vec![
            ("Content-Type".to_string(), "text/html; charset=utf-8".to_string()),
            ("Retry-After".to_string(), "0".to_string()),
        ],
        body: b"<!DOCTYPE html><html lang=en><title>Error 408 (Request Timeout)!!1</title><p><b>408.</b> <ins>That\xe2\x80\x99s an error.</ins><p>Your client has taken too long to issue its request.</html>"
            .to_vec(),
    }
}

fn upload_chunk(id: &str, request: &Request, state: &Arc<Mutex<State>>) -> HttpResponse {
    let Some(range) = request.headers.get("content-range") else {
        return HttpResponse::error(400, "falta Content-Range");
    };

    // Consulta de progreso: `Content-Range: bytes */TOTAL`, cuerpo vacío.
    if let Some(total) = range.trim().strip_prefix("bytes */") {
        let total: u64 = total.trim().parse().unwrap_or(0);
        let state = state.lock().unwrap();
        let Some(session) = state.sessions.get(id) else {
            return HttpResponse::error(404, "sesión de carga desconocida");
        };
        let recibido = session.buffer.len() as u64;
        if total > 0 && recibido >= total {
            return HttpResponse::error(409, "la sesión ya se completó");
        }
        return if recibido == 0 {
            HttpResponse {
                status: 308,
                headers: Vec::new(),
                body: Vec::new(),
            }
        } else {
            HttpResponse {
                status: 308,
                headers: vec![("Range".to_string(), format!("bytes=0-{}", recibido - 1))],
                body: Vec::new(),
            }
        };
    }

    // Inyección de fallos: el bloque se rechaza sin tocar la sesión.
    {
        let mut guard = state.lock().unwrap();
        if guard.chunk_faults > 0 {
            guard.chunk_faults -= 1;
            return timeout_page();
        }
    }

    let Some((start, end, total)) = parse_content_range(range) else {
        return HttpResponse::error(400, "Content-Range inválido");
    };
    if request.body.len() as u64 != end - start + 1 {
        return HttpResponse::error(400, "el cuerpo no coincide con Content-Range");
    }

    let mut state = state.lock().unwrap();
    let Some(mut session) = state.sessions.remove(id) else {
        return HttpResponse::error(404, "sesión de carga desconocida");
    };
    if start != session.buffer.len() as u64 {
        state.sessions.insert(id.to_string(), session);
        return HttpResponse::error(400, "bloque fuera de orden");
    }
    session.buffer.extend_from_slice(&request.body);

    let complete = session.buffer.len() as u64 >= total;
    if !complete {
        let length = session.buffer.len() as u64;
        state.sessions.insert(id.to_string(), session);
        return HttpResponse {
            status: 308,
            headers: vec![("Range".to_string(), format!("bytes=0-{}", length - 1))],
            body: Vec::new(),
        };
    }

    let content = session.buffer;
    let file = match session.target {
        Some(file_id) => {
            let Some(file) = state.files.get_mut(&file_id) else {
                return HttpResponse::error(404, "archivo no encontrado");
            };
            file.content = content;
            file.clone()
        }
        None => state.create(&session.name, &session.parent, &session.mime, content),
    };
    HttpResponse::json(200, &file_json(&file))
}

fn metadata(id: &str, state: &Arc<Mutex<State>>) -> HttpResponse {
    let state = state.lock().unwrap();
    match state.files.get(id) {
        Some(file) => HttpResponse::json(200, &file_json(file)),
        None => HttpResponse::error(404, "archivo no encontrado"),
    }
}

fn download(id: &str, state: &Arc<Mutex<State>>) -> HttpResponse {
    let state = state.lock().unwrap();
    match state.files.get(id) {
        Some(file) => HttpResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), file.mime.clone())],
            body: file.content.clone(),
        },
        None => HttpResponse::error(404, "archivo no encontrado"),
    }
}

fn patch_metadata(id: &str, request: &Request, state: &Arc<Mutex<State>>) -> HttpResponse {
    let metadata: Value = serde_json::from_slice(&request.body).unwrap_or_else(|_| json!({}));
    let mut state = state.lock().unwrap();
    match state.files.get_mut(id) {
        Some(file) => {
            if metadata.get("trashed").and_then(Value::as_bool) == Some(true) {
                file.trashed = true;
            }
            let response = file_json(file);
            HttpResponse::json(200, &response)
        }
        None => HttpResponse::error(404, "archivo no encontrado"),
    }
}

fn file_json(file: &MockFile) -> Value {
    json!({
        "id": file.id,
        "name": file.name,
        "mimeType": file.mime,
        "md5Checksum": md5_hex(&file.content),
        "size": file.content.len().to_string(),
        "modifiedTime": "2026-11-02T10:00:00.000Z",
        "parents": [file.parent],
        "trashed": file.trashed,
    })
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        // `serde_urlencoded` codifica los espacios como '+'.
        let decode = |text: &str| {
            percent_decode_str(&text.replace('+', " "))
                .decode_utf8_lossy()
                .into_owned()
        };
        params.insert(decode(key), decode(value));
    }
    params
}

fn first_quoted(query: &str) -> Option<String> {
    let start = query.find('\'')? + 1;
    let mut out = String::new();
    let mut chars = query[start..].chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '\'' => return Some(out),
            other => out.push(other),
        }
    }
    None
}

fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some((
        start.trim().parse().ok()?,
        end.trim().parse().ok()?,
        total.trim().parse().ok()?,
    ))
}

fn parse_multipart(body: &[u8], boundary: &str) -> Option<(Value, Vec<u8>)> {
    let delimiter = format!("--{boundary}");
    let mut metadata = None;
    let mut content = None;

    for part in split_bytes(body, delimiter.as_bytes()) {
        let part = part.strip_prefix(b"\r\n").unwrap_or(part);
        let part = part.strip_suffix(b"\r\n").unwrap_or(part);
        let Some(index) = find_bytes(part, b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&part[..index]).to_ascii_lowercase();
        let payload = &part[index + 4..];
        if headers.contains("application/json") {
            metadata = serde_json::from_slice(payload).ok();
        } else if headers.contains("content-type") {
            content = Some(payload.to_vec());
        }
    }
    Some((metadata?, content?))
}

fn split_bytes<'a>(haystack: &'a [u8], needle: &[u8]) -> Vec<&'a [u8]> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index + needle.len() <= haystack.len() {
        if &haystack[index..index + needle.len()] == needle {
            parts.push(&haystack[start..index]);
            index += needle.len();
            start = index;
        } else {
            index += 1;
        }
    }
    parts.push(&haystack[start..]);
    parts
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        308 => "Resume Incomplete",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

fn run_cli(base: &str, config: &Path, args: &[&str]) -> Output {
    run_cli_inner(base, config, args, Some("token-de-prueba"))
}

fn run_cli_without_token(base: &str, config: &Path, args: &[&str]) -> Output {
    run_cli_inner(base, config, args, None)
}

fn run_cli_with_stdin(base: &str, config: &Path, args: &[&str], stdin: &str) -> Output {
    use std::process::Stdio;
    let mut child = command_for(base, config, None)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("no se pudo ejecutar drivesync");
    {
        let mut pipe = child.stdin.take().expect("stdin");
        pipe.write_all(stdin.as_bytes()).expect("escribir stdin");
    }
    child.wait_with_output().expect("esperar drivesync")
}

fn run_cli_in(base: &str, config: &Path, cwd: &Path, args: &[&str]) -> Output {
    let mut command = command_for(base, config, Some("token-de-prueba"));
    command
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("no se pudo ejecutar drivesync")
}

fn command_for(base: &str, config: &Path, token: Option<&str>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_drivesync"));
    command
        .env("DRIVESYNC_API_BASE", base)
        .env("DRIVESYNC_CONFIG", config)
        .env("DRIVESYNC_TOKEN_URL", format!("{base}/token"));
    match token {
        Some(token) => {
            command.env("GOOGLE_ACCESS_TOKEN", token);
        }
        None => {
            command.env_remove("GOOGLE_ACCESS_TOKEN");
        }
    }
    command
}

fn run_cli_inner(base: &str, config: &Path, args: &[&str], token: Option<&str>) -> Output {
    command_for(base, config, token)
        .args(args)
        .output()
        .expect("no se pudo ejecutar drivesync")
}

fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "código {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn config_path(work: &Path) -> PathBuf {
    work.join("config.json")
}

#[test]
fn sincroniza_subidas_bajadas_conflictos_y_borrados() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());
    let local = work.path().join("local");
    fs::create_dir_all(local.join("sub")).unwrap();
    fs::write(local.join("a.txt"), b"hola").unwrap();
    fs::write(local.join("sub/b.txt"), b"mundo").unwrap();
    let backup = mock.add_folder("Backup", "root");
    let local_str = local.to_str().unwrap();

    // Simulación: no debe tocar ni el remoto ni el estado local.
    let output = run_cli(
        mock.base(),
        &config,
        &["sync", local_str, "--remote", "Backup", "--dry-run"],
    );
    assert_ok(&output);
    assert!(
        stdout(&output).contains("haría[subir]"),
        "{}",
        stdout(&output)
    );
    assert!(mock.content_by_name("a.txt").is_none());
    assert!(!local.join(".drivesync").exists());

    let output = run_cli(
        mock.base(),
        &config,
        &["sync", local_str, "--remote", "Backup"],
    );
    assert_ok(&output);
    let out = stdout(&output);
    assert!(out.contains("2 subidos"), "{out}");
    assert_eq!(mock.content_by_name("a.txt").as_deref(), Some(&b"hola"[..]));
    assert_eq!(
        mock.content_by_name("b.txt").as_deref(),
        Some(&b"mundo"[..])
    );
    assert!(local.join(".drivesync/state.json").exists());

    // Segunda pasada: nada que hacer.
    let output = run_cli(
        mock.base(),
        &config,
        &["sync", local_str, "--remote", "Backup"],
    );
    assert_ok(&output);
    assert!(
        stdout(&output).contains("0 subidos, 0 bajados"),
        "{}",
        stdout(&output)
    );

    // Cambio local + archivo nuevo en el remoto.
    fs::write(local.join("a.txt"), b"local-v2").unwrap();
    mock.add_file("remoto.txt", &backup, b"solo-remoto");

    let output = run_cli(
        mock.base(),
        &config,
        &["sync", local_str, "--remote", "Backup"],
    );
    assert_ok(&output);
    let out = stdout(&output);
    assert!(out.contains("1 subidos"), "{out}");
    assert!(out.contains("1 bajados"), "{out}");
    assert_eq!(fs::read(local.join("remoto.txt")).unwrap(), b"solo-remoto");
    assert_eq!(
        mock.content_by_name("a.txt").as_deref(),
        Some(&b"local-v2"[..])
    );

    // Conflicto: ambos lados cambian.
    fs::write(local.join("a.txt"), b"local-v3").unwrap();
    mock.set_content("a.txt", b"remoto-v3");

    let output = run_cli(
        mock.base(),
        &config,
        &["sync", local_str, "--remote", "Backup"],
    );
    assert_ok(&output);
    assert!(
        stdout(&output).contains("1 conflictos"),
        "{}",
        stdout(&output)
    );
    assert_eq!(
        mock.content_by_name("a.txt").as_deref(),
        Some(&b"local-v3"[..])
    );

    let conflicts: Vec<MockFile> = mock
        .children(&backup)
        .into_iter()
        .filter(|file| file.name.starts_with("a.conflicto-remoto-"))
        .collect();
    assert_eq!(
        conflicts.len(),
        1,
        "el remoto debe conservar ambas versiones"
    );
    assert_eq!(conflicts[0].content, b"remoto-v3");

    let local_conflicts: Vec<PathBuf> = fs::read_dir(&local)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().starts_with("a.conflicto-remoto-"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(local_conflicts.len(), 1);
    assert_eq!(fs::read(&local_conflicts[0]).unwrap(), b"remoto-v3");

    // Tras resolver el conflicto, todo queda estable.
    let output = run_cli(
        mock.base(),
        &config,
        &["sync", local_str, "--remote", "Backup"],
    );
    assert_ok(&output);
    let out = stdout(&output);
    assert!(out.contains("0 subidos, 0 bajados"), "{out}");
    assert!(out.contains("0 conflictos"), "{out}");

    // Borrado propagado.
    fs::remove_file(local.join("sub/b.txt")).unwrap();
    let output = run_cli(
        mock.base(),
        &config,
        &["sync", local_str, "--remote", "Backup", "--delete"],
    );
    assert_ok(&output);
    assert!(
        stdout(&output).contains("1 eliminados"),
        "{}",
        stdout(&output)
    );
    assert!(mock.is_trashed("b.txt"));

    // No se vuelve a bajar.
    let output = run_cli(
        mock.base(),
        &config,
        &["sync", local_str, "--remote", "Backup"],
    );
    assert_ok(&output);
    assert!(!local.join("sub/b.txt").exists());
}

#[test]
fn respeta_exclusiones_en_ambos_lados() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());
    let local = work.path().join("local");
    fs::create_dir_all(local.join("cache")).unwrap();
    fs::write(local.join("keep.txt"), b"keep").unwrap();
    fs::write(local.join("cache/local.bin"), b"local").unwrap();

    let target = mock.add_folder("Excl", "root");
    let remote_cache = mock.add_folder("cache", &target);
    mock.add_file("remoto.bin", &remote_cache, b"remoto");

    let output = run_cli(
        mock.base(),
        &config,
        &[
            "sync",
            local.to_str().unwrap(),
            "--remote",
            "Excl",
            "--exclude",
            "cache/**",
            "--delete",
        ],
    );
    assert_ok(&output);
    assert_eq!(
        mock.content_by_name("keep.txt").as_deref(),
        Some(&b"keep"[..])
    );
    assert!(mock.content_by_name("local.bin").is_none());
    assert_eq!(
        mock.content_by_name("remoto.bin").as_deref(),
        Some(&b"remoto"[..])
    );
}

#[test]
fn sube_archivos_grandes_con_carga_reanudable() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());
    let local = work.path().join("grande");
    fs::create_dir_all(&local).unwrap();

    let mut data = vec![0u8; 9 * 1024 * 1024 + 123];
    for (index, byte) in data.iter_mut().enumerate() {
        *byte = (index % 251) as u8;
    }
    fs::write(local.join("big.bin"), &data).unwrap();
    mock.add_folder("Grandes", "root");

    let output = run_cli(
        mock.base(),
        &config,
        &["sync", local.to_str().unwrap(), "--remote", "Grandes"],
    );
    assert_ok(&output);
    assert!(stdout(&output).contains("1 subidos"), "{}", stdout(&output));

    let remote = mock.content_by_name("big.bin").expect("archivo remoto");
    assert_eq!(remote.len(), data.len());
    assert_eq!(md5_hex(&remote), md5_hex(&data));
}

#[test]
fn reintenta_timeouts_408_en_archivos_grandes() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());
    let local = work.path().join("grande");
    fs::create_dir_all(&local).unwrap();

    let data = vec![7u8; 9 * 1024 * 1024 + 55];
    fs::write(local.join("big.bin"), &data).unwrap();
    mock.add_folder("Tiempos", "root");

    // Tres 408 seguidos: cabe dentro de los reintentos del bloque.
    mock.fail_chunks(3);

    let output = run_cli(
        mock.base(),
        &config,
        &["upload", local.to_str().unwrap(), "--to", "Tiempos"],
    );
    assert_ok(&output);
    assert!(stdout(&output).contains("1 subidos"), "{}", stdout(&output));

    let remoto = mock.content_by_name("big.bin").expect("archivo remoto");
    assert_eq!(md5_hex(&remoto), md5_hex(&data));
}

#[test]
fn reanuda_la_carga_tras_timeouts_persistentes() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());
    let local = work.path().join("grande");
    fs::create_dir_all(&local).unwrap();

    let data = vec![9u8; 9 * 1024 * 1024 + 77];
    fs::write(local.join("big.bin"), &data).unwrap();
    mock.add_folder("Tiempos", "root");

    // Más fallos que intentos por bloque: obliga a consultar el progreso de la
    // sesión y continuar, que es lo que salva una subida de varios GB.
    mock.fail_chunks(12);

    let output = run_cli(
        mock.base(),
        &config,
        &["upload", local.to_str().unwrap(), "--to", "Tiempos"],
    );
    assert_ok(&output);
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("recuperado"),
        "debe avisar de la reanudación: {err}"
    );

    let remoto = mock.content_by_name("big.bin").expect("archivo remoto");
    assert_eq!(remoto.len(), data.len());
    assert_eq!(md5_hex(&remoto), md5_hex(&data));
}

#[test]
fn descarga_archivos_y_carpetas_respetando_overwrite() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());
    let folder = mock.add_folder("Docs", "root");
    mock.add_file("x.txt", &folder, b"abc");
    let sub = mock.add_folder("Sub", &folder);
    mock.add_file("y.txt", &sub, b"def");

    let dest = work.path().join("salida");
    let dest_str = dest.to_str().unwrap();

    let output = run_cli(
        mock.base(),
        &config,
        &["download", "Docs", "--to", dest_str],
    );
    assert_ok(&output);
    assert_eq!(fs::read(dest.join("x.txt")).unwrap(), b"abc");
    assert_eq!(fs::read(dest.join("Sub/y.txt")).unwrap(), b"def");

    fs::write(dest.join("x.txt"), b"local").unwrap();
    let output = run_cli(
        mock.base(),
        &config,
        &["download", "Docs", "--to", dest_str],
    );
    assert_ok(&output);
    assert_eq!(fs::read(dest.join("x.txt")).unwrap(), b"local");

    let output = run_cli(
        mock.base(),
        &config,
        &["download", "Docs", "--to", dest_str, "--overwrite"],
    );
    assert_ok(&output);
    assert_eq!(fs::read(dest.join("x.txt")).unwrap(), b"abc");

    // Archivo suelto por ruta y sin --to: se escribe en el directorio actual.
    let solo = work.path().join("solo");
    fs::create_dir_all(&solo).unwrap();
    let output = run_cli_in(mock.base(), &config, &solo, &["download", "Docs/x.txt"]);
    assert_ok(&output);
    assert_eq!(fs::read(solo.join("x.txt")).unwrap(), b"abc");
}

#[test]
fn renueva_el_token_con_refresh_token() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());

    let output = run_cli_without_token(
        mock.base(),
        &config,
        &[
            "auth",
            "--client-id",
            "cid",
            "--client-secret",
            "sec",
            "--refresh-token",
            "r1",
            "--quiet",
        ],
    );
    assert_ok(&output);
    let saved: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(saved["access_token"], "token-renovado");
    assert_eq!(saved["refresh_token"], "r1");

    // Forzamos un token caducado: el CLI debe renovarlo solo.
    let mut saved = saved;
    saved["access_token_expiry"] = json!(1);
    fs::write(&config, serde_json::to_vec_pretty(&saved).unwrap()).unwrap();

    let output = run_cli_without_token(mock.base(), &config, &["status"]);
    assert_ok(&output);
    assert!(
        stdout(&output).contains("prueba@example.com"),
        "{}",
        stdout(&output)
    );

    let refreshed: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(refreshed["access_token"], "token-renovado");
}

#[test]
fn auth_manual_canjea_la_url_pegada() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());
    let args = [
        "auth",
        "--manual",
        "--client-id",
        "cid",
        "--client-secret",
        "sec",
        "--state",
        "estado-fijo",
        "--quiet",
    ];

    // Sin entrada y sin página posible (puerto privilegiado): falla al instante
    // y no guarda nada.
    let mut sin_pagina = args.to_vec();
    sin_pagina.extend_from_slice(&["--redirect-uri", "http://127.0.0.1:1/callback"]);
    let sin_entrada = run_cli_with_stdin(mock.base(), &config, &sin_pagina, "");
    assert!(!sin_entrada.status.success());
    assert!(!config.exists(), "no debe guardar nada si no hay entrada");
    let out = stdout(&sin_entrada);
    assert!(
        out.contains("accounts.google.com/o/oauth2/v2/auth"),
        "{out}"
    );
    assert!(out.contains("access_type=offline"), "{out}");
    // La URL va escapada; se comprueba el valor decodificado.
    let decoded = percent_decode_str(&out).decode_utf8_lossy().into_owned();
    assert!(decoded.contains("state=estado-fijo"), "{decoded}");
    assert!(
        decoded.contains("redirect_uri=http://127.0.0.1:1/callback"),
        "{decoded}"
    );

    // Un state ajeno se rechaza.
    let ajeno = run_cli_with_stdin(
        mock.base(),
        &config,
        &args,
        "http://127.0.0.1:8765/callback?state=otro&code=codigo\n",
    );
    assert!(!ajeno.status.success());
    assert!(!config.exists());
    assert!(
        String::from_utf8_lossy(&ajeno.stderr).contains("state no coincide"),
        "{}",
        String::from_utf8_lossy(&ajeno.stderr)
    );

    // Con el state correcto se canjea el código y se guarda el refresh token.
    let bueno = run_cli_with_stdin(
        mock.base(),
        &config,
        &args,
        "http://127.0.0.1:8765/callback?state=estado-fijo&code=codigo-de-prueba\n",
    );
    assert_ok(&bueno);

    let saved: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(saved["access_token"], "token-renovado");
    assert_eq!(saved["refresh_token"], "refresh-nuevo");
    assert_eq!(saved["client_id"], "cid");
}

/// Lanza el CLI, espera a que imprima la URL con el puerto, simula el navegador
/// y devuelve (HTML servido, salida del proceso).
fn run_manual_with_browser(
    base: &str,
    config: &Path,
    state: &str,
    queries: &[&str],
) -> (Vec<String>, Output, String) {
    use std::process::Stdio;
    use std::sync::{Arc, Mutex};

    let mut child = command_for(base, config, None)
        .args([
            "auth",
            "--manual",
            "--client-id",
            "cid",
            "--client-secret",
            "sec",
            "--port",
            "0",
            "--state",
            state,
            "--quiet",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("no se pudo ejecutar drivesync");

    let stdout = child.stdout.take().expect("stdout");
    let collected = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&collected);
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            sink.lock().unwrap().push_str(&line);
            line.clear();
        }
    });

    // Espera a que aparezca la URL con el puerto real.
    let mut port = None;
    for _ in 0..100 {
        let text = collected.lock().unwrap().clone();
        if let Some(index) = text.find("127.0.0.1:") {
            let rest = &text[index + "127.0.0.1:".len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(value) = digits.parse::<u16>() {
                port = Some(value);
                break;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    let port = port.expect("el CLI nunca imprimió la URL con el puerto");

    let mut responses = Vec::new();
    for query in queries {
        let mut connection = TcpStream::connect(("127.0.0.1", port)).expect("conectar al callback");
        let request = format!(
            "GET /callback?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        );
        connection
            .write_all(request.as_bytes())
            .expect("enviar petición");
        let mut response = String::new();
        connection
            .read_to_string(&mut response)
            .expect("leer respuesta");
        responses.push(response);
    }

    let output = child.wait_with_output().expect("esperar drivesync");
    let text = collected.lock().unwrap().clone();
    (responses, output, text)
}

#[test]
fn auth_manual_sirve_pagina_con_el_codigo_y_lo_recibe() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());

    let (responses, output, texto) = run_manual_with_browser(
        mock.base(),
        &config,
        "estado-pagina",
        &["state=estado-pagina&code=4/0AUn6Hd_codigoDeLaPagina"],
    );
    assert_ok(&output);
    let html = &responses[0];
    assert!(html.starts_with("HTTP/1.1 200"), "{html}");

    // La página muestra el código y el texto pedido.
    assert!(
        html.contains("Pega este código ahora en el CLI"),
        "falta el título: {html}"
    );
    assert!(
        html.contains("4/0AUn6Hd_codigoDeLaPagina"),
        "falta el código en la página"
    );
    assert!(html.contains("Copiar código"), "falta el botón de copiar");

    // Y el CLI lo recibió sin que se pegara nada: guardó los tokens.
    let saved: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(saved["access_token"], "token-renovado");
    assert_eq!(saved["refresh_token"], "refresh-nuevo");
    assert!(
        texto.contains("Código recibido desde el navegador"),
        "el CLI debe avisar que recibió el código por la página: {texto}"
    );
}

#[test]
fn auth_manual_rechaza_el_state_ajeno_y_luego_acepta() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());

    let (responses, output, _texto) = run_manual_with_browser(
        mock.base(),
        &config,
        "estado-bueno",
        &[
            // Primero una respuesta con state ajeno: debe rechazarse.
            "state=ajeno&code=4/0malo",
            // Después la buena: el CLI sigue esperando y ahora sí canjea.
            "state=estado-bueno&code=4/0bueno",
        ],
    );
    assert_ok(&output);
    assert!(
        responses[0].starts_with("HTTP/1.1 400"),
        "el state ajeno debe rechazarse: {}",
        responses[0]
    );
    assert!(
        responses[0].contains("Sesión no válida"),
        "{}",
        responses[0]
    );
    assert!(responses[1].starts_with("HTTP/1.1 200"), "{}", responses[1]);
    assert!(responses[1].contains("4/0bueno"));

    let saved: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(saved["access_token"], "token-renovado");
}

#[test]
fn auth_manual_acepta_un_token_de_acceso_pegado() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());

    let output = run_cli_with_stdin(
        mock.base(),
        &config,
        &[
            "auth",
            "--manual",
            "--client-id",
            "cid",
            "--client-secret",
            "sec",
            "--quiet",
        ],
        "ya29.token-pegado-a-mano\n",
    );
    assert_ok(&output);

    let saved: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(saved["access_token"], "ya29.token-pegado-a-mano");
    assert!(saved.get("refresh_token").is_none());
}

#[test]
fn auth_manual_falla_sin_entrada() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());

    let output = run_cli_with_stdin(
        mock.base(),
        &config,
        &[
            "auth",
            "--manual",
            "--client-id",
            "cid",
            "--client-secret",
            "sec",
            // Puerto privilegiado: no se puede servir página, así que sin stdin
            // el comando debe fallar en vez de esperar al navegador.
            "--redirect-uri",
            "http://127.0.0.1:1/callback",
            "--quiet",
        ],
        "",
    );
    assert!(!output.status.success());
    assert!(!config.exists(), "no debe guardar nada si no hay entrada");
}

#[test]
fn check_client_detecta_un_id_inexistente() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());

    let output = run_cli_without_token(
        mock.base(),
        &config,
        &[
            "auth",
            "--check-client",
            "--client-id",
            "cliente-desconocido",
            "--client-secret",
            "x",
        ],
    );
    assert!(!output.status.success());
    let out = stdout(&output);
    assert!(out.contains("NO encuentra"), "{out}");
    assert!(out.contains("Aplicación de escritorio"), "{out}");
    assert!(
        !config.exists(),
        "el diagnóstico no debe guardar configuración"
    );
}

#[test]
fn check_client_acepta_un_id_existente() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());

    let output = run_cli_without_token(
        mock.base(),
        &config,
        &[
            "auth",
            "--check-client",
            "--client-id",
            "123456789012-abc.apps.googleusercontent.com",
            "--client-secret",
            "x",
        ],
    );
    assert_ok(&output);
    let out = stdout(&output);
    assert!(out.contains("el cliente existe"), "{out}");
    assert!(out.contains("invalid_grant"), "{out}");
    assert!(!config.exists());
}

#[test]
fn mkdir_crea_subcarpetas_anidadas() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());
    let recomp = mock.add_folder("Recomp", "root");

    // 1) Ruta con separador: debe crear NSW 2 DENTRO de Recomp.
    let output = run_cli(mock.base(), &config, &["mkdir", "Recomp/NSW 2"]);
    assert_ok(&output);
    let out = stdout(&output);
    assert!(out.contains("Carpeta creada: Recomp/NSW 2/"), "{out}");
    assert!(out.contains("dentro de 'Mi unidad'"), "{out}");

    let dentro: Vec<String> = mock.children(&recomp).into_iter().map(|f| f.name).collect();
    assert_eq!(dentro, vec!["NSW 2".to_string()], "debe colgar de Recomp");
    let raiz: Vec<String> = mock.children("root").into_iter().map(|f| f.name).collect();
    assert!(
        !raiz
            .iter()
            .any(|name| name.contains('/') || name.contains("NSW")),
        "no debe crear nada en la raíz: {raiz:?}"
    );

    // 2) Estilo Windows con barra invertida.
    let output = run_cli(mock.base(), &config, &["mkdir", "Recomp\\GBA 2"]);
    assert_ok(&output);
    let dentro: Vec<String> = mock.children(&recomp).into_iter().map(|f| f.name).collect();
    assert!(
        dentro.contains(&"GBA 2".to_string()),
        "la barra invertida también debe separar: {dentro:?}"
    );

    // 3) --folder con nombre simple.
    let output = run_cli(
        mock.base(),
        &config,
        &["mkdir", "--folder", "Recomp", "NDS 2"],
    );
    assert_ok(&output);
    let dentro: Vec<String> = mock.children(&recomp).into_iter().map(|f| f.name).collect();
    assert!(dentro.contains(&"NDS 2".to_string()), "{dentro:?}");

    // 4) Intermedios que no existen se crean (mkdir -p).
    let output = run_cli(mock.base(), &config, &["mkdir", "Recomp/PSX 2/ISOs"]);
    assert_ok(&output);
    let psx = mock
        .children(&recomp)
        .into_iter()
        .find(|f| f.name == "PSX 2")
        .expect("PSX 2 debe existir");
    let isos: Vec<String> = mock.children(&psx.id).into_iter().map(|f| f.name).collect();
    assert_eq!(isos, vec!["ISOs".to_string()]);

    // 5) Repetir no duplica.
    let output = run_cli(mock.base(), &config, &["mkdir", "Recomp/NSW 2"]);
    assert_ok(&output);
    let dentro: Vec<String> = mock.children(&recomp).into_iter().map(|f| f.name).collect();
    let repes = dentro.iter().filter(|name| *name == "NSW 2").count();
    assert_eq!(repes, 1, "no debe duplicar: {dentro:?}");

    // 6) --dry-run no crea nada.
    let antes = mock.children("root").len();
    let output = run_cli(
        mock.base(),
        &config,
        &["mkdir", "Recomp/Nueva", "--dry-run"],
    );
    assert_ok(&output);
    assert!(
        stdout(&output).contains("Recomp/Nueva"),
        "{}",
        stdout(&output)
    );
    let dentro: Vec<String> = mock.children(&recomp).into_iter().map(|f| f.name).collect();
    assert!(!dentro.contains(&"Nueva".to_string()), "{dentro:?}");
    assert_eq!(mock.children("root").len(), antes);
}

#[test]
fn las_rutas_con_barra_invertida_sirven_en_todos_los_comandos() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());
    let recomp = mock.add_folder("Recomp", "root");
    let nsw = mock.add_folder("NSW 2", &recomp);
    mock.add_file("juego.nsp", &nsw, b"contenido");

    // list con ruta de Windows
    let output = run_cli(mock.base(), &config, &["list", "--folder", "Recomp\\NSW 2"]);
    assert_ok(&output);
    assert!(stdout(&output).contains("juego.nsp"), "{}", stdout(&output));

    // download con ruta de Windows
    let dest = work.path().join("bajado");
    fs::create_dir_all(&dest).unwrap();
    let output = run_cli_in(
        mock.base(),
        &config,
        &dest,
        &["download", "Recomp\\NSW 2\\juego.nsp"],
    );
    assert_ok(&output);
    assert_eq!(fs::read(dest.join("juego.nsp")).unwrap(), b"contenido");

    // mkdir --folder con ruta de Windows
    let output = run_cli(
        mock.base(),
        &config,
        &["mkdir", "--folder", "Recomp\\NSW 2", "DLC"],
    );
    assert_ok(&output);
    let dentro: Vec<String> = mock.children(&nsw).into_iter().map(|f| f.name).collect();
    assert!(dentro.contains(&"DLC".to_string()), "{dentro:?}");
}

#[test]
fn upload_replica_el_arbol_recursivamente() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());
    let proyecto = work.path().join("proyecto");
    fs::create_dir_all(proyecto.join("src/nucleo")).unwrap();
    fs::create_dir_all(proyecto.join("vacia")).unwrap();
    fs::write(proyecto.join("README.md"), b"raiz").unwrap();
    fs::write(proyecto.join("src/main.rs"), b"main").unwrap();
    fs::write(proyecto.join("src/nucleo/mod.rs"), b"nucleo").unwrap();
    let backup = mock.add_folder("Backup", "root");

    let output = run_cli(
        mock.base(),
        &config,
        &["upload", proyecto.to_str().unwrap(), "--to", "Backup"],
    );
    assert_ok(&output);
    let out = stdout(&output);
    assert!(out.contains("3 subidos"), "{out}");
    assert!(out.contains("4 carpetas creadas"), "{out}");

    // Backup/proyecto/{README.md, src/{main.rs, nucleo/mod.rs}, vacia}
    let en_backup = mock.children(&backup);
    let raiz = en_backup
        .iter()
        .find(|f| f.name == "proyecto")
        .expect("debe existir la carpeta proyecto");
    assert!(raiz.mime == FOLDER_MIME);

    let nivel1: Vec<String> = mock
        .children(&raiz.id)
        .into_iter()
        .map(|f| f.name)
        .collect();
    assert!(nivel1.contains(&"README.md".to_string()), "{nivel1:?}");
    assert!(nivel1.contains(&"src".to_string()), "{nivel1:?}");
    assert!(
        nivel1.contains(&"vacia".to_string()),
        "las vacías cuentan: {nivel1:?}"
    );

    let src = mock
        .children(&raiz.id)
        .into_iter()
        .find(|f| f.name == "src")
        .unwrap();
    let nivel2: Vec<String> = mock.children(&src.id).into_iter().map(|f| f.name).collect();
    assert!(nivel2.contains(&"main.rs".to_string()), "{nivel2:?}");
    assert!(nivel2.contains(&"nucleo".to_string()), "{nivel2:?}");

    let nucleo = mock
        .children(&src.id)
        .into_iter()
        .find(|f| f.name == "nucleo")
        .unwrap();
    let nivel3: Vec<String> = mock
        .children(&nucleo.id)
        .into_iter()
        .map(|f| f.name)
        .collect();
    assert_eq!(nivel3, vec!["mod.rs".to_string()]);

    assert_eq!(
        mock.content_by_name("mod.rs").as_deref(),
        Some(&b"nucleo"[..])
    );

    // Segunda pasada: nada cambia.
    let output = run_cli(
        mock.base(),
        &config,
        &["upload", proyecto.to_str().unwrap(), "--to", "Backup"],
    );
    assert_ok(&output);
    assert!(
        stdout(&output).contains("3 sin cambios"),
        "{}",
        stdout(&output)
    );

    // Un archivo modificado se actualiza.
    fs::write(proyecto.join("src/main.rs"), b"main v2").unwrap();
    let output = run_cli(
        mock.base(),
        &config,
        &["upload", proyecto.to_str().unwrap(), "--to", "Backup"],
    );
    assert_ok(&output);
    assert!(
        stdout(&output).contains("1 actualizados"),
        "{}",
        stdout(&output)
    );
    assert_eq!(
        mock.content_by_name("main.rs").as_deref(),
        Some(&b"main v2"[..])
    );
}

#[test]
fn upload_acepta_varias_rutas_y_contenido_suelto() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());
    let docs = work.path().join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("a.txt"), b"a").unwrap();
    fs::write(work.path().join("suelto.txt"), b"suelto").unwrap();
    let backup = mock.add_folder("Backup", "root");

    // Varias rutas a la vez.
    let output = run_cli(
        mock.base(),
        &config,
        &[
            "upload",
            docs.to_str().unwrap(),
            work.path().join("suelto.txt").to_str().unwrap(),
            "--to",
            "Backup",
        ],
    );
    assert_ok(&output);
    let nivel: Vec<String> = mock.children(&backup).into_iter().map(|f| f.name).collect();
    assert!(nivel.contains(&"docs".to_string()), "{nivel:?}");
    assert!(nivel.contains(&"suelto.txt".to_string()), "{nivel:?}");

    // --contents sube el contenido sin crear la carpeta contenedora.
    let otro = work.path().join("otro");
    fs::create_dir_all(otro.join("sub")).unwrap();
    fs::write(otro.join("sub/b.txt"), b"b").unwrap();
    let output = run_cli(
        mock.base(),
        &config,
        &[
            "upload",
            otro.to_str().unwrap(),
            "--to",
            "Backup",
            "--contents",
        ],
    );
    assert_ok(&output);
    let nivel: Vec<String> = mock.children(&backup).into_iter().map(|f| f.name).collect();
    assert!(nivel.contains(&"sub".to_string()), "{nivel:?}");
    assert!(!nivel.contains(&"otro".to_string()), "{nivel:?}");
}

#[test]
fn upload_excluye_patrones_y_respeta_dry_run() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());
    let proyecto = work.path().join("proyecto");
    fs::create_dir_all(proyecto.join("cache")).unwrap();
    fs::write(proyecto.join("keep.txt"), b"keep").unwrap();
    fs::write(proyecto.join("temporal.tmp"), b"tmp").unwrap();
    fs::write(proyecto.join("cache/blob.bin"), b"bin").unwrap();
    let backup = mock.add_folder("Backup", "root");

    // Simulación: no debe crear nada.
    let output = run_cli(
        mock.base(),
        &config,
        &[
            "upload",
            proyecto.to_str().unwrap(),
            "--to",
            "Backup",
            "--dry-run",
        ],
    );
    assert_ok(&output);
    assert!(
        stdout(&output).contains("haría[subir]"),
        "{}",
        stdout(&output)
    );
    assert!(
        mock.children(&backup).is_empty(),
        "la simulación no crea nada"
    );

    let output = run_cli(
        mock.base(),
        &config,
        &[
            "upload",
            proyecto.to_str().unwrap(),
            "--to",
            "Backup",
            "--exclude",
            "*.tmp",
            "--exclude",
            "cache/**",
        ],
    );
    assert_ok(&output);
    let raiz = mock
        .children(&backup)
        .into_iter()
        .find(|f| f.name == "proyecto")
        .expect("carpeta proyecto");
    let nivel: Vec<String> = mock
        .children(&raiz.id)
        .into_iter()
        .map(|f| f.name)
        .collect();
    assert_eq!(nivel, vec!["keep.txt".to_string()], "{nivel:?}");
    assert!(mock.content_by_name("temporal.tmp").is_none());
    assert!(mock.content_by_name("blob.bin").is_none());
}

#[test]
fn upload_sigue_tras_un_error_y_termina_con_fallo() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());
    let proyecto = work.path().join("proyecto");
    fs::create_dir_all(&proyecto).unwrap();
    fs::write(proyecto.join("aaa.txt"), b"a").unwrap();
    fs::write(proyecto.join("zzz.txt"), b"z").unwrap();
    let backup = mock.add_folder("Backup", "root");

    // Un archivo remoto con el nombre de una carpeta local hace fallar esa rama.
    let raiz = mock.add_folder("proyecto", &backup);
    mock.add_file("sub", &raiz, b"archivo-que-estorba");
    fs::create_dir_all(proyecto.join("sub")).unwrap();
    fs::write(proyecto.join("sub/dentro.txt"), b"dentro").unwrap();

    let output = run_cli(
        mock.base(),
        &config,
        &["upload", proyecto.to_str().unwrap(), "--to", "Backup"],
    );
    // Termina con error...
    assert!(!output.status.success());
    // ...pero los archivos que no dependían del conflicto se subieron igual.
    assert_eq!(mock.content_by_name("aaa.txt").as_deref(), Some(&b"a"[..]));
    assert_eq!(mock.content_by_name("zzz.txt").as_deref(), Some(&b"z"[..]));
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("sub"), "{err}");
}

#[test]
fn list_muestra_el_tamano_recursivo_de_las_carpetas() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());

    let backup = mock.add_folder("Backup", "root");
    let sub = mock.add_folder("Sub", &backup);
    mock.add_file("a.bin", &backup, &vec![0u8; 1024]);
    mock.add_file("b.bin", &sub, &vec![0u8; 512]);
    mock.add_folder("Vacia", &backup);

    // En la raíz, Backup debe sumar su subárbol completo: 1024 + 512 = 1.5 KiB.
    let output = run_cli(mock.base(), &config, &["list"]);
    assert_ok(&output);
    let out = stdout(&output);
    assert!(out.contains("[1.5 KiB] Backup/"), "{out}");

    // Dentro de Backup: Sub suma 512 B, Vacia 0 B y los archivos sueltos 1.0 KiB.
    let output = run_cli(mock.base(), &config, &["list", "--folder", "Backup"]);
    assert_ok(&output);
    let out = stdout(&output);
    assert!(out.contains("[512 B] Sub/"), "{out}");
    assert!(
        out.contains("[0 B] Vacia/"),
        "una carpeta vacía no se omite: {out}"
    );
    assert!(out.contains("(1.0 KiB)"), "{out}");
}

#[test]
fn list_puede_omitir_los_tamanos() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());
    let backup = mock.add_folder("Backup", "root");
    mock.add_file("a.bin", &backup, &vec![0u8; 2048]);

    let output = run_cli(mock.base(), &config, &["list", "--no-sizes"]);
    assert_ok(&output);
    let out = stdout(&output);
    assert!(out.contains("[dir ] Backup/"), "{out}");
    assert!(
        !out.contains("KiB]"),
        "no debe mostrar tamaño de carpeta: {out}"
    );
}

#[test]
fn list_avisa_de_documentos_de_google_sin_tamano() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());
    let backup = mock.add_folder("Backup", "root");
    mock.add_file("a.bin", &backup, &vec![0u8; 1024]);
    mock.add_native("notas", &backup);

    let output = run_cli(mock.base(), &config, &["list"]);
    assert_ok(&output);
    let out = stdout(&output);
    assert!(out.contains("[1.0 KiB] Backup/"), "{out}");
    assert!(
        out.contains("documento(s) de Google sin tamaño"),
        "debe avisar de que el total no los incluye: {out}"
    );
}

#[test]
fn guarda_el_token_y_consulta_la_cuenta() {
    let mock = MockDrive::start();
    let work = tempfile::tempdir().unwrap();
    let config = config_path(work.path());

    let output = run_cli_without_token(
        mock.base(),
        &config,
        &["auth", "--access-token", "token-explicito", "--quiet"],
    );
    assert_ok(&output);
    assert!(config.exists());
    let saved: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(saved["access_token"], "token-explicito");

    let output = run_cli_without_token(mock.base(), &config, &["status"]);
    assert_ok(&output);
    let out = stdout(&output);
    assert!(out.contains("prueba@example.com"), "{out}");
    assert!(out.contains("token de acceso"), "{out}");
}
