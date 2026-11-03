//! Motor de sincronización: escaneo local/remoto, plan de tres vías y ejecución.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::api::{DriveClient, RemoteFile};
use crate::util;

pub const STATE_DIR: &str = ".drivesync";
const STATE_FILE: &str = "state.json";
const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Direction {
    /// Sube y baja cambios en ambos sentidos (tres vías)
    Both,
    /// Solo local → remoto
    Push,
    /// Solo remoto → local
    Pull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[allow(clippy::enum_variant_names)]
pub enum ConflictPolicy {
    /// Conserva ambas versiones (local en el remoto, remota como copia local)
    KeepBoth,
    /// Gana la versión local
    KeepLocal,
    /// Gana la versión remota
    KeepRemote,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum DeleteMode {
    /// No propaga borrados: solo avisa de lo que falta en cada lado
    Never,
    /// Borra en Drive lo que ya no existe en local (por defecto)
    #[default]
    Remote,
    /// Borra en local lo que ya no existe en Drive
    Local,
    /// Propaga borrados en ambos sentidos
    Both,
}

impl DeleteMode {
    /// ¿Se mueve a la papelera de Drive lo que falta en local?
    pub fn trashes_remote(self) -> bool {
        matches!(self, Self::Remote | Self::Both)
    }

    /// ¿Se borra en local lo que falta en Drive?
    pub fn deletes_local(self) -> bool {
        matches!(self, Self::Local | Self::Both)
    }
}

#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub direction: Direction,
    pub delete: DeleteMode,
    pub conflict: ConflictPolicy,
    pub dry_run: bool,
    pub verbose: bool,
    pub quiet: bool,
}

#[derive(Debug, Clone)]
pub struct LocalFile {
    pub path: PathBuf,
    pub size: u64,
    pub md5: String,
}

#[derive(Debug, Default, Clone)]
pub struct LocalTree {
    pub files: BTreeMap<String, LocalFile>,
    pub dirs: BTreeSet<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct RemoteTree {
    pub files: BTreeMap<String, RemoteFile>,
    /// Ruta relativa de carpeta → ID de Drive. La raíz es `""`.
    pub dirs: BTreeMap<String, String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Entry {
    #[serde(default)]
    pub local_md5: Option<String>,
    #[serde(default)]
    pub local_size: Option<u64>,
    #[serde(default)]
    pub remote_id: String,
    #[serde(default)]
    pub remote_md5: Option<String>,
    #[serde(default)]
    pub synced_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    #[serde(default)]
    pub remote_root: String,
    #[serde(default)]
    pub entries: BTreeMap<String, Entry>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            remote_root: String::new(),
            entries: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    UploadNew { rel: String },
    UploadUpdate { rel: String, remote_id: String },
    DownloadNew { rel: String, remote_id: String },
    DownloadUpdate { rel: String, remote_id: String },
    DeleteLocal { rel: String },
    TrashRemote { rel: String, remote_id: String },
    Conflict { rel: String, remote_id: String },
    MarkSynced { rel: String },
}

#[derive(Debug, Default)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub notes: Vec<String>,
    pub prune: Vec<String>,
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub uploaded: usize,
    pub downloaded: usize,
    pub deleted: usize,
    pub conflicts: usize,
    pub errors: Vec<String>,
}

/// Escanea el árbol local calculando MD5 de cada archivo.
pub fn scan_local(root: &Path, excludes: &[String]) -> Result<LocalTree> {
    if !root.is_dir() {
        bail!("{} no es un directorio", root.display());
    }
    let mut tree = LocalTree::default();
    let mut stack = vec![(root.to_path_buf(), String::new())];

    while let Some((dir, prefix)) = stack.pop() {
        let entries = fs::read_dir(&dir)
            .with_context(|| format!("no se pudo leer el directorio {}", dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("error leyendo {}", dir.display()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };

            if prefix.is_empty() && name == STATE_DIR {
                continue;
            }
            if util::is_excluded(&rel, excludes) {
                tree.notes.push(format!("excluido: {rel}"));
                continue;
            }

            let file_type = entry
                .file_type()
                .with_context(|| format!("no se pudo determinar el tipo de {rel}"))?;

            if file_type.is_symlink() {
                tree.notes
                    .push(format!("omitido (enlace simbólico): {rel}"));
            } else if file_type.is_dir() {
                tree.dirs.insert(rel.clone());
                stack.push((entry.path(), rel));
            } else if file_type.is_file() {
                let size = entry
                    .metadata()
                    .with_context(|| format!("no se pudo leer metadatos de {rel}"))?
                    .len();
                let md5 = util::md5_file(&entry.path())?;
                tree.files.insert(
                    rel,
                    LocalFile {
                        path: entry.path(),
                        size,
                        md5,
                    },
                );
            } else {
                tree.notes
                    .push(format!("omitido (no es archivo regular): {rel}"));
            }
        }
    }
    Ok(tree)
}

/// Lista recursivamente el subárbol remoto, aplicando las mismas exclusiones.
pub fn scan_remote(client: &DriveClient, root_id: &str, excludes: &[String]) -> Result<RemoteTree> {
    let mut tree = RemoteTree::default();
    tree.dirs.insert(String::new(), root_id.to_string());
    let mut stack = vec![(root_id.to_string(), String::new())];

    while let Some((folder_id, prefix)) = stack.pop() {
        for child in client.list_children(&folder_id)? {
            let name = util::sanitize_component(&child.name);
            if name.is_empty() {
                tree.notes
                    .push(format!("omitido (nombre vacío) id={}", child.id));
                continue;
            }
            if name != child.name {
                tree.notes.push(format!(
                    "el nombre remoto '{}' contiene '/' y se tratará como '{}'",
                    child.name, name
                ));
            }
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            if util::is_excluded(&rel, excludes) {
                tree.notes.push(format!("excluido en remoto: {rel}"));
                continue;
            }

            if child.is_folder() {
                tree.dirs.insert(rel.clone(), child.id.clone());
                stack.push((child.id.clone(), rel));
            } else if child.is_shortcut() {
                tree.notes.push(format!("omitido (acceso directo): {rel}"));
            } else if child.is_google_native() {
                tree.notes.push(format!(
                    "omitido (documento nativo de Google, sin MD5): {rel}"
                ));
            } else if !child.is_syncable() {
                tree.notes
                    .push(format!("omitido (tipo no soportado): {rel}"));
            } else if child.md5_checksum.is_none() {
                tree.notes
                    .push(format!("omitido (Drive no reporta MD5): {rel}"));
            } else {
                match tree.files.entry(rel.clone()) {
                    std::collections::btree_map::Entry::Occupied(_) => {
                        tree.notes.push(format!(
                            "nombre duplicado en Drive, se usa el primero: {rel}"
                        ));
                    }
                    std::collections::btree_map::Entry::Vacant(slot) => {
                        slot.insert(child);
                    }
                }
            }
        }
    }
    Ok(tree)
}

pub fn state_path(root: &Path) -> PathBuf {
    root.join(STATE_DIR).join(STATE_FILE)
}

pub fn load_state(root: &Path, use_state: bool) -> Result<State> {
    if !use_state {
        return Ok(State::default());
    }
    let path = state_path(root);
    if !path.exists() {
        return Ok(State::default());
    }
    let text = fs::read_to_string(&path)
        .with_context(|| format!("no se pudo leer el estado {}", path.display()))?;
    let state: State = serde_json::from_str(&text).with_context(|| {
        format!(
            "estado corrupto en {} (bórralo para reiniciar)",
            path.display()
        )
    })?;
    if state.version != STATE_VERSION {
        eprintln!(
            "[aviso] el estado tiene versión {} (se esperaba {}); se ignora",
            state.version, STATE_VERSION
        );
        return Ok(State::default());
    }
    Ok(state)
}

pub fn save_state(root: &Path, state: &State) -> Result<()> {
    let dir = root.join(STATE_DIR);
    fs::create_dir_all(&dir).with_context(|| format!("no se pudo crear {}", dir.display()))?;
    let path = dir.join(STATE_FILE);
    let tmp = dir.join("state.json.tmp");
    let text = serde_json::to_string_pretty(state)?;
    fs::write(&tmp, text).with_context(|| format!("no se pudo escribir {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("no se pudo guardar {}", path.display()))?;
    Ok(())
}

/// Construye el plan de acciones comparando local, remoto y el estado previo.
pub fn build_plan(
    local: &LocalTree,
    remote: &RemoteTree,
    state: &State,
    options: &SyncOptions,
) -> Plan {
    let mut plan = Plan::default();
    let mut paths: BTreeSet<String> = BTreeSet::new();
    paths.extend(local.files.keys().cloned());
    paths.extend(remote.files.keys().cloned());
    paths.extend(state.entries.keys().cloned());

    for rel in paths {
        let local_file = local.files.get(&rel);
        let remote_file = remote.files.get(&rel);
        let base = state.entries.get(&rel);

        let remote_md5 = remote_file
            .and_then(|f| f.md5_checksum.as_deref())
            .unwrap_or("");

        let action = match options.direction {
            Direction::Push => match (local_file, remote_file) {
                (Some(local_file), Some(remote_file)) => {
                    if local_file.md5 == remote_md5 {
                        Some(Action::MarkSynced { rel: rel.clone() })
                    } else {
                        Some(Action::UploadUpdate {
                            rel: rel.clone(),
                            remote_id: remote_file.id.clone(),
                        })
                    }
                }
                (Some(_), None) => Some(Action::UploadNew { rel: rel.clone() }),
                (None, Some(remote_file)) => {
                    // Solo se borra en Drive lo que el estado registra como
                    // sincronizado y sigue intacto: nunca contenido ajeno.
                    let synced = base
                        .map(|entry| entry.remote_md5.as_deref() == Some(remote_md5))
                        .unwrap_or(false);
                    if synced && options.delete.trashes_remote() {
                        Some(Action::TrashRemote {
                            rel: rel.clone(),
                            remote_id: remote_file.id.clone(),
                        })
                    } else {
                        plan.notes.push(if synced {
                            format!("solo en remoto: {rel} (usa --delete para eliminarlo de Drive)")
                        } else {
                            format!(
                                "solo en remoto: {rel} (no está en el estado; no se borra de Drive)"
                            )
                        });
                        None
                    }
                }
                (None, None) => None,
            },
            Direction::Pull => match (local_file, remote_file) {
                (Some(local_file), Some(remote_file)) => {
                    if local_file.md5 == remote_md5 {
                        Some(Action::MarkSynced { rel: rel.clone() })
                    } else {
                        Some(Action::DownloadUpdate {
                            rel: rel.clone(),
                            remote_id: remote_file.id.clone(),
                        })
                    }
                }
                (None, Some(remote_file)) => Some(Action::DownloadNew {
                    rel: rel.clone(),
                    remote_id: remote_file.id.clone(),
                }),
                (Some(_), None) => {
                    if options.delete.deletes_local() {
                        Some(Action::DeleteLocal { rel: rel.clone() })
                    } else {
                        plan.notes.push(format!(
                            "solo en local: {rel} (usa --delete para eliminarlo)"
                        ));
                        None
                    }
                }
                (None, None) => None,
            },
            Direction::Both => match (local_file, remote_file) {
                (Some(local_file), Some(remote_file)) => {
                    if local_file.md5 == remote_md5 {
                        Some(Action::MarkSynced { rel: rel.clone() })
                    } else {
                        let local_changed = base
                            .map(|b| b.local_md5.as_deref() != Some(local_file.md5.as_str()))
                            .unwrap_or(true);
                        let remote_changed = base
                            .map(|b| b.remote_md5.as_deref() != Some(remote_md5))
                            .unwrap_or(true);
                        match (local_changed, remote_changed) {
                            (true, false) => Some(Action::UploadUpdate {
                                rel: rel.clone(),
                                remote_id: remote_file.id.clone(),
                            }),
                            (false, true) => Some(Action::DownloadUpdate {
                                rel: rel.clone(),
                                remote_id: remote_file.id.clone(),
                            }),
                            (true, true) => Some(Action::Conflict {
                                rel: rel.clone(),
                                remote_id: remote_file.id.clone(),
                            }),
                            (false, false) => {
                                plan.notes.push(format!(
                                    "estado inconsistente para {rel}; se trata como conflicto"
                                ));
                                Some(Action::Conflict {
                                    rel: rel.clone(),
                                    remote_id: remote_file.id.clone(),
                                })
                            }
                        }
                    }
                }
                (Some(local_file), None) => match base {
                    Some(entry)
                        if !entry.remote_id.is_empty()
                            && entry.local_md5.as_deref() == Some(local_file.md5.as_str()) =>
                    {
                        if options.delete.deletes_local() {
                            Some(Action::DeleteLocal { rel: rel.clone() })
                        } else {
                            plan.notes.push(format!(
                                "eliminado en remoto: {rel} (usa --delete para borrarlo en local)"
                            ));
                            None
                        }
                    }
                    _ => Some(Action::UploadNew { rel: rel.clone() }),
                },
                (None, Some(remote_file)) => match base {
                    Some(entry) if entry.remote_md5.as_deref() == Some(remote_md5) => {
                        if options.delete.trashes_remote() {
                            Some(Action::TrashRemote {
                                rel: rel.clone(),
                                remote_id: remote_file.id.clone(),
                            })
                        } else {
                            plan.notes.push(format!(
                                "eliminado en local: {rel} (usa --delete para quitarlo de Drive)"
                            ));
                            None
                        }
                    }
                    _ => Some(Action::DownloadNew {
                        rel: rel.clone(),
                        remote_id: remote_file.id.clone(),
                    }),
                },
                (None, None) => None,
            },
        };

        if let Some(action) = action {
            plan.actions.push(action);
        }
        if local_file.is_none() && remote_file.is_none() {
            plan.prune.push(rel);
        }
    }

    plan.actions
        .sort_by(|a, b| action_key(a).cmp(&action_key(b)));
    plan
}

fn action_key(action: &Action) -> (u8, &str) {
    let order = match action {
        Action::UploadNew { .. } => 0,
        Action::UploadUpdate { .. } => 1,
        Action::DownloadNew { .. } => 2,
        Action::DownloadUpdate { .. } => 3,
        Action::DeleteLocal { .. } => 4,
        Action::TrashRemote { .. } => 5,
        Action::Conflict { .. } => 6,
        Action::MarkSynced { .. } => 7,
    };
    let rel = match action {
        Action::UploadNew { rel }
        | Action::UploadUpdate { rel, .. }
        | Action::DownloadNew { rel, .. }
        | Action::DownloadUpdate { rel, .. }
        | Action::DeleteLocal { rel }
        | Action::TrashRemote { rel, .. }
        | Action::Conflict { rel, .. }
        | Action::MarkSynced { rel } => rel,
    };
    (order, rel.as_str())
}

pub struct Syncer<'a> {
    client: &'a DriveClient,
    local: &'a LocalTree,
    remote: RemoteTree,
    state: State,
    root: PathBuf,
    remote_root: String,
    options: SyncOptions,
    unsaved: usize,
    report: SyncReport,
}

impl<'a> Syncer<'a> {
    pub fn new(
        client: &'a DriveClient,
        local: &'a LocalTree,
        remote: RemoteTree,
        state: State,
        root: PathBuf,
        remote_root: String,
        options: SyncOptions,
    ) -> Self {
        Self {
            client,
            local,
            remote,
            state,
            root,
            remote_root,
            options,
            unsaved: 0,
            report: SyncReport::default(),
        }
    }

    pub fn run(mut self, plan: Plan) -> SyncReport {
        for note in &plan.notes {
            println!("[aviso] {note}");
        }
        for action in &plan.actions {
            if self.options.dry_run {
                self.print_action(action, true);
                continue;
            }
            let label = action_label(action);
            match self.apply(action) {
                Ok(()) => self.count(action),
                Err(error) => {
                    let message = format!("{label}: {error:#}");
                    eprintln!("[error] {message}");
                    self.report.errors.push(message);
                }
            }
        }

        if !self.options.dry_run {
            for rel in &plan.prune {
                self.state.entries.remove(rel);
            }
            if let Err(error) = save_state(&self.root, &self.state) {
                let message = format!("no se pudo guardar el estado: {error:#}");
                eprintln!("[error] {message}");
                self.report.errors.push(message);
            }
        }

        self.report
    }

    fn count(&mut self, action: &Action) {
        match action {
            Action::UploadNew { .. } | Action::UploadUpdate { .. } => self.report.uploaded += 1,
            Action::DownloadNew { .. } | Action::DownloadUpdate { .. } => {
                self.report.downloaded += 1
            }
            Action::DeleteLocal { .. } | Action::TrashRemote { .. } => self.report.deleted += 1,
            Action::Conflict { .. } => self.report.conflicts += 1,
            Action::MarkSynced { .. } => {}
        }
    }

    fn apply(&mut self, action: &Action) -> Result<()> {
        match action {
            Action::UploadNew { rel } => {
                self.print_action(action, false);
                self.upload(rel, None)
            }
            Action::UploadUpdate { rel, remote_id } => {
                self.print_action(action, false);
                self.upload(rel, Some(remote_id))
            }
            Action::DownloadNew { rel, remote_id } => {
                self.print_action(action, false);
                self.download(rel, remote_id)
            }
            Action::DownloadUpdate { rel, remote_id } => {
                self.print_action(action, false);
                self.download(rel, remote_id)
            }
            Action::DeleteLocal { rel } => {
                self.print_action(action, false);
                let path = self.root.join(rel);
                fs::remove_file(&path)
                    .with_context(|| format!("no se pudo borrar {}", path.display()))?;
                self.state.entries.remove(rel);
                self.remote.files.remove(rel);
                self.save_soon();
                Ok(())
            }
            Action::TrashRemote { rel, remote_id } => {
                self.print_action(action, false);
                self.client.trash_file(remote_id)?;
                self.state.entries.remove(rel);
                self.remote.files.remove(rel);
                self.save_soon();
                Ok(())
            }
            Action::Conflict { rel, remote_id } => {
                self.print_action(action, false);
                self.resolve_conflict(rel, remote_id)
            }
            Action::MarkSynced { rel } => {
                if self.options.verbose {
                    self.print_action(action, false);
                }
                self.mark_synced(rel);
                self.save_soon();
                Ok(())
            }
        }
    }

    fn print_action(&self, action: &Action, simulated: bool) {
        if self.options.quiet {
            return;
        }
        let prefix = if simulated { "haría" } else { "" };
        match action {
            Action::UploadNew { rel } => {
                let size = self
                    .local
                    .files
                    .get(rel)
                    .map(|f| format!(" ({})", util::format_size(f.size)))
                    .unwrap_or_default();
                println!("{prefix}[subir] {rel}{size}");
            }
            Action::UploadUpdate { rel, .. } => println!("{prefix}[actualizar-remoto] {rel}"),
            Action::DownloadNew { rel, .. } => println!("{prefix}[bajar] {rel}"),
            Action::DownloadUpdate { rel, .. } => println!("{prefix}[actualizar-local] {rel}"),
            Action::DeleteLocal { rel } => println!("{prefix}[borrar-local] {rel}"),
            Action::TrashRemote { rel, .. } => println!("{prefix}[borrar-remoto] {rel}"),
            Action::Conflict { rel, .. } => {
                println!("{prefix}[conflicto] {rel}");
            }
            Action::MarkSynced { rel } => println!("{prefix}[igual] {rel}"),
        }
    }

    fn upload(&mut self, rel: &str, remote_id: Option<&str>) -> Result<()> {
        let local = self
            .local
            .files
            .get(rel)
            .with_context(|| format!("no hay copia local de {rel}"))?;
        let mime = util::mime_for(&local.path);
        let (parent_rel, name) = util::split_rel(rel);

        let mut file = match remote_id {
            Some(id) => self.client.update_file(id, mime, &local.path)?,
            None => {
                let parent_id = self.ensure_dir(parent_rel)?;
                self.client
                    .create_file(&parent_id, name, mime, &local.path)?
            }
        };

        if file.md5_checksum.is_none() {
            file.md5_checksum = Some(local.md5.clone());
        }
        file.size = Some(local.size);
        self.remote.files.insert(rel.to_string(), file.clone());

        self.state.entries.insert(
            rel.to_string(),
            Entry {
                local_md5: Some(local.md5.clone()),
                local_size: Some(local.size),
                remote_id: file.id,
                remote_md5: Some(local.md5.clone()),
                synced_at: util::now_unix(),
            },
        );
        self.save_soon();
        Ok(())
    }

    fn download(&mut self, rel: &str, remote_id: &str) -> Result<()> {
        let remote = self
            .remote
            .files
            .get(rel)
            .cloned()
            .with_context(|| format!("no hay copia remota de {rel}"))?;
        let destination = self.root.join(rel);
        let copied = self.client.download_to(remote_id, &destination)?;
        if let Some(expected) = remote.size {
            if expected != copied {
                eprintln!(
                    "[aviso] {rel}: se esperaban {} y se descargaron {}",
                    util::format_size(expected),
                    util::format_size(copied)
                );
            }
        }
        let local_md5 = util::md5_file(&destination)?;
        if let Some(expected) = remote.md5_checksum.as_deref() {
            if local_md5 != expected {
                let _ = fs::remove_file(&destination);
                bail!(
                    "la descarga de {rel} no coincide con el MD5 de Drive (¿corrupción o cambio durante la descarga?)"
                );
            }
        }
        let md5 = local_md5;

        self.state.entries.insert(
            rel.to_string(),
            Entry {
                local_md5: Some(md5.clone()),
                local_size: Some(copied),
                remote_id: remote.id,
                remote_md5: Some(md5),
                synced_at: util::now_unix(),
            },
        );
        self.save_soon();
        Ok(())
    }

    fn mark_synced(&mut self, rel: &str) {
        let (Some(local), Some(remote)) = (self.local.files.get(rel), self.remote.files.get(rel))
        else {
            return;
        };
        self.state.entries.insert(
            rel.to_string(),
            Entry {
                local_md5: Some(local.md5.clone()),
                local_size: Some(local.size),
                remote_id: remote.id.clone(),
                remote_md5: remote.md5_checksum.clone(),
                synced_at: util::now_unix(),
            },
        );
    }

    fn resolve_conflict(&mut self, rel: &str, remote_id: &str) -> Result<()> {
        match self.options.conflict {
            ConflictPolicy::KeepLocal => self.upload(rel, Some(remote_id)),
            ConflictPolicy::KeepRemote => self.download(rel, remote_id),
            ConflictPolicy::KeepBoth => self.keep_both(rel, remote_id),
        }
    }

    fn keep_both(&mut self, rel: &str, remote_id: &str) -> Result<()> {
        let remote = self
            .remote
            .files
            .get(rel)
            .cloned()
            .with_context(|| format!("no hay copia remota de {rel}"))?;
        let (parent_rel, name) = util::split_rel(rel);
        let (stem, extension) = split_extension(name);
        let stamp = util::format_utc_compact(util::now_unix());
        let conflict_name = format!("{stem}.conflicto-remoto-{stamp}{extension}");
        let conflict_rel = if parent_rel.is_empty() {
            conflict_name.clone()
        } else {
            format!("{parent_rel}/{conflict_name}")
        };
        let conflict_path = self.root.join(&conflict_rel);
        if conflict_path.exists() {
            bail!("ya existe {}", conflict_path.display());
        }

        let copied = self.client.download_to(remote_id, &conflict_path)?;

        let local = self
            .local
            .files
            .get(rel)
            .with_context(|| format!("no hay copia local de {rel}"))?
            .clone();
        let mut updated =
            self.client
                .update_file(remote_id, util::mime_for(&local.path), &local.path)?;
        updated.md5_checksum = Some(local.md5.clone());
        updated.size = Some(local.size);
        self.remote.files.insert(rel.to_string(), updated.clone());
        self.state.entries.insert(
            rel.to_string(),
            Entry {
                local_md5: Some(local.md5.clone()),
                local_size: Some(local.size),
                remote_id: updated.id,
                remote_md5: Some(local.md5),
                synced_at: util::now_unix(),
            },
        );

        let parent_id = self.ensure_dir(parent_rel)?;
        let created = self.client.create_file(
            &parent_id,
            &conflict_name,
            util::mime_for(&conflict_path),
            &conflict_path,
        )?;
        let remote_md5 = remote
            .md5_checksum
            .clone()
            .or_else(|| util::md5_file(&conflict_path).ok());
        self.state.entries.insert(
            conflict_rel.clone(),
            Entry {
                local_md5: remote_md5.clone(),
                local_size: Some(copied),
                remote_id: created.id.clone(),
                remote_md5,
                synced_at: util::now_unix(),
            },
        );
        self.remote.files.insert(conflict_rel, created);
        self.save_soon();
        Ok(())
    }

    fn ensure_dir(&mut self, rel_dir: &str) -> Result<String> {
        if rel_dir.is_empty() {
            return Ok(self.remote_root.clone());
        }
        if let Some(id) = self.remote.dirs.get(rel_dir) {
            return Ok(id.clone());
        }
        let (parent_rel, name) = util::split_rel(rel_dir);
        let parent_id = self.ensure_dir(parent_rel)?;
        let folder = self.client.create_folder(name, &parent_id)?;
        self.remote
            .dirs
            .insert(rel_dir.to_string(), folder.id.clone());
        Ok(folder.id)
    }

    fn save_soon(&mut self) {
        self.unsaved += 1;
        if self.unsaved >= 20 {
            self.unsaved = 0;
            if let Err(error) = save_state(&self.root, &self.state) {
                eprintln!("[aviso] no se pudo guardar el estado: {error:#}");
            }
        }
    }
}

fn action_label(action: &Action) -> String {
    match action {
        Action::UploadNew { rel } => format!("subir {rel}"),
        Action::UploadUpdate { rel, .. } => format!("actualizar remoto {rel}"),
        Action::DownloadNew { rel, .. } => format!("bajar {rel}"),
        Action::DownloadUpdate { rel, .. } => format!("actualizar local {rel}"),
        Action::DeleteLocal { rel } => format!("borrar local {rel}"),
        Action::TrashRemote { rel, .. } => format!("borrar remoto {rel}"),
        Action::Conflict { rel, .. } => format!("resolver conflicto {rel}"),
        Action::MarkSynced { rel } => format!("marcar {rel}"),
    }
}

fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(index) if index > 0 => name.split_at(index),
        _ => (name, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(md5: &str) -> LocalFile {
        LocalFile {
            path: PathBuf::from("archivo.txt"),
            size: 10,
            md5: md5.to_string(),
        }
    }

    fn remote(id: &str, md5: &str) -> RemoteFile {
        RemoteFile {
            id: id.to_string(),
            name: "archivo.txt".into(),
            mime_type: "text/plain".into(),
            md5_checksum: Some(md5.to_string()),
            size: Some(10),
            ..Default::default()
        }
    }

    fn tree(
        local_files: Vec<(&str, LocalFile)>,
        remote_files: Vec<(&str, RemoteFile)>,
    ) -> (LocalTree, RemoteTree) {
        let mut local_tree = LocalTree::default();
        for (rel, file) in local_files {
            local_tree.files.insert(rel.to_string(), file);
        }
        let mut remote_tree = RemoteTree::default();
        for (rel, file) in remote_files {
            remote_tree.files.insert(rel.to_string(), file);
        }
        (local_tree, remote_tree)
    }

    fn options(direction: Direction, delete: DeleteMode) -> SyncOptions {
        SyncOptions {
            direction,
            delete,
            conflict: ConflictPolicy::KeepBoth,
            dry_run: false,
            verbose: false,
            quiet: false,
        }
    }

    fn state_with(rel: &str, local_md5: &str, remote_md5: &str) -> State {
        let mut state = State::default();
        state.entries.insert(
            rel.to_string(),
            Entry {
                local_md5: Some(local_md5.to_string()),
                local_size: Some(10),
                remote_id: "id-1".to_string(),
                remote_md5: Some(remote_md5.to_string()),
                synced_at: 0,
            },
        );
        state
    }

    #[test]
    fn sube_archivo_nuevo() {
        let (local, remote) = tree(vec![("a.txt", local("aaa"))], vec![]);
        let plan = build_plan(
            &local,
            &remote,
            &State::default(),
            &options(Direction::Both, DeleteMode::Never),
        );
        assert!(matches!(
            plan.actions.as_slice(),
            [Action::UploadNew { .. }]
        ));
    }

    #[test]
    fn archivos_iguales_no_hacen_nada() {
        let (local, remote) = tree(
            vec![("a.txt", local("aaa"))],
            vec![("a.txt", remote("id", "aaa"))],
        );
        let plan = build_plan(
            &local,
            &remote,
            &State::default(),
            &options(Direction::Both, DeleteMode::Never),
        );
        assert!(matches!(
            plan.actions.as_slice(),
            [Action::MarkSynced { .. }]
        ));
    }

    #[test]
    fn baja_cambio_remoto() {
        let (local, remote) = tree(
            vec![("a.txt", local("aaa"))],
            vec![("a.txt", remote("id", "bbb"))],
        );
        let state = state_with("a.txt", "aaa", "aaa");
        let plan = build_plan(
            &local,
            &remote,
            &state,
            &options(Direction::Both, DeleteMode::Never),
        );
        assert!(matches!(
            plan.actions.as_slice(),
            [Action::DownloadUpdate { .. }]
        ));
    }

    #[test]
    fn sube_cambio_local() {
        let (local, remote) = tree(
            vec![("a.txt", local("bbb"))],
            vec![("a.txt", remote("id", "aaa"))],
        );
        let state = state_with("a.txt", "aaa", "aaa");
        let plan = build_plan(
            &local,
            &remote,
            &state,
            &options(Direction::Both, DeleteMode::Never),
        );
        assert!(matches!(
            plan.actions.as_slice(),
            [Action::UploadUpdate { .. }]
        ));
    }

    #[test]
    fn detecta_conflicto() {
        let (local, remote) = tree(
            vec![("a.txt", local("bbb"))],
            vec![("a.txt", remote("id", "ccc"))],
        );
        let state = state_with("a.txt", "aaa", "aaa");
        let plan = build_plan(
            &local,
            &remote,
            &state,
            &options(Direction::Both, DeleteMode::Never),
        );
        assert!(matches!(plan.actions.as_slice(), [Action::Conflict { .. }]));
    }

    #[test]
    fn borrado_remoto_con_delete() {
        let (local, remote) = tree(vec![("a.txt", local("aaa"))], vec![]);
        let state = state_with("a.txt", "aaa", "aaa");
        let plan = build_plan(
            &local,
            &remote,
            &state,
            &options(Direction::Both, DeleteMode::Both),
        );
        assert!(matches!(
            plan.actions.as_slice(),
            [Action::DeleteLocal { .. }]
        ));
    }

    #[test]
    fn borrado_remoto_sin_delete_avisa() {
        let (local, remote) = tree(vec![("a.txt", local("aaa"))], vec![]);
        let state = state_with("a.txt", "aaa", "aaa");
        let plan = build_plan(
            &local,
            &remote,
            &state,
            &options(Direction::Both, DeleteMode::Never),
        );
        assert!(plan.actions.is_empty());
        assert_eq!(plan.notes.len(), 1);
    }

    #[test]
    fn borrado_local_con_delete() {
        let (local, remote) = tree(vec![], vec![("a.txt", remote("id", "aaa"))]);
        let state = state_with("a.txt", "aaa", "aaa");
        let plan = build_plan(
            &local,
            &remote,
            &state,
            &options(Direction::Both, DeleteMode::Both),
        );
        assert!(matches!(
            plan.actions.as_slice(),
            [Action::TrashRemote { .. }]
        ));
    }

    #[test]
    fn borrado_local_se_propaga_a_drive_por_defecto() {
        let (local, remote) = tree(vec![], vec![("a.txt", remote("id", "aaa"))]);
        let state = state_with("a.txt", "aaa", "aaa");
        let plan = build_plan(
            &local,
            &remote,
            &state,
            &options(Direction::Both, DeleteMode::default()),
        );
        assert!(matches!(
            plan.actions.as_slice(),
            [Action::TrashRemote { .. }]
        ));
        assert!(plan.notes.is_empty());
    }

    #[test]
    fn borrado_remoto_no_toca_local_por_defecto() {
        let (local, remote) = tree(vec![("a.txt", local("aaa"))], vec![]);
        let state = state_with("a.txt", "aaa", "aaa");
        let plan = build_plan(
            &local,
            &remote,
            &state,
            &options(Direction::Both, DeleteMode::default()),
        );
        assert!(plan.actions.is_empty());
        assert_eq!(plan.notes.len(), 1);
        assert!(plan.notes[0].contains("eliminado en remoto"));
    }

    #[test]
    fn delete_local_solo_ignora_lo_que_falta_en_local() {
        let (local, remote) = tree(vec![], vec![("a.txt", remote("id", "aaa"))]);
        let state = state_with("a.txt", "aaa", "aaa");
        let plan = build_plan(
            &local,
            &remote,
            &state,
            &options(Direction::Both, DeleteMode::Local),
        );
        assert!(plan.actions.is_empty());
        assert_eq!(plan.notes.len(), 1);
    }

    #[test]
    fn solo_remoto_por_defecto_no_se_borra_sin_estado_previo() {
        let (local, remote) = tree(vec![], vec![("a.txt", remote("id", "aaa"))]);
        let plan = build_plan(
            &local,
            &remote,
            &State::default(),
            &options(Direction::Push, DeleteMode::default()),
        );
        assert!(plan.actions.is_empty());
        assert_eq!(plan.notes.len(), 1);
        assert!(plan.notes[0].contains("no está en el estado"));
    }

    #[test]
    fn push_borra_en_drive_lo_que_estaba_sincronizado() {
        let (local, remote) = tree(vec![], vec![("a.txt", remote("id", "aaa"))]);
        let state = state_with("a.txt", "aaa", "aaa");
        let plan = build_plan(
            &local,
            &remote,
            &state,
            &options(Direction::Push, DeleteMode::default()),
        );
        assert!(matches!(
            plan.actions.as_slice(),
            [Action::TrashRemote { .. }]
        ));
    }

    #[test]
    fn recrea_local_borrado_pero_modificado_en_remoto() {
        let (local, remote) = tree(vec![], vec![("a.txt", remote("id", "bbb"))]);
        let state = state_with("a.txt", "aaa", "aaa");
        let plan = build_plan(
            &local,
            &remote,
            &state,
            &options(Direction::Both, DeleteMode::Never),
        );
        assert!(matches!(
            plan.actions.as_slice(),
            [Action::DownloadNew { .. }]
        ));
    }

    #[test]
    fn push_ignora_cambios_remotos() {
        let (local, remote) = tree(
            vec![("a.txt", local("aaa"))],
            vec![("a.txt", remote("id", "ccc"))],
        );
        let state = state_with("a.txt", "aaa", "ccc");
        let plan = build_plan(
            &local,
            &remote,
            &state,
            &options(Direction::Push, DeleteMode::Never),
        );
        assert!(matches!(
            plan.actions.as_slice(),
            [Action::UploadUpdate { .. }]
        ));
    }

    #[test]
    fn pull_impone_version_remota() {
        let (local, remote) = tree(
            vec![("a.txt", local("aaa"))],
            vec![("a.txt", remote("id", "ccc"))],
        );
        let plan = build_plan(
            &local,
            &remote,
            &State::default(),
            &options(Direction::Pull, DeleteMode::Never),
        );
        assert!(matches!(
            plan.actions.as_slice(),
            [Action::DownloadUpdate { .. }]
        ));
    }

    #[test]
    fn purga_entradas_huerfanas() {
        let (local, remote) = tree(vec![], vec![]);
        let state = state_with("viejo.txt", "aaa", "aaa");
        let plan = build_plan(
            &local,
            &remote,
            &state,
            &options(Direction::Both, DeleteMode::Never),
        );
        assert_eq!(plan.prune, vec!["viejo.txt".to_string()]);
    }

    #[test]
    fn ordena_subidas_antes_que_borrados() {
        let (local, remote) = tree(
            vec![("nuevo.txt", local("bbb"))],
            vec![("viejo.txt", remote("id", "aaa"))],
        );
        let mut state = State::default();
        state.entries.insert(
            "viejo.txt".into(),
            Entry {
                local_md5: Some("aaa".into()),
                remote_id: "id".into(),
                remote_md5: Some("aaa".into()),
                ..Default::default()
            },
        );
        let plan = build_plan(
            &local,
            &remote,
            &state,
            &options(Direction::Both, DeleteMode::Both),
        );
        assert!(matches!(plan.actions[0], Action::UploadNew { .. }));
        assert!(matches!(plan.actions[1], Action::TrashRemote { .. }));
    }

    #[test]
    fn escanea_local_y_excluye() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/a.txt"), b"hola").unwrap();
        fs::write(dir.path().join("b.tmp"), b"basura").unwrap();
        fs::create_dir_all(dir.path().join(STATE_DIR)).unwrap();
        fs::write(dir.path().join(STATE_DIR).join("state.json"), b"{}").unwrap();

        let excludes = vec!["*.tmp".to_string()];
        let tree = scan_local(dir.path(), &excludes).unwrap();
        assert!(tree.files.contains_key("sub/a.txt"));
        assert!(!tree.files.contains_key("b.tmp"));
        assert!(!tree.files.iter().any(|(k, _)| k.starts_with(STATE_DIR)));
        assert_eq!(tree.dirs.len(), 1);
    }

    #[test]
    fn separa_extension() {
        assert_eq!(split_extension("nota.txt"), ("nota", ".txt"));
        assert_eq!(split_extension("Makefile"), ("Makefile", ""));
        assert_eq!(split_extension(".oculto"), (".oculto", ""));
    }
}
