//! Autenticación con Google: token estático, refresh token y flujo OAuth de
//! bucle local (installed app).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use reqwest::blocking::Client;
use serde::Deserialize;

use crate::config::{save_config, Config};
use crate::util;

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const MANUAL_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorBody {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

pub fn token_endpoint() -> String {
    std::env::var("DRIVESYNC_TOKEN_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "https://oauth2.googleapis.com/token".to_string())
}

fn http_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("drivesync/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(60))
        .build()
        .context("no se pudo crear el cliente HTTP")
}

/// Resultado de comprobar un par client_id / client_secret contra Google.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientCheck {
    /// Google conoce el cliente y solo rechazó el código de prueba.
    Known { code: String },
    /// Google no encuentra el cliente (`invalid_client`).
    Unknown { detail: String },
    /// Respuesta que no es JSON de OAuth (proxy, HTML de error, etc.).
    Other { detail: String },
}

/// Comprueba si un `client_id` existe en Google sin abrir el navegador.
///
/// Envía un código inválido al endpoint de tokens: si el cliente no existe la
/// respuesta es `invalid_client`, y si existe se queja del código
/// (`invalid_grant`). Así se distingue un ID mal copiado de un problema de
/// redirección o de permisos.
pub fn check_client(
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
) -> Result<ClientCheck> {
    let response = http_client()?
        .post(token_endpoint())
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", "codigo-de-prueba-drivesync"),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .context("no se pudo contactar el endpoint de tokens de Google")?;

    let body = response
        .text()
        .context("respuesta ilegible del endpoint de tokens")?;

    let Ok(err) = serde_json::from_str::<OAuthErrorBody>(&body) else {
        return Ok(ClientCheck::Other { detail: body });
    };

    let detail = err.error_description.unwrap_or_default();
    Ok(match err.error.as_str() {
        "invalid_client" => ClientCheck::Unknown { detail },
        other => ClientCheck::Known {
            code: other.to_string(),
        },
    })
}

fn post_token(params: &[(&str, String)]) -> Result<TokenResponse> {
    let response = http_client()?
        .post(token_endpoint())
        .form(params)
        .send()
        .context("no se pudo contactar el endpoint de tokens de Google")?;

    let status = response.status();
    let body = response
        .text()
        .context("respuesta ilegible del endpoint de tokens")?;

    if !status.is_success() {
        if let Ok(err) = serde_json::from_str::<OAuthErrorBody>(&body) {
            let detail = err.error_description.unwrap_or_default();
            bail!(
                "Google rechazó la petición de token ({}): {}{}",
                status.as_u16(),
                err.error,
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" — {detail}")
                }
            );
        }
        bail!(
            "Google rechazó la petición de token ({}): {}",
            status.as_u16(),
            body
        );
    }

    serde_json::from_str(&body).context("respuesta de token con formato inesperado")
}

/// Provee tokens de acceso y los renueva cuando hace falta.
pub struct TokenProvider {
    config: Config,
    path: Option<PathBuf>,
    static_token: Option<String>,
    persist: bool,
}

impl TokenProvider {
    pub fn new(
        config: Config,
        path: Option<PathBuf>,
        static_token: Option<String>,
        persist: bool,
    ) -> Self {
        Self {
            config,
            path,
            static_token,
            persist,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn can_refresh(&self) -> bool {
        self.config.has_refresh_flow()
    }

    pub fn auth_mode(&self) -> &'static str {
        if self.static_token.is_some() {
            "token de acceso (CLI/entorno)"
        } else if self.config.refresh_token.is_some() {
            "refresh token"
        } else if self.config.access_token.is_some() {
            "token de acceso guardado"
        } else {
            "sin credenciales"
        }
    }

    /// Devuelve un access token válido, renovándolo si está caducado.
    pub fn token(&mut self) -> Result<String> {
        if let Some(token) = &self.static_token {
            return Ok(token.clone());
        }
        if let Some(token) = &self.config.access_token {
            match self.config.access_token_expiry {
                Some(expiry) if expiry <= util::now_unix() + 60 => {}
                _ => return Ok(token.clone()),
            }
        }
        self.refresh().with_context(|| {
            "no hay un token de Google válido; ejecuta `drivesync auth` o pasa --token".to_string()
        })?;
        self.config
            .access_token
            .clone()
            .context("Google no devolvió un token de acceso")
    }

    /// Fuerza la renovación con el refresh token.
    pub fn refresh(&mut self) -> Result<()> {
        let client_id = self.config.client_id.clone().context(
            "falta client_id: ejecuta `drivesync auth --client-id ... --client-secret ...`",
        )?;
        let client_secret = self
            .config
            .client_secret
            .clone()
            .context("falta client_secret: ejecuta `drivesync auth` de nuevo")?;
        let refresh_token = self
            .config
            .refresh_token
            .clone()
            .context("falta refresh_token: ejecuta `drivesync auth` de nuevo")?;

        let response = post_token(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token".to_string()),
        ])?;

        self.config.access_token = Some(response.access_token);
        self.config.access_token_expiry =
            Some(util::now_unix() + response.expires_in.unwrap_or(3600) - 60);
        if let Some(scope) = response.scope {
            self.config.scope = Some(scope);
        }
        self.persist_token();
        Ok(())
    }

