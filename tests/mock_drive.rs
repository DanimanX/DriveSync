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

fn upload_chunk(id: &str, request: &Request, state: &Arc<Mutex<State>>) -> HttpResponse {
    let Some(range) = request.headers.get("content-range") else {
        return HttpResponse::error(400, "falta Content-Range");
    };
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
