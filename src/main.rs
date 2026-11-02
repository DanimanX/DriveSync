//! drivesync — CLI para sincronizar archivos con Google Drive usando un token.

mod api;
mod auth;
mod config;
mod sync;
mod util;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::Value;

use crate::api::DriveClient;
use crate::auth::{AuthOptions, TokenProvider};
use crate::config::{Config, DEFAULT_SCOPE};
use crate::sync::{ConflictPolicy, Direction, SyncOptions};

const EJEMPLOS: &str = "\
Ejemplos:
  drivesync auth --client-id ID.apps.googleusercontent.com --client-secret SECRET
  drivesync auth --manual --client-id ID --client-secret SECRET
  drivesync auth --access-token \"$(gcloud auth print-access-token)\"
  drivesync list --folder Backup
  drivesync sync ./documentos --remote Backup/Documentos
  drivesync sync ./fotos --remote 1AbCdEf... --delete --dry-run
  drivesync upload ./informe.pdf --to Backup
  drivesync download Backup/informe.pdf --to ./descargas

Variables de entorno:
  GOOGLE_ACCESS_TOKEN    token de acceso (equivale a --token)
  GOOGLE_CLIENT_ID       client_id para renovar con refresh token
  GOOGLE_CLIENT_SECRET   client_secret para renovar con refresh token
  GOOGLE_REFRESH_TOKEN   refresh token de Google
  DRIVESYNC_CONFIG       ruta alternativa del archivo de configuración";