    fn persist_token(&self) {
        if !self.persist {
            return;
        }
        if let Some(path) = &self.path {
            if let Err(err) = save_config(&self.config, path) {
                eprintln!("[aviso] no se pudo guardar el token renovado: {err:#}");
            }
        }
    }
}

pub struct AuthOptions {
    pub client_id: String,
    pub client_secret: String,
    pub scope: String,
    pub port: u16,
    pub open_browser: bool,
}

/// Compone la URL de consentimiento de Google.
pub fn authorization_url(client_id: &str, redirect_uri: &str, scope: &str, state: &str) -> String {
    format!(
        "{AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&state={}",
        encode(client_id),
        encode(redirect_uri),
        encode(scope),
        encode(state),
    )
}

/// Flujo OAuth de aplicación instalada con redirección a 127.0.0.1.
pub fn run_loopback(options: &AuthOptions) -> Result<Config> {
    let listener = TcpListener::bind(("127.0.0.1", options.port)).with_context(|| {
        format!(
            "no se pudo escuchar en 127.0.0.1:{} (prueba otro --port)",
            options.port
        )
    })?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let state = util::unique_id();
    let auth_url = authorization_url(&options.client_id, &redirect_uri, &options.scope, &state);

    println!("Autoriza el acceso abriendo esta URL:\n\n  {auth_url}\n");
    if options.open_browser {
        if let Err(err) = open_browser(&auth_url) {
            eprintln!("[aviso] no se pudo abrir el navegador automáticamente: {err}");
        }
    }
    println!("Esperando la redirección a {redirect_uri} (5 minutos máximo)...");

    let code = wait_for_code(&listener, &state, Duration::from_secs(300))?;
    exchange_code(
        &options.client_id,
        &options.client_secret,
        &code,
        &redirect_uri,
        &options.scope,
    )
}

/// Lo que el usuario pega cuando el flujo se hace fuera de banda.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualGrant {
    /// Código de autorización (suelto o dentro de la URL de redirección).
    Code(String),
    /// Token de acceso ya emitido (empieza por `ya29.`).
    AccessToken(String),
}

#[derive(Debug, Clone)]
pub struct ManualInput {
    pub grant: ManualGrant,
    /// Parámetro `state` si venía en una URL de redirección.
    pub state: Option<String>,
}

/// Interpreta lo que el usuario pegó: URL de redirección, código o token.
pub fn parse_manual_input(input: &str) -> Result<ManualInput> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("no se recibió nada; pega la URL de redirección o el código de autorización");
    }

    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let query = trimmed
            .split_once('?')
            .map(|(_, query)| query)
            .unwrap_or("");
        let params = parse_query(query);

        if let Some(error) = params.get("error") {
            let detail = params
                .get("error_description")
                .map(String::as_str)
                .unwrap_or("");
            bail!(
                "Google rechazó la autorización: {error}{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" — {detail}")
                }
            );
        }

        let Some(code) = params.get("code").filter(|code| !code.is_empty()) else {
            bail!(
                "la URL pegada no contiene el parámetro code=...; \
                 copia la barra de direcciones completa tras autorizar"
            );
        };

        return Ok(ManualInput {
            grant: ManualGrant::Code(code.clone()),
            state: params.get("state").cloned(),
        });
    }

    if trimmed.starts_with("ya29.") {
        return Ok(ManualInput {
            grant: ManualGrant::AccessToken(trimmed.to_string()),
            state: None,
        });
    }

    Ok(ManualInput {
        grant: ManualGrant::Code(trimmed.to_string()),
        state: None,
    })
}