#[derive(Parser)]
#[command(
    name = "drivesync",
    version,
    about = "Sincroniza archivos y carpetas con Google Drive",
    after_help = EJEMPLOS
)]
struct Cli {
    /// Archivo de configuración (por defecto ~/.config/drivesync/config.json)
    #[arg(long, global = true, env = "DRIVESYNC_CONFIG", value_name = "ARCHIVO")]
    config: Option<PathBuf>,
    /// Token de acceso de Google
    #[arg(long, global = true, env = "GOOGLE_ACCESS_TOKEN", value_name = "TOKEN")]
    token: Option<String>,
    /// Muestra lo que se haría sin modificar nada
    #[arg(long, global = true)]
    dry_run: bool,
    /// Detalle adicional en la salida
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Solo errores y resumen
    #[arg(short, long, global = true)]
    quiet: bool,
    /// URL base de la API (pruebas)
    #[arg(long, global = true, hide = true)]
    api_base: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct AuthArgs {
    /// ID de cliente OAuth (aplicación de escritorio)
    #[arg(long)]
    client_id: Option<String>,
    /// Secreto de cliente OAuth
    #[arg(long)]
    client_secret: Option<String>,
    /// Guarda un token de acceso ya emitido
    #[arg(long)]
    access_token: Option<String>,
    /// Guarda un refresh token y lo valida
    #[arg(long)]
    refresh_token: Option<String>,
    /// Alcance OAuth solicitado
    #[arg(long)]
    scope: Option<String>,
    /// Puerto local para la redirección OAuth
    #[arg(long, default_value_t = 8765)]
    port: u16,
    /// No intenta abrir el navegador
    #[arg(long)]
    no_browser: bool,
    /// No abre un servidor local: muestra la URL y pide el código o el token a mano
    #[arg(long)]
    manual: bool,
    /// URI de redirección que se incrusta en la URL (por defecto http://127.0.0.1:<puerto>/callback)
    #[arg(long, value_name = "URI")]
    redirect_uri: Option<String>,
    /// Valor de state a exigir en la respuesta (por defecto se genera uno)
    #[arg(long, value_name = "VALOR")]
    state: Option<String>,
    /// Solo comprueba el client_id contra Google y sale (no guarda nada)
    #[arg(long)]
    check_client: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Autentica con Google (navegador, token de acceso o refresh token)
    Auth(AuthArgs),
    /// Muestra la cuenta, el alcance y el estado de la autenticación
    Status,
    /// Lista el contenido de una carpeta de Drive
    List {
        /// Carpeta por ID, ruta (`A/B`) o nombre (por defecto, la raíz)
        #[arg(long, value_name = "CARPETA")]
        folder: Option<String>,
        /// Máximo de elementos a mostrar
        #[arg(long, default_value_t = 200)]
        max: usize,
    },
    /// Sube archivos o carpetas, recursivamente en el caso de carpetas
    Upload {
        /// Archivos o carpetas a subir (varios)
        #[arg(required = true, num_args = 1.., value_name = "RUTA")]
        paths: Vec<PathBuf>,
        /// Carpeta de Drive de destino (por defecto, la raíz)
        #[arg(long = "to", visible_alias = "folder", value_name = "CARPETA")]
        remote: Option<String>,
        /// Nombre remoto; solo con una ruta
        #[arg(long, value_name = "NOMBRE")]
        name: Option<String>,
        /// ID de un archivo existente que se debe reemplazar; solo con una ruta
        #[arg(long, value_name = "ID")]
        replace: Option<String>,
        /// Sube solo el contenido de la carpeta, sin crear la carpeta contenedora
        #[arg(long)]
        contents: bool,
        /// Patrón a excluir, repetible: `*.tmp`, `cache/**`
        #[arg(long, value_name = "PATRÓN")]
        exclude: Vec<String>,
    },
    /// Descarga un archivo o una carpeta completa
    Download {
        /// Archivo o carpeta de Drive (ID, ruta o nombre)
        remote: String,
        /// Ruta local de destino
        #[arg(long = "to", value_name = "RUTA")]
        dest: Option<PathBuf>,
        /// Sobrescribe archivos locales distintos
        #[arg(long)]
        overwrite: bool,
    },
    /// Crea una carpeta en Drive, incluidas las intermedias que falten
    Mkdir {
        /// Nombre o ruta relativa: `Backup`, `Recomp/NSW 2`, `Recomp\\GBA 2`
        name: String,
        /// Carpeta contenedora (por defecto, la raíz)
        #[arg(long = "folder", visible_alias = "in", value_name = "CARPETA")]
        parent: Option<String>,
        /// Trata el nombre como literal: no interpreta `/` ni `\` como separadores
        #[arg(long)]
        literal: bool,
    },
    /// Sincroniza un directorio local con una carpeta de Drive
    Sync {
        /// Directorio local
        path: PathBuf,
        /// Carpeta de Drive (ID, ruta o nombre)
        #[arg(long, value_name = "CARPETA")]
        remote: Option<String>,
        /// Dirección de la sincronización
        #[arg(long, value_enum, default_value = "both")]
        direction: Direction,
        /// Propaga borrados (con cuidado)
        #[arg(long)]
        delete: bool,
        /// Qué hacer cuando ambos lados cambiaron
        #[arg(long, value_enum, default_value = "keep-both")]
        conflict: ConflictPolicy,
        /// Patrón a excluir (repetible): `*.tmp`, `cache/**`
        #[arg(long, value_name = "PATRÓN")]
        exclude: Vec<String>,
        /// Ignora el estado guardado y compara todo de nuevo
        #[arg(long)]
        no_state: bool,
    },
    /// Borra los tokens guardados
    Logout,
}

struct Global {
    config: Option<PathBuf>,
    token: Option<String>,
    dry_run: bool,
    verbose: bool,
    quiet: bool,
    api_base: Option<String>,
}

fn main() {
    let Cli {
        config,
        token,
        dry_run,
        verbose,
        quiet,
        api_base,
        command,
    } = Cli::parse();

    let global = Global {
        config,
        token,
        dry_run,
        verbose,
        quiet,
        api_base,
    };

    if let Err(error) = run(&global, command) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(global: &Global, command: Command) -> Result<()> {
    match command {
        Command::Auth(args) => cmd_auth(global, args),
        Command::Logout => cmd_logout(global),
        other => {
            let client = build_client(global)?;
            match other {
                Command::Status => cmd_status(global, &client),
                Command::List { folder, max } => cmd_list(global, &client, folder, max),
                Command::Upload {
                    paths,
                    remote,
                    name,
                    replace,
                    contents,
                    exclude,
                } => cmd_upload(
                    global, &client, paths, remote, name, replace, contents, exclude,
                ),
                Command::Download {
                    remote,
                    dest,
                    overwrite,
                } => cmd_download(global, &client, remote, dest, overwrite),
                Command::Mkdir {
                    name,
                    parent,
                    literal,
                } => cmd_mkdir(global, &client, name, parent, literal),
                Command::Sync {
                    path,
                    remote,
                    direction,
                    delete,
                    conflict,
                    exclude,
                    no_state,
                } => cmd_sync(
                    global, &client, path, remote, direction, delete, conflict, exclude, no_state,
                ),
                Command::Auth(_) | Command::Logout => unreachable!(),
            }
        }
    }
}

fn build_client(global: &Global) -> Result<DriveClient> {
    let (mut config, path) = config::load_config(global.config.as_deref())?;
    config::apply_env(&mut config);
    let provider = TokenProvider::new(config, Some(path), global.token.clone(), true);
    DriveClient::new(provider, global.api_base.clone(), global.verbose)
}

fn cmd_auth(global: &Global, args: AuthArgs) -> Result<()> {
    let AuthArgs {
        client_id,
        client_secret,
        access_token,
        refresh_token,
        scope,
        port,
        no_browser,
        manual,
        redirect_uri,
        state,
        check_client,
    } = args;
    let (mut config, path) = config::load_config(global.config.as_deref())?;
    config::apply_env(&mut config);
    let scope = scope
        .or_else(|| config.scope.clone())
        .unwrap_or_else(|| DEFAULT_SCOPE.to_string());

    if let Some(token) = access_token {
        auth::save_static_token(&mut config, &path, &token)?;
        println!("Token de acceso guardado en {}", path.display());
        if !global.quiet {
            match verify(&config, global.api_base.clone()) {
                Ok(about) => print_identity(&about),
                Err(error) => {
                    eprintln!("[aviso] el token se guardó pero no pudo verificarse: {error:#}")
                }
            }
        }
        return Ok(());
    }

    if let Some(refresh_token) = refresh_token {
        let client_id = client_id
            .or(config.client_id.clone())
            .context(faltan_credenciales())?;
        let client_secret = client_secret
            .or(config.client_secret.clone())
            .context(faltan_credenciales())?;
        config.client_id = Some(client_id);
        config.client_secret = Some(client_secret);
        config.refresh_token = Some(refresh_token.trim().to_string());
        config.access_token = None;
        config.access_token_expiry = None;
        config.scope = Some(scope);

        let mut provider = TokenProvider::new(config.clone(), Some(path.clone()), None, true);
        provider
            .refresh()
            .context("el refresh token no es válido o le faltan permisos")?;
        let updated = provider.config().clone();
        println!("Refresh token guardado y validado en {}", path.display());
        if !global.quiet {
            match verify(&updated, global.api_base.clone()) {
                Ok(about) => print_identity(&about),
                Err(error) => eprintln!(
                    "[aviso] el refresh token funciona pero no se pudo leer la cuenta: {error:#}"
                ),
            }
        }
        return Ok(());
    }

    let client_id = client_id
        .or(config.client_id.clone())
        .context(faltan_credenciales())?;
    let client_secret = client_secret
        .or(config.client_secret.clone())
        .context(faltan_credenciales())?;

    if check_client {
        let redirect = redirect_uri
            .clone()
            .unwrap_or_else(|| format!("http://127.0.0.1:{port}/callback"));
        return report_client_check(&client_id, &client_secret, &redirect);
    }

    if !global.quiet && !client_id.ends_with(".apps.googleusercontent.com") {
        eprintln!(
            "[aviso] el client_id no termina en .apps.googleusercontent.com: los clientes \
             de Android/iOS o de cuenta de servicio no sirven aquí. Si Google responde \
             \"invalid_client\", revisa que copiaste el ID completo del cliente de tipo \
             \"Aplicación de escritorio\"."
        );
    }

    let mut fresh = if manual {
        let redirect = redirect_uri
            .clone()
            .unwrap_or_else(|| format!("http://127.0.0.1:{port}/callback"));
        auth::run_manual(&auth::ManualOptions {
            client_id,
            client_secret,
            scope,
            redirect_uri: redirect,
            state,
        })?
    } else {
        auth::run_loopback(&AuthOptions {
            client_id,
            client_secret,
            scope,
            port,
            open_browser: !no_browser,
        })?
    };

    fresh.default_remote = config.default_remote.clone();
    config::save_config(&fresh, &path)?;
    println!("Credenciales guardadas en {}", path.display());

    if !global.quiet {
        match verify(&fresh, global.api_base.clone()) {
            Ok(about) => print_identity(&about),
            Err(error) => eprintln!("[aviso] no se pudo verificar la cuenta: {error:#}"),
        }
    }
    Ok(())
}

fn report_client_check(client_id: &str, client_secret: &str, redirect_uri: &str) -> Result<()> {
    println!("client_id    : {client_id}");
    println!("longitud     : {} caracteres", client_id.len());
    println!("redirect_uri : {redirect_uri}");
    println!("Comprobando contra Google...");

    match auth::check_client(client_id, client_secret, redirect_uri)? {
        auth::ClientCheck::Known { code } => {
            println!("Resultado    : el cliente existe en Google (respuesta: {code})");
            println!();
            println!("El client_id es correcto. Si el navegador sigue diciendo");
            println!("\"The OAuth client was not found\", revisa que la URL que abres sea la");
            println!("que imprime este mismo comando, sin editar ni recortar.");
            Ok(())
        }
        auth::ClientCheck::Unknown { detail } => {
            println!("Resultado    : Google NO encuentra este cliente ({detail})");
            println!();
            println!("El client_id no existe. Causas habituales:");
            println!(
                "  - Copiaste el ID incompleto, o pegaste el del proyecto en vez del cliente."
            );
            println!("  - El cliente se borró o pertenece a otro proyecto de Google Cloud.");
            println!("  - No es un client_id: los de cuenta de servicio son correos y los de");
            println!("    API key son cadenas que empiezan por AIza.");
            println!();
            println!("Créalo en Google Cloud Console -> APIs y servicios -> Credenciales");
            println!(
                "-> Crear credenciales -> ID de cliente OAuth -> \"Aplicación de escritorio\"."
            );
            bail!("el client_id no existe en Google")
        }
        auth::ClientCheck::Other { detail } => {
            println!("Resultado    : respuesta inesperada de Google");
            println!("{detail}");
            bail!("no se pudo determinar si el client_id es válido")
        }
    }
}

fn faltan_credenciales() -> String {
    "faltan credenciales de cliente OAuth.\n\
     Crea un cliente OAuth de tipo \"Aplicación de escritorio\" en Google Cloud y ejecuta:\n  \
     drivesync auth --client-id ID --client-secret SECRET\n\
     También puedes usar solo un token de acceso:\n  \
     drivesync auth --access-token \"$(gcloud auth print-access-token)\""
        .to_string()
}

fn verify(config: &Config, api_base: Option<String>) -> Result<Value> {
    let provider = TokenProvider::new(config.clone(), None, None, false);
    let client = DriveClient::new(provider, api_base, false)?;
    client.about()
}

fn print_identity(about: &Value) {
    let user = about.get("user");
    let name = user
        .and_then(|u| u.get("displayName"))
        .and_then(Value::as_str)
        .unwrap_or("(sin nombre)");
    let email = user
        .and_then(|u| u.get("emailAddress"))
        .and_then(Value::as_str)
        .unwrap_or("(sin correo)");
    println!("Cuenta        : {name} <{email}>");

    if let Some(quota) = about.get("storageQuota") {
        let usage = quota
            .get("usage")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<u64>().ok());
        let limit = quota
            .get("limit")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<u64>().ok());
        match (usage, limit) {
            (Some(used), Some(total)) => {
                println!(
                    "Almacenamiento: {} de {}",
                    util::format_size(used),
                    util::format_size(total)
                );
            }
            (Some(used), None) => println!("Almacenamiento: {} usados", util::format_size(used)),
            _ => {}
        }
    }
}