pub struct ManualOptions {
    pub client_id: String,
    pub client_secret: String,
    pub scope: String,
    pub redirect_uri: String,
    /// Valor de `state` a exigir. Si es `None` se genera uno por ejecución.
    pub state: Option<String>,
}

/// Flujo OAuth sin servidor local: se muestra la URL y se pide el resultado
/// por teclado. Sirve en máquinas sin navegador o por SSH.
/// De dónde llegó el código: del navegador (página local) o pegado a mano.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Browser,
    Stdin,
}

/// Eventos que pueden desbloquear la espera.
enum Event {
    Input(Source, String),
    /// La entrada estándar se cerró sin datos.
    StdinClosed,
}

pub fn run_manual(options: &ManualOptions) -> Result<Config> {
    let state = options
        .state
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(util::unique_id);

    // Se intenta escuchar en la URI de redirección para poder servir una página
    // con el código. Si el puerto está ocupado, el flujo sigue siendo manual.
    let (host, requested_port, path) = parse_redirect_uri(&options.redirect_uri)
        .context("redirect_uri inválida: se espera http://127.0.0.1:<puerto>/callback")?;
    let listener = TcpListener::bind((host.as_str(), requested_port)).ok();
    let port = match &listener {
        Some(listener) => listener
            .local_addr()
            .map(|addr| addr.port())
            .unwrap_or(requested_port),
        None => requested_port,
    };
    let redirect_uri = format!("http://{host}:{port}{path}");
    let auth_url = authorization_url(&options.client_id, &redirect_uri, &options.scope, &state);

    println!("1. Abre esta URL en el navegador:\n");
    println!("   {auth_url}\n");
    match &listener {
        Some(_) => println!(
            "2. Autoriza la aplicación. DriveSync escucha en {redirect_uri}\n   y te mostrará el código en una página, listo para copiar."
        ),
        None => println!(
            "2. Autoriza la aplicación. El navegador intentará ir a {redirect_uri}\n   y mostrará un error de conexión: eso es esperado."
        ),
    }
    println!(
        "3. Si el navegador está en otra máquina y no llega a la página, copia la URL\n   completa de la barra de direcciones (contiene code=...).\n"
    );
    if listener.is_none() {
        eprintln!(
            "[aviso] el puerto {port} está ocupado: no habrá página local, pega la URL a mano."
        );
    }

    let (tx, rx) = mpsc::channel::<Event>();
    let has_page = listener.is_some();

    if let Some(listener) = listener {
        let tx = tx.clone();
        let expected = state.clone();
        let redirect = redirect_uri.clone();
        std::thread::spawn(move || {
            serve_code_page(listener, expected, redirect, tx);
        });
    }

    {
        let tx = tx.clone();
        std::thread::spawn(move || loop {
            match read_input_line() {
                Ok(line) if !line.trim().is_empty() => {
                    let _ = tx.send(Event::Input(Source::Stdin, line.trim().to_string()));
                    break;
                }
                Ok(_) => continue,
                Err(_) => {
                    let _ = tx.send(Event::StdinClosed);
                    break;
                }
            }
        });
    }
    drop(tx);

    print!("Pega la URL, el código o el token: ");
    std::io::stdout().flush().ok();

    let deadline = Instant::now() + MANUAL_TIMEOUT;
    let (source, raw) = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!(
                "tiempo de espera agotado ({} minutos) sin recibir la autorización",
                MANUAL_TIMEOUT.as_secs() / 60
            );
        }
        match rx.recv_timeout(remaining) {
            Ok(Event::Input(source, raw)) => break (source, raw),
            // Sin entrada por teclado y sin página posible no hay nada que esperar.
            Ok(Event::StdinClosed) if !has_page => bail!(
                "la entrada estándar se cerró sin datos; vuelve a ejecutar \
                 `drivesync auth --manual` y pega la URL o el código"
            ),
            Ok(Event::StdinClosed) => continue,
            Err(_) => bail!(
                "tiempo de espera agotado ({} minutos) sin recibir la autorización",
                MANUAL_TIMEOUT.as_secs() / 60
            ),
        }
    };

    let parsed = parse_manual_input(&raw)?;

    match &parsed.state {
        Some(pasted) if pasted == &state => {}
        Some(pasted) => bail!(
            "el parámetro state no coincide (se esperaba {state} y llegó {pasted}); \
             la respuesta no corresponde a esta sesión"
        ),
        None => eprintln!(
            "[aviso] no se pudo comprobar el parámetro state; asegúrate de que la respuesta \
             viene de esta misma ejecución"
        ),
    }

    if source == Source::Browser {
        println!("\nCódigo recibido desde el navegador; no hace falta pegarlo.");
    }

    match parsed.grant {
        ManualGrant::AccessToken(token) => {
            eprintln!(
                "[aviso] has pegado un token de acceso: caduca en ~1 hora y no se renueva solo."
            );
            Ok(Config {
                client_id: Some(options.client_id.clone()),
                client_secret: Some(options.client_secret.clone()),
                access_token: Some(token),
                access_token_expiry: None,
                scope: Some(options.scope.clone()),
                ..Default::default()
            })
        }
        ManualGrant::Code(code) => exchange_code(
            &options.client_id,
            &options.client_secret,
            &code,
            &redirect_uri,
            &options.scope,
        ),
    }
}

/// Atiende peticiones hasta recibir un `code` válido, mostrando la página.
fn serve_code_page(
    listener: TcpListener,
    expected_state: String,
    redirect_uri: String,
    tx: mpsc::Sender<Event>,
) {
    for stream in listener.incoming().flatten() {
        let mut stream = stream;
        let target = match read_request_target(&mut stream) {
            Some(target) => target,
            None => continue,
        };
        let (path, query) = split_target(&target);
        let params = parse_query(query);

        if path != "/callback" && path != "/" {
            let _ = write_html(
                &mut stream,
                404,
                "DriveSync",
                &simple_page(
                    "Ruta desconocida",
                    "Vuelve a la terminal y abre la URL de autorización que se imprimió.",
                ),
            );
            continue;
        }

        if let Some(error) = params.get("error") {
            let detail = params
                .get("error_description")
                .cloned()
                .unwrap_or_else(|| error.clone());
            let _ = write_html(
                &mut stream,
                400,
                "DriveSync · error",
                &simple_page("Google rechazó la autorización", &detail),
            );
            let _ = tx.send(Event::Input(
                Source::Browser,
                format!("{redirect_uri}?error={error}"),
            ));
            break;
        }

        let Some(code) = params.get("code").filter(|code| !code.is_empty()) else {
            let _ = write_html(
                &mut stream,
                400,
                "DriveSync · falta el código",
                &simple_page(
                    "Falta el parámetro code",
                    "La URL no traía code=... Copia la URL completa de la barra de direcciones.",
                ),
            );
            continue;
        };

        if params.get("state").map(String::as_str) != Some(expected_state.as_str()) {
            let _ = write_html(
                &mut stream,
                400,
                "DriveSync · state inválido",
                &simple_page(
                    "Sesión no válida",
                    "El parámetro state no coincide con esta ejecución. Vuelve a la terminal \
                     y abre la URL que imprimió el comando actual.",
                ),
            );
            continue;
        }

        let full_url = format!("{redirect_uri}?{}", query);
        let _ = write_html(
            &mut stream,
            200,
            "DriveSync · código",
            &code_page(code, &full_url),
        );
        let _ = tx.send(Event::Input(Source::Browser, full_url));
        break;
    }
}