fn cmd_status(global: &Global, client: &DriveClient) -> Result<()> {
    let (config, path) = config::load_config(global.config.as_deref())?;
    println!("Configuración : {}", path.display());
    println!("Autenticación : {}", client.auth_mode());
    println!(
        "Alcance       : {}",
        config.scope.as_deref().unwrap_or(DEFAULT_SCOPE)
    );
    let about = client.about()?;
    print_identity(&about);
    Ok(())
}

fn cmd_list(
    global: &Global,
    client: &DriveClient,
    folder: Option<String>,
    max: usize,
) -> Result<()> {
    let folder = match folder {
        Some(reference) => client.resolve_ref(&reference)?,
        None => client.root(),
    };
    if !folder.is_folder() {
        bail!("'{}' no es una carpeta de Drive", folder.name);
    }

    let mut children = client.list_children(&folder.id)?;
    children.sort_by(|a, b| {
        b.is_folder()
            .cmp(&a.is_folder())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    if !global.quiet {
        println!(
            "{} — {} elemento(s) — {}",
            folder.name,
            children.len(),
            folder.id
        );
    }
    for child in children.iter().take(max) {
        let modified = child.modified_time.as_deref().unwrap_or("-");
        if child.is_folder() {
            println!("  [dir ] {}/  — {modified} — {}", child.name, child.id);
        } else {
            let kind = if child.is_google_native() {
                "google".to_string()
            } else {
                util::format_size(child.size_or_zero())
            };
            println!(
                "  [arch] {} ({kind}) — {modified} — {}",
                child.name, child.id
            );
        }
    }
    if children.len() > max {
        println!("  … {} elemento(s) más (usa --max)", children.len() - max);
    }
    Ok(())
}

#[derive(Debug, Default)]
struct UploadStats {
    uploaded: usize,
    updated: usize,
    skipped: usize,
    folders: usize,
    ignored: usize,
    errors: Vec<String>,
}

impl UploadStats {
    fn record(&mut self, outcome: UploadOutcome) {
        match outcome {
            UploadOutcome::Uploaded => self.uploaded += 1,
            UploadOutcome::Updated => self.updated += 1,
            UploadOutcome::Skipped => self.skipped += 1,
            UploadOutcome::Simulated => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadOutcome {
    Uploaded,
    Updated,
    /// Ya estaba igual (mismo MD5).
    Skipped,
    /// `--dry-run`: se describió pero no se tocó nada.
    Simulated,
}

#[allow(clippy::too_many_arguments)]
fn cmd_upload(
    global: &Global,
    client: &DriveClient,
    paths: Vec<PathBuf>,
    remote: Option<String>,
    name: Option<String>,
    replace: Option<String>,
    contents: bool,
    exclude: Vec<String>,
) -> Result<()> {
    if paths.is_empty() {
        bail!("indica al menos un archivo o una carpeta");
    }
    for path in &paths {
        if !path.exists() {
            bail!("no existe {}", path.display());
        }
    }
    if paths.len() > 1 && name.is_some() {
        bail!("--name solo se puede usar con una única ruta");
    }
    if paths.len() > 1 && replace.is_some() {
        bail!("--replace solo se puede usar con una única ruta");
    }

    let target = match remote {
        Some(reference) => client.resolve_ref(&reference)?,
        None => client.root(),
    };
    if !target.is_folder() {
        bail!("el destino '{}' no es una carpeta", target.name);
    }

    let mut stats = UploadStats::default();
    let mut tombstones = Vec::new();

    for path in &paths {
        let result = if path.is_file() {
            let remote_name = match &name {
                Some(name) => name.clone(),
                None => file_name(path)?,
            };
            if remote_name.contains('/') || remote_name.contains('\\') {
                bail!("el nombre remoto no puede contener separadores: {remote_name}");
            }
            upload_one(
                client,
                path,
                &target,
                &remote_name,
                replace.as_deref(),
                global.dry_run,
                global.quiet,
            )
            .map(|outcome| {
                stats.record(outcome);
            })
        } else if path.is_dir() {
            let dir_name = match &name {
                Some(name) => name.clone(),
                None => file_name(path)?,
            };
            // Por defecto la carpeta local se replica dentro del destino;
            // con --contents se sube solo lo que hay dentro.
            let (parent, prefix) = if contents {
                (Some(target.clone()), String::new())
            } else {
                match ensure_child_folder(client, &target, &dir_name, global.dry_run, global.quiet)
                {
                    Ok(Some(folder)) => (Some(folder), String::new()),
                    Ok(None) => (None, String::new()),
                    Err(error) => Err(error)?,
                }
            };
            if !global.quiet && !global.dry_run && parent.is_some() {
                stats.folders += 1;
            }
            upload_dir(
                client,
                path,
                parent.as_ref(),
                &prefix,
                &exclude,
                global.dry_run,
                global.quiet,
                &mut stats,
            )
        } else {
            bail!("{} no es un archivo ni un directorio", path.display());
        };

        if let Err(error) = result {
            let message = format!("{}: {error:#}", path.display());
            eprintln!("[error] {message}");
            stats.errors.push(message);
        }
        tombstones.push(path.clone());
    }

    // Resumen
    if !global.quiet {
        let mut partes = Vec::new();
        if stats.uploaded > 0 {
            partes.push(format!("{} subidos", stats.uploaded));
        }
        if stats.updated > 0 {
            partes.push(format!("{} actualizados", stats.updated));
        }
        if stats.skipped > 0 {
            partes.push(format!("{} sin cambios", stats.skipped));
        }
        if stats.folders > 0 {
            partes.push(format!("{} carpetas creadas", stats.folders));
        }
        if stats.ignored > 0 {
            partes.push(format!("{} omitidos", stats.ignored));
        }
        if partes.is_empty() {
            partes.push("nada que subir".to_string());
        }
        println!(
            "{}{}",
            if global.dry_run {
                "Simulación: "
            } else {
                "Resumen: "
            },
            partes.join(", ")
        );
    }

    if !stats.errors.is_empty() {
        bail!(
            "{} elemento(s) fallaron de {} ruta(s)",
            stats.errors.len(),
            tombstones.len()
        );
    }
    Ok(())
}

/// Recorre un directorio local subiéndolo en profundidad.
#[allow(clippy::too_many_arguments)]
fn upload_dir(
    client: &DriveClient,
    local_dir: &Path,
    remote_dir: Option<&api::RemoteFile>,
    prefix: &str,
    exclude: &[String],
    dry_run: bool,
    quiet: bool,
    stats: &mut UploadStats,
) -> Result<()> {
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(local_dir)
        .with_context(|| format!("no se pudo leer {}", local_dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == sync::STATE_DIR {
            continue;
        }
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if util::is_excluded(&rel, exclude) {
            if !quiet {
                println!("[omitido] {rel} (excluido)");
            }
            stats.ignored += 1;
            continue;
        }

        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("no se pudo leer {}", path.display()))?;

        if file_type.is_symlink() {
            eprintln!("[aviso] omitido (enlace simbólico): {rel}");
            stats.ignored += 1;
            continue;
        }

        if file_type.is_dir() {
            // Sin destino remoto conocido (simulación) se describe y se sigue.
            // Un fallo al crear esta carpeta no debe abortar el resto del árbol.
            let child = match remote_dir {
                Some(parent) => match ensure_child_folder(client, parent, &name, dry_run, quiet) {
                    Ok(folder) => {
                        if folder.is_some() {
                            stats.folders += 1;
                        }
                        folder
                    }
                    Err(error) => {
                        let message = format!("{rel}: {error:#}");
                        eprintln!("[error] {message}");
                        stats.errors.push(message);
                        continue;
                    }
                },
                None => None,
            };
            if let Err(error) = upload_dir(
                client,
                &path,
                child.as_ref(),
                &rel,
                exclude,
                dry_run,
                quiet,
                stats,
            ) {
                let message = format!("{rel}: {error:#}");
                eprintln!("[error] {message}");
                stats.errors.push(message);
            }
        } else if file_type.is_file() {
            match upload_file_in_dir(client, &path, remote_dir, &name, dry_run, quiet) {
                Ok(outcome) => stats.record(outcome),
                Err(error) => {
                    let message = format!("{rel}: {error:#}");
                    eprintln!("[error] {message}");
                    stats.errors.push(message);
                }
            }
        } else {
            eprintln!("[aviso] omitido (no es archivo regular): {rel}");
            stats.ignored += 1;
        }
    }
    Ok(())
}

/// Sube un archivo dentro de un recorrido; en simulación no consulta la API.
fn upload_file_in_dir(
    client: &DriveClient,
    path: &Path,
    remote_dir: Option<&api::RemoteFile>,
    name: &str,
    dry_run: bool,
    quiet: bool,
) -> Result<UploadOutcome> {
    match remote_dir {
        Some(parent) => upload_one(client, path, parent, name, None, dry_run, quiet),
        None => {
            if !quiet {
                println!("haría[subir] {name}");
            }
            Ok(UploadOutcome::Simulated)
        }
    }
}

fn upload_one(
    client: &DriveClient,
    path: &Path,
    target: &api::RemoteFile,
    name: &str,
    replace_id: Option<&str>,
    dry_run: bool,
    quiet: bool,
) -> Result<UploadOutcome> {
    let mime = util::mime_for(path);

    if let Some(file_id) = replace_id {
        if dry_run {
            println!("haría[actualizar-remoto] {name}");
            return Ok(UploadOutcome::Simulated);
        }
        client.update_file(file_id, mime, path)?;
        if !quiet {
            println!("[actualizado] {name} — {file_id}");
        }
        return Ok(UploadOutcome::Updated);
    }

    let md5 = util::md5_file(path)?;
    match client.find_child(&target.id, name)? {
        Some(existing) => {
            if existing.is_folder() {
                bail!(
                    "ya existe una carpeta llamada '{name}' en '{}'",
                    target.name
                );
            }
            if existing.md5_checksum.as_deref() == Some(md5.as_str()) {
                if !quiet {
                    println!("[igual] {name}");
                }
                return Ok(UploadOutcome::Skipped);
            }
            if dry_run {
                println!("haría[actualizar-remoto] {name}");
                return Ok(UploadOutcome::Simulated);
            }
            client.update_file(&existing.id, mime, path)?;
            if !quiet {
                println!("[actualizado] {name} — {}", existing.id);
            }
            Ok(UploadOutcome::Updated)
        }
        None => {
            if dry_run {
                println!("haría[subir] {name}");
                return Ok(UploadOutcome::Simulated);
            }
            let created = client.create_file(&target.id, name, mime, path)?;
            if !quiet {
                println!(
                    "[subido] {name} ({}) — {}",
                    util::format_size(fs::metadata(path)?.len()),
                    created.id
                );
            }
            Ok(UploadOutcome::Uploaded)
        }
    }
}

fn ensure_child_folder(
    client: &DriveClient,
    parent: &api::RemoteFile,
    name: &str,
    dry_run: bool,
    quiet: bool,
) -> Result<Option<api::RemoteFile>> {
    if let Some(existing) = client.find_child(&parent.id, name)? {
        if !existing.is_folder() {
            bail!(
                "ya existe un archivo llamado '{name}' en '{}'; no se puede usar como carpeta",
                parent.name
            );
        }
        return Ok(Some(existing));
    }
    if dry_run {
        println!("haría[crear-carpeta] {name}/");
        return Ok(None);
    }
    let created = client.create_folder(name, &parent.id)?;
    if !quiet {
        println!("[carpeta] {name}/ — {}", created.id);
    }
    Ok(Some(created))
}

fn cmd_download(
    global: &Global,
    client: &DriveClient,
    reference: String,
    dest: Option<PathBuf>,
    overwrite: bool,
) -> Result<()> {
    let remote = client.resolve_ref(&reference)?;

    if remote.is_folder() {
        let dest = match dest {
            Some(path) => path,
            None => PathBuf::from(util::sanitize_component(&remote.name)),
        };
        if !global.dry_run {
            fs::create_dir_all(&dest)
                .with_context(|| format!("no se pudo crear {}", dest.display()))?;
        } else {
            println!("haría[bajar-carpeta] {} → {}", remote.name, dest.display());
        }
        download_tree(
            client,
            &remote,
            &dest,
            overwrite,
            global.dry_run,
            global.quiet,
        )?;
        return Ok(());
    }

    if !remote.is_syncable() {
        bail!(
            "'{}' no es un archivo descargable (los documentos nativos de Google no tienen contenido binario)",
            remote.name
        );
    }

    let destination = match dest {
        Some(path) if path.is_dir() => path.join(util::sanitize_component(&remote.name)),
        Some(path) => path,
        None => PathBuf::from(util::sanitize_component(&remote.name)),
    };

    if destination.exists() && !overwrite {
        let same = util::md5_file(&destination).ok().as_deref() == remote.md5_checksum.as_deref();
        if same {
            if !global.quiet {
                println!("[igual] {}", destination.display());
            }
            return Ok(());
        }
        bail!(
            "{} ya existe; usa --overwrite para reemplazarlo",
            destination.display()
        );
    }

    if global.dry_run {
        println!("haría[bajar] {} → {}", remote.name, destination.display());
        return Ok(());
    }

    let copied = client.download_to(&remote.id, &destination)?;
    verify_download(&destination, remote.md5_checksum.as_deref())?;
    if !global.quiet {
        println!(
            "[bajado] {} ({}) — {}",
            destination.display(),
            util::format_size(copied),
            remote.id
        );
    }
    Ok(())
}

fn verify_download(path: &Path, expected: Option<&str>) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let actual = util::md5_file(path)?;
    if actual != expected {
        let _ = fs::remove_file(path);
        bail!(
            "la descarga de {} no coincide con el MD5 de Drive (posible corrupción)",
            path.display()
        );
    }
    Ok(())
}

fn download_tree(
    client: &DriveClient,
    folder: &api::RemoteFile,
    dest: &Path,
    overwrite: bool,
    dry_run: bool,
    quiet: bool,
) -> Result<()> {
    let mut children = client.list_children(&folder.id)?;
    children.sort_by(|a, b| a.name.cmp(&b.name));

    for child in children {
        let name = util::sanitize_component(&child.name);
        let target = dest.join(&name);

        if child.is_folder() {
            if !dry_run {
                fs::create_dir_all(&target)
                    .with_context(|| format!("no se pudo crear {}", target.display()))?;
            }
            download_tree(client, &child, &target, overwrite, dry_run, quiet)?;
            continue;
        }
        if !child.is_syncable() {
            eprintln!("[aviso] omitido (documento nativo de Google): {name}");
            continue;
        }
        let same = target.exists()
            && util::md5_file(&target).ok().as_deref() == child.md5_checksum.as_deref();
        if same {
            if !quiet {
                println!("[igual] {name}");
            }
            continue;
        }
        if target.exists() && !overwrite {
            eprintln!("[aviso] {name} existe y es distinto; usa --overwrite para reemplazarlo");
            continue;
        }
        if dry_run {
            println!("haría[bajar] {name}");
            continue;
        }
        client.download_to(&child.id, &target)?;
        verify_download(&target, child.md5_checksum.as_deref())?;
        if !quiet {
            println!(
                "[bajado] {name} ({}) — {}",
                util::format_size(child.size_or_zero()),
                child.id
            );
        }
    }
    Ok(())
}

fn cmd_mkdir(
    global: &Global,
    client: &DriveClient,
    name: String,
    parent: Option<String>,
    literal: bool,
) -> Result<()> {
    // Primero se crea (o localiza) el contenedor indicado con --folder.
    let base = match parent {
        Some(reference) => client.resolve_ref(&reference)?,
        None => client.root(),
    };
    if !base.is_folder() {
        bail!("'{}' no es una carpeta", base.name);
    }

    // El nombre puede traer la ruta completa: `Recomp/NSW 2` o `Recomp\NSW 2`.
    let parts = if literal {
        if name.contains('/') {
            bail!(
                "Drive no permite '/' en el nombre de una carpeta; \
                 quita --literal para crear subcarpetas con 'Recomp/NSW 2'"
            );
        }
        vec![name.trim().to_string()]
    } else {
        util::split_folder_path(&name)?
    };
    if parts.len() == 1 && parts[0].is_empty() {
        bail!("el nombre de la carpeta no puede estar vacío");
    }

    // Recorre los componentes creando los que falten (semántica de `mkdir -p`).
    let mut current = base.clone();
    let mut recorrido: Vec<String> = Vec::new();

    for part in &parts {
        recorrido.push(part.clone());
        let ruta = recorrido.join("/");

        match client.find_child(&current.id, part)? {
            Some(existing) if existing.is_folder() => current = existing,
            Some(_) => bail!(
                "ya existe un archivo llamado '{}' en '{}'; no se puede usar como carpeta",
                part,
                if recorrido.len() == 1 {
                    base.name.clone()
                } else {
                    recorrido[..recorrido.len() - 1].join("/")
                }
            ),
            None => {
                if global.dry_run {
                    println!("haría[crear-carpeta] {ruta}/");
                    // Sin ID real no se puede seguir bajando: se anota el resto.
                    for resto in &parts[recorrido.len()..] {
                        recorrido.push(resto.clone());
                        println!("haría[crear-carpeta] {}/", recorrido.join("/"));
                    }
                    return Ok(());
                }
                let creada = client.create_folder(part, &current.id)?;
                println!("Carpeta creada: {ruta}/ — {}", creada.id);
                current = creada;
            }
        }
    }

    if !global.quiet {
        println!("Listo: {}/ dentro de '{}'", recorrido.join("/"), base.name);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_sync(
    global: &Global,
    client: &DriveClient,
    root: PathBuf,
    remote: Option<String>,
    direction: Direction,
    delete: bool,
    conflict: ConflictPolicy,
    exclude: Vec<String>,
    no_state: bool,
) -> Result<()> {
    if !root.is_dir() {
        bail!("{} no es un directorio", root.display());
    }
    let reference = match remote {
        Some(reference) => reference,
        None => client
            .config()
            .default_remote
            .context("falta --remote: indica la carpeta de Drive (ID, ruta o nombre)")?,
    };
    let remote_folder = client.resolve_ref(&reference)?;
    if !remote_folder.is_folder() {
        bail!("'{}' no es una carpeta de Drive", reference);
    }

    let mut state = sync::load_state(&root, !no_state)?;
    if !state.remote_root.is_empty() && state.remote_root != remote_folder.id {
        eprintln!(
            "[aviso] el estado guardado pertenece a otra carpeta remota ({}); se ignora",
            state.remote_root
        );
        state = sync::State::default();
    }
    state.remote_root = remote_folder.id.clone();

    if !global.quiet {
        println!(
            "Sincronizando {} ↔ {}/ ({}, dirección: {})",
            root.display(),
            remote_folder.name,
            remote_folder.id,
            match direction {
                Direction::Both => "ambos sentidos",
                Direction::Push => "local → remoto",
                Direction::Pull => "remoto → local",
            }
        );
    }

    let local = sync::scan_local(&root, &exclude)?;
    for note in &local.notes {
        println!("[aviso] {note}");
    }
    let remote_tree = sync::scan_remote(client, &remote_folder.id, &exclude)?;
    for note in &remote_tree.notes {
        println!("[aviso] {note}");
    }

    if !global.quiet {
        println!(
            "Local: {} archivo(s) · Remoto: {} archivo(s)",
            local.files.len(),
            remote_tree.files.len()
        );
    }

    let options = SyncOptions {
        direction,
        delete,
        conflict,
        dry_run: global.dry_run,
        verbose: global.verbose,
        quiet: global.quiet,
    };
    let plan = sync::build_plan(&local, &remote_tree, &state, &options);
    let planned = plan.actions.len();
    let report = sync::Syncer::new(
        client,
        &local,
        remote_tree,
        state,
        root.clone(),
        remote_folder.id.clone(),
        options,
    )
    .run(plan);

    println!(
        "Resumen: {} subidos, {} bajados, {} eliminados, {} conflictos, {} errores ({planned} acciones planificadas)",
        report.uploaded,
        report.downloaded,
        report.deleted,
        report.conflicts,
        report.errors.len()
    );
    if global.dry_run {
        println!("(simulación: no se modificó nada)");
    }
    if !report.errors.is_empty() {
        bail!("{} operación(es) fallaron", report.errors.len());
    }
    Ok(())
}

fn cmd_logout(global: &Global) -> Result<()> {
    let (mut config, path) = config::load_config(global.config.as_deref())?;
    let had_tokens = config.access_token.is_some() || config.refresh_token.is_some();
    config.access_token = None;
    config.access_token_expiry = None;
    config.refresh_token = None;
    config::save_config(&config, &path)?;
    if had_tokens {
        println!("Tokens eliminados de {}", path.display());
    } else {
        println!("No había tokens guardados en {}", path.display());
    }
    Ok(())
}

fn file_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("no se pudo obtener el nombre de {}", path.display()))
}