/// Página con el código de autorización, listo para copiar.
fn code_page(code: &str, full_url: &str) -> String {
    const PAGE: &str = r#"<!doctype html>
<html lang="es">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>DriveSync · pega este código en el CLI</title>
<style>
  :root { color-scheme: light dark; --bg:#0f1115; --card:#171a21; --fg:#e8eaed;
          --muted:#9aa3b2; --accent:#7bd88f; --line:#2a2f3a; }
  * { box-sizing: border-box; }
  body { margin:0; min-height:100vh; display:grid; place-items:center;
         background:var(--bg); color:var(--fg);
         font:16px/1.55 ui-sans-serif,system-ui,-apple-system,"Segoe UI",Roboto,sans-serif; padding:24px; }
  main { width:min(680px,100%); background:var(--card); border:1px solid var(--line);
         border-radius:16px; padding:32px; box-shadow:0 18px 50px rgba(0,0,0,.35); }
  h1 { font-size:1.35rem; margin:0 0 6px; }
  .lead { margin:0 0 22px; color:var(--muted); }
  .badge { display:inline-flex; align-items:center; gap:8px; font-size:.8rem;
           color:var(--accent); border:1px solid var(--accent); border-radius:999px;
           padding:4px 10px; margin-bottom:18px; }
  .code { font:600 clamp(1rem,2.6vw,1.4rem)/1.4 ui-monospace,SFMono-Regular,Menlo,monospace;
          background:#0b0d11; border:1px solid var(--line); border-radius:12px;
          padding:18px; word-break:break-all; user-select:all; }
  .row { display:flex; gap:10px; flex-wrap:wrap; margin-top:16px; }
  button { font:inherit; font-weight:600; cursor:pointer; border-radius:10px;
           border:1px solid var(--line); background:#222836; color:var(--fg);
           padding:10px 16px; transition:.15s; }
  button:hover { background:#2b3242; }
  button.primary { background:var(--accent); color:#08240f; border-color:transparent; }
  button.primary:hover { filter:brightness(1.06); }
  details { margin-top:22px; color:var(--muted); }
  summary { cursor:pointer; }
  .url { font:0.82rem/1.5 ui-monospace,SFMono-Regular,Menlo,monospace; margin-top:10px;
         padding:12px; background:#0b0d11; border:1px solid var(--line); border-radius:10px;
         word-break:break-all; user-select:all; }
  .warn { margin-top:22px; font-size:.85rem; color:var(--muted);
          border-top:1px solid var(--line); padding-top:16px; }
  code { font-family:ui-monospace,SFMono-Regular,Menlo,monospace; }
</style>
</head>
<body>
<main>
  <span class="badge">Autorización completada</span>
  <h1>Pega este código ahora en el CLI</h1>
  <p class="lead">Es lo único que falta. Vuelve a la terminal donde corre <code>drivesync</code>.</p>

  <div class="code" id="code">{{CODE}}</div>

  <div class="row">
    <button class="primary" id="copy">Copiar código</button>
    <button id="select">Seleccionar</button>
  </div>

  <details>
    <summary>¿No puedes pegarlo? Copia la URL completa</summary>
    <div class="url" id="url">{{URL}}</div>
    <div class="row"><button id="copy-url">Copiar URL</button></div>
  </details>

  <p class="warn">No compartas este código: permite obtener acceso a tu Drive durante unos
  minutos. Caduca en ~10 minutos y solo sirve una vez.</p>
</main>
<script>
  const code = document.getElementById('code').textContent.trim();
  const url = document.getElementById('url').textContent.trim();
  async function copy(text, button, label) {
    try {
      await navigator.clipboard.writeText(text);
      button.textContent = '¡Copiado!';
    } catch (error) {
      const area = document.createElement('textarea');
      area.value = text;
      document.body.appendChild(area);
      area.select();
      document.execCommand('copy');
      area.remove();
      button.textContent = 'Copiado';
    }
    setTimeout(() => { button.textContent = label; }, 2000);
  }
  document.getElementById('copy').addEventListener('click', (event) =>
    copy(code, event.currentTarget, 'Copiar código'));
  document.getElementById('copy-url').addEventListener('click', (event) =>
    copy(url, event.currentTarget, 'Copiar URL'));
  document.getElementById('select').addEventListener('click', () => {
    const range = document.createRange();
    range.selectNodeContents(document.getElementById('code'));
    const selection = window.getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
  });
</script>
</body>
</html>
"#;
    PAGE.replace("{{CODE}}", &escape_html(code))
        .replace("{{URL}}", &escape_html(full_url))
}

fn simple_page(title: &str, message: &str) -> String {
    format!(
        "<!doctype html><html lang=\"es\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}</title></head>\
         <body style=\"font:16px/1.6 system-ui,sans-serif;margin:3rem auto;max-width:40rem;padding:0 1.2rem\">\
         <h1 style=\"font-size:1.3rem\">{title}</h1><p>{message}</p></body></html>",
        title = escape_html(title),
        message = escape_html(message),
    )
}

fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

fn write_html(stream: &mut TcpStream, status: u16, title: &str, body: &str) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    let _ = title;
    Ok(())
}

fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buf = [0u8; 8192];
    let read = stream.read(&mut buf).ok()?;
    if read == 0 {
        return None;
    }
    let text = String::from_utf8_lossy(&buf[..read]);
    let line = text.lines().next()?;
    line.split_whitespace().nth(1).map(str::to_string)
}

/// `http://127.0.0.1:8765/callback` → (`127.0.0.1`, 8765, `/callback`).
fn parse_redirect_uri(uri: &str) -> Option<(String, u16, String)> {
    let rest = uri
        .strip_prefix("http://")
        .or_else(|| uri.strip_prefix("https://"))?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host.to_string(), port.parse().ok()?),
        None => (authority.to_string(), 80),
    };
    Some((host, port, path.to_string()))
}

fn read_input_line() -> Result<String> {
    use std::io::BufRead;
    let mut line = String::new();
    let read = std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("no se pudo leer la entrada estándar")?;
    if read == 0 {
        bail!(
            "la entrada estándar se cerró sin datos; vuelve a ejecutar `drivesync auth --manual`"
        );
    }
    Ok(line)
}

fn exchange_code(
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
    scope: &str,
) -> Result<Config> {
    let tokens = post_token(&[
        ("client_id", client_id.to_string()),
        ("client_secret", client_secret.to_string()),
        ("code", code.to_string()),
        ("grant_type", "authorization_code".to_string()),
        ("redirect_uri", redirect_uri.to_string()),
    ])?;

    if tokens.refresh_token.is_none() {
        eprintln!(
            "[aviso] Google no devolvió refresh_token. El token de acceso caducará en ~1 hora; \
             vuelve a ejecutar `drivesync auth` o revoca el acceso y repite con prompt=consent."
        );
    }

    Ok(Config {
        client_id: Some(client_id.to_string()),
        client_secret: Some(client_secret.to_string()),
        refresh_token: tokens.refresh_token,
        access_token: Some(tokens.access_token),
        access_token_expiry: Some(util::now_unix() + tokens.expires_in.unwrap_or(3600) - 60),
        scope: tokens.scope.or_else(|| Some(scope.to_string())),
        default_remote: None,
    })
}

fn wait_for_code(
    listener: &TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout;

    loop {
        if Instant::now() >= deadline {
            bail!("tiempo de espera agotado esperando la autorización de Google");
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let request_line = match read_request_line(&mut stream) {
                    Ok(line) => line,
                    Err(_) => continue,
                };
                let target = request_line.split_whitespace().nth(1).unwrap_or("/");
                let (path, query) = split_target(target);
                let params = parse_query(query);

                if path != "/callback" {
                    let _ = respond(&mut stream, "DriveSync", "Ruta desconocida.");
                    continue;
                }

                let body = if params.contains_key("error") {
                    "Google rechazó la autorización. Puedes cerrar esta ventana."
                } else {
                    "Autorización completada. Ya puedes cerrar esta ventana y volver a la terminal."
                };
                let _ = respond(&mut stream, "DriveSync", body);

                if let Some(error) = params.get("error") {
                    bail!("autorización cancelada por Google: {error}");
                }
                match params.get("state") {
                    Some(value) if value == expected_state => {}
                    _ => bail!("respuesta OAuth con state inválido; intenta de nuevo"),
                }
                if let Some(code) = params.get("code") {
                    return Ok(code.clone());
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => return Err(err).context("error aceptando la conexión local"),
        }
    }
}

fn split_target(target: &str) -> (&str, &str) {
    match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    }
}

/// Decodifica una query estilo `application/x-www-form-urlencoded`, donde `+`
/// significa espacio (es lo que usa Google en `error_description`).
fn decode_param(value: &str) -> String {
    percent_encoding::percent_decode_str(&value.replace('+', " "))
        .decode_utf8_lossy()
        .into_owned()
}

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    let mut params = std::collections::HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        params.insert(decode_param(key), decode_param(value));
    }
    params
}

fn read_request_line(stream: &mut TcpStream) -> Result<String> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = [0u8; 8192];
    let read = stream.read(&mut buf)?;
    let text = String::from_utf8_lossy(&buf[..read]);
    let line = text.lines().next().unwrap_or("").to_string();
    if line.is_empty() {
        bail!("petición HTTP vacía");
    }
    Ok(line)
}

fn respond(stream: &mut TcpStream, title: &str, message: &str) -> Result<()> {
    let body = format!(
        "<!doctype html><html lang=\"es\"><meta charset=\"utf-8\"><title>{title}</title>\
         <body style=\"font-family:sans-serif;margin:4rem;max-width:40rem\">\
         <h1>{title}</h1><p>{message}</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

/// Caracteres a escapar dentro de un valor de query (RFC 3986).
///
/// Se dejan intactos los *unreserved*: `A-Z a-z 0-9 - . _ ~` y `!`, que es
/// `sub-delim` válido en una query. Escapar `-` o `.` rompe el `client_id`:
/// Google no lo decodifica y responde `invalid_client` ("The OAuth client was
/// not found").
const QUERY_VALUE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

fn encode(value: &str) -> String {
    utf8_percent_encode(value, QUERY_VALUE).to_string()
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    let mut command = std::process::Command::new("xdg-open");
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    };
    command
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("no se pudo lanzar el navegador")?;
    Ok(())
}

/// Guarda un token de acceso estático (por ejemplo de `gcloud auth print-access-token`).
pub fn save_static_token(config: &mut Config, path: &Path, token: &str) -> Result<()> {
    config.access_token = Some(token.trim().to_string());
    config.access_token_expiry = None;
    save_config(config, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsea_query_con_escape() {
        let params = parse_query("code=4%2F0AbC&state=abc%20123&scope=a%20b");
        assert_eq!(params.get("code").map(String::as_str), Some("4/0AbC"));
        assert_eq!(params.get("state").map(String::as_str), Some("abc 123"));
    }

    #[test]
    fn decodifica_mas_como_espacio() {
        // Google manda así los error_description.
        let params = parse_query("error=access_denied&error_description=El+usuario+rechaz%C3%B3");
        assert_eq!(
            params.get("error_description").map(String::as_str),
            Some("El usuario rechazó")
        );
    }

    #[test]
    fn separa_path_y_query() {
        assert_eq!(split_target("/callback?code=x"), ("/callback", "code=x"));
        assert_eq!(split_target("/favicon.ico"), ("/favicon.ico", ""));
    }

    #[test]
    fn interpreta_url_de_redireccion() {
        let input = "http://127.0.0.1:8765/callback?state=abc&code=4%2F0AX&scope=x";
        let parsed = parse_manual_input(input).unwrap();
        assert_eq!(parsed.grant, ManualGrant::Code("4/0AX".to_string()));
        assert_eq!(parsed.state.as_deref(), Some("abc"));
    }

    #[test]
    fn interpreta_codigo_suelto() {
        let parsed = parse_manual_input("  4/0AXcodigo  \n").unwrap();
        assert_eq!(parsed.grant, ManualGrant::Code("4/0AXcodigo".to_string()));
        assert!(parsed.state.is_none());
    }

    #[test]
    fn interpreta_token_de_acceso() {
        let parsed = parse_manual_input("ya29.a0AfH6SMBx").unwrap();
        assert_eq!(
            parsed.grant,
            ManualGrant::AccessToken("ya29.a0AfH6SMBx".to_string())
        );
    }

    #[test]
    fn interpreta_la_url_real_de_google() {
        // Forma exacta que devuelve Google, con iss= intercalado y code=4/0A...
        let input = "http://127.0.0.1:8765/callback?state=156300-1793626227045296881-0&iss=https://accounts.google.com&code=4/0AUn6Hd_rVnOy8LleCRy0M9NM2Ehuxhemq8goQAFZYXCGlNPBt_nZeKT9HXlcaaRszyb8jg&scope=https://www.googleapis.com/auth/drive";
        let parsed = parse_manual_input(input).unwrap();
        assert_eq!(
            parsed.grant,
            ManualGrant::Code(
                "4/0AUn6Hd_rVnOy8LleCRy0M9NM2Ehuxhemq8goQAFZYXCGlNPBt_nZeKT9HXlcaaRszyb8jg"
                    .to_string()
            )
        );
        assert_eq!(
            parsed.state.as_deref(),
            Some("156300-1793626227045296881-0")
        );
    }

    #[test]
    fn rechaza_url_sin_codigo() {
        let error = parse_manual_input("http://127.0.0.1:8765/callback?state=abc").unwrap_err();
        assert!(format!("{error:#}").contains("code="));
    }

    #[test]
    fn reporta_error_oauth_de_la_url() {
        let input = "http://127.0.0.1:8765/callback?error=access_denied&error_description=El+usuario+rechaz\u{f3}";
        let error = parse_manual_input(input).unwrap_err();
        let texto = format!("{error:#}");
        assert!(texto.contains("access_denied"), "{texto}");
        assert!(texto.contains("rechaz"), "{texto}");
    }

    #[test]
    fn rechaza_entrada_vacia() {
        assert!(parse_manual_input("   \n").is_err());
    }

    #[test]
    fn no_escapa_caracteres_unreserved_del_client_id() {
        // Google no decodifica el client_id: escapar `-` o `.` da invalid_client.
        let client_id = "123456789012-abcdefghijklmnop.apps.googleusercontent.com";
        assert_eq!(encode(client_id), client_id);
        assert_eq!(encode("a-b_c.d~e"), "a-b_c.d~e");

        let url = authorization_url(client_id, "http://127.0.0.1:8765/callback", "s", "st");
        assert!(
            url.contains(&format!("client_id={client_id}&")),
            "el client_id debe ir literal: {url}"
        );
        assert!(!url.contains("%2E"), "no debe escapar puntos: {url}");
        assert!(!url.contains("%2D"), "no debe escapar guiones: {url}");
    }

    #[test]
    fn escapa_solo_lo_que_rompe_la_query() {
        assert_eq!(encode("a b"), "a%20b");
        assert_eq!(encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(encode("a#b"), "a%23b");
        assert_eq!(encode("a+b"), "a%2Bb");
        assert_eq!(encode("ñ"), "%C3%B1");
    }

    #[test]
    fn url_de_autorizacion_escapa_parametros() {
        let url = authorization_url(
            "id.apps.googleusercontent.com",
            "http://127.0.0.1:8765/callback",
            "https://www.googleapis.com/auth/drive",
            "estado-1",
        );
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(
            url.contains("client_id=id.apps.googleusercontent.com"),
            "el client_id debe ir literal: {url}"
        );
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
        // Se decodifica para comprobar el valor, no la forma exacta del escape.
        let decoded = percent_encoding::percent_decode_str(&url)
            .decode_utf8_lossy()
            .into_owned();
        assert!(
            decoded.contains("redirect_uri=http://127.0.0.1:8765/callback"),
            "{decoded}"
        );
        assert!(
            decoded.contains("scope=https://www.googleapis.com/auth/drive"),
            "{decoded}"
        );
        assert!(decoded.contains("state=estado-1"), "{decoded}");
        assert!(
            !url.contains(" "),
            "no debe haber espacios sin escapar: {url}"
        );
    }

    #[test]
    fn token_estatico_tiene_prioridad() {
        let config = Config {
            access_token: Some("guardado".into()),
            ..Default::default()
        };
        let mut provider = TokenProvider::new(config, None, Some("explicito".into()), false);
        assert_eq!(provider.token().unwrap(), "explicito");
        assert!(provider.static_token.is_some());
        assert_eq!(provider.auth_mode(), "token de acceso (CLI/entorno)");
    }

    #[test]
    fn token_guardado_sin_caducidad_se_usa() {
        let config = Config {
            access_token: Some("guardado".into()),
            ..Default::default()
        };
        let mut provider = TokenProvider::new(config, None, None, false);
        assert_eq!(provider.token().unwrap(), "guardado");
    }

    #[test]
    fn token_caducado_sin_refresh_falla() {
        let config = Config {
            access_token: Some("viejo".into()),
            access_token_expiry: Some(1),
            ..Default::default()
        };
        let mut provider = TokenProvider::new(config, None, None, false);
        let err = provider.token().unwrap_err();
        assert!(format!("{err:#}").contains("client_id"));
    }
}
