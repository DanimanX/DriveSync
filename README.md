# DriveSync

[![CI](https://github.com/DanimanX/DriveSync/actions/workflows/ci.yml/badge.svg)](https://github.com/DanimanX/DriveSync/actions/workflows/ci.yml)

Versión 0.3.1 · CLI en Rust para sincronizar archivos y carpetas con Google Drive usando un
token de Google. Sin SDKs pesados: habla directamente con la API v3 de Drive
sobre HTTPS, guarda el estado de la sincronización y resuelve conflictos sin
perder ninguna versión.

```
drivesync auth --access-token "$(gcloud auth print-access-token)"
drivesync sync ./documentos --remote Backup/Documentos
```

## Características

- **Autenticación por token**: token de acceso directo, refresh token, flujo
  OAuth de escritorio con redirección a `127.0.0.1` o flujo manual
  (`--manual`: imprime la URL y pide el código o el token a mano).
- **Sincronización en tres vías** con un archivo de estado local: distingue
  "cambió en local", "cambió en remoto" y "borrado" en cada lado.
- **Subidas reanudables por bloques** (8 MiB) para archivos grandes.
- **Descargas verificadas** con el MD5 que reporta Drive.
- **Conflictos sin pérdida**: por defecto conserva la versión local y la remota
  (esta última como `archivo.conflicto-remoto-AAAAMMDD-HHMMSS.ext`).
- **Comandos sueltos**: `list`, `upload`, `download`, `mkdir`, `status`.
- **Tamaño de carpetas** en `list`: suma el subárbol completo
  (`[dir ] [3.0 GiB] Backup/`), con `--no-sizes` para evitarlo.
- **Simulación** con `--dry-run`, exclusiones con `--exclude` y direcciones
  `push`/`pull`/`both`.
- **Reintentos** con backoff para 429/5xx y renovación automática del token.

## Requisitos

- Rust 1.74 o superior (`cargo`).
- Una cuenta de Google y un cliente OAuth de tipo **Aplicación de escritorio**
  (solo si quieres refresh token; con un token de acceso temporal no hace falta).

## Instalar

### Binario precompilado (Linux)

Cada tag publica una release con los binarios de `x86_64` y `aarch64` en
<https://github.com/DanimanX/DriveSync/releases>.

```bash
tag=v0.3.1
arch=x86_64-unknown-linux-gnu        # o aarch64-unknown-linux-gnu
base="https://github.com/DanimanX/DriveSync/releases/download/$tag"

curl -fsSLO "$base/drivesync-${tag#v}-$arch"
curl -fsSLO "$base/SHA256SUMS-$arch.txt"
sha256sum -c "SHA256SUMS-$arch.txt"

install -m 755 "drivesync-${tag#v}-$arch" ~/.local/bin/drivesync
drivesync --version
```

### Desde el código

```bash
cargo build --release
install -m 755 target/release/drivesync ~/.local/bin/drivesync
drivesync --help
```

## Configurar Google Cloud (una sola vez)

1. Entra en <https://console.cloud.google.com/> y crea (o elige) un proyecto.
2. Activa **Google Drive API** en *APIs y servicios → Biblioteca*.
3. Configura la pantalla de consentimiento OAuth (*APIs y servicios → Pantalla
   de consentimiento OAuth*): tipo **Externo**, añade tu correo como *usuario de
   prueba*. La app queda en modo prueba y solo esos usuarios pueden autorizarla.
4. Crea credenciales en *APIs y servicios → Credenciales → Crear credenciales →
   ID de cliente OAuth*: tipo de aplicación **Aplicación de escritorio**.
5. Copia el **client ID** y el **client secret**.

El alcance por defecto es `https://www.googleapis.com/auth/drive` (acceso
completo a los archivos de la cuenta). Es un alcance restringido: para uso
personal con la app en modo prueba funciona añadiendo tu usuario; publicarla
para terceros exige el proceso de verificación de Google.

## Autenticación

Hay tres formas. Las credenciales se guardan en
`~/.config/drivesync/config.json` con permisos `0600` (o en la ruta que indique
`--config` / `DRIVESYNC_CONFIG`).

### 1. Token de acceso (rápido, caduca en ~1 hora)

```bash
drivesync auth --access-token "$(gcloud auth print-access-token)"
# o
drivesync --token "$TOKEN" status
# o
export GOOGLE_ACCESS_TOKEN="$TOKEN"
```

Cuando el token caduca, la API devuelve 401 y DriveSync pide volver a
autenticarse. Útil para scripts y CI.

### 2. Navegador + refresh token (recomendado para uso continuo)

```bash
drivesync auth --client-id ID.apps.googleusercontent.com --client-secret SECRET
```

Abre la URL que imprime (o intenta abrir el navegador), escucha en
`http://127.0.0.1:8765/callback`, canjea el código y guarda un **refresh token**
con `access_type=offline`. A partir de ahí el token de acceso se renueva solo.

Opciones: `--port 9000` si el puerto está ocupado, `--no-browser` en servidores
sin escritorio. Si Google rechaza la redirección, añade
`http://127.0.0.1:8765/callback` en los *URI de redirección autorizados* del
cliente OAuth.

### 3. Sin servidor local: la URL y el código a mano (`--manual`)

Para máquinas sin navegador, sesiones SSH o cuando el puerto local está
bloqueado. DriveSync imprime la URL de consentimiento, tú autorizas en
cualquier navegador (puede ser otro equipo) y pegas el resultado:

```bash
drivesync auth --manual --client-id ID --client-secret SECRET
```

```
1. Abre esta URL en el navegador:

   https://accounts.google.com/o/oauth2/v2/auth?client_id=...&redirect_uri=...

2. Autoriza la aplicación. DriveSync escucha en http://127.0.0.1:8765/callback
   y te mostrará el código en una página, listo para copiar.
3. Si el navegador está en otra máquina y no llega a la página, copia la URL
   completa de la barra de direcciones (contiene code=...).

Pega la URL, el código o el token:
```

Si el navegador corre en la misma máquina, al autorizar verás una página con el
código en grande, botón **Copiar código** y, plegada, la URL completa por si
prefieres pegarla. El CLI recibe el código automáticamente desde esa página, así
que normalmente no hay que pegar nada; el prompt queda para cuando el navegador
está en otro equipo (o el puerto está ocupado y no se puede servir la página).

Qué acepta el prompt:

- La **URL de redirección completa** (`http://127.0.0.1:8765/callback?state=…&code=…`).
- El **código suelto** (`4/0AX…`).
- Un **token de acceso** ya emitido (`ya29.…`), que se guarda tal cual con el
  aviso de que caduca en ~1 hora y no se renueva.

La respuesta se valida contra el `state` de esa ejecución; si no coincide, se
rechaza el canje. Usa `--redirect-uri` para incrustar otra URI (debe estar
autorizada en el cliente OAuth) y `--state VALOR` para fijar el valor, útil en
scripts.

En el caso del código se obtiene un refresh token igual que en el flujo con
navegador, así que el token se renueva después solo.

### 4. Refresh token ya emitido

```bash
drivesync auth --client-id ID --client-secret SECRET --refresh-token REFRESH
```

Se valida contra Google antes de guardarlo.

### Variables de entorno

| Variable | Uso |
| --- | --- |
| `GOOGLE_ACCESS_TOKEN` | token de acceso (equivale a `--token`) |
| `GOOGLE_CLIENT_ID` | client ID para renovar |
| `GOOGLE_CLIENT_SECRET` | client secret para renovar |
| `GOOGLE_REFRESH_TOKEN` | refresh token |
| `DRIVESYNC_CONFIG` | ruta alternativa de configuración |
| `DRIVESYNC_API_BASE` | URL base de la API (pruebas) |

`drivesync status` muestra la cuenta, el alcance, el modo de autenticación y el
almacenamiento usado. `drivesync logout` borra los tokens y conserva el client
ID/secret.

## Problemas frecuentes

**`Error 401: invalid_client` — "The OAuth client was not found"**

La página de Google aparece, pero no reconoce el cliente. Casi siempre es una
de estas:

1. El `client_id` no está completo o tiene un espacio/retorno de carro al
   pegar. Debe terminar en `.apps.googleusercontent.com` y medir ~72 caracteres.
   Compruébalo sin abrir el navegador:

   ```bash
   drivesync auth --check-client --client-id TU_ID --client-secret TU_SECRET
   ```

   Envía un código inválido al endpoint de tokens: si Google responde
   `invalid_client` el cliente no existe; si responde `invalid_grant` el
   `client_id` es correcto y el problema está en otra parte. No guarda nada.
2. El cliente es de tipo **Android**, **iOS** o **cuenta de servicio**: solo los
   clientes de tipo **Aplicación de escritorio** o **Aplicación web** sirven
   para este flujo. Compruébalo en *Credenciales* del proyecto.
3. El cliente se creó en otro proyecto del que crees, o se borró.

DriveSync avisa si el `client_id` no termina en `.apps.googleusercontent.com`.
Verifica el valor guardado con:

```bash
drivesync status          # no muestra el secreto
python3 -c "import json,os;print(json.load(open(os.path.expanduser('~/.config/drivesync/config.json')))['client_id'])"
```

**`redirect_uri_mismatch`**

La URI de redirección no está autorizada en el cliente. Añade
`http://127.0.0.1:8765/callback` en *URI de redirección autorizados*, o usa el
mismo `--port` / `--redirect-uri` que tengas registrado.

**`access_denied` — "DriveSync no completó el proceso de verificación de Google"**

La app está en estado **Prueba** y la cuenta con la que autorizas no está en la
lista de *usuarios de prueba*. Es el caso normal mientras la app no pasa la
verificación de Google (el scope `drive` es restringido y verificarlo es un
proceso aparte). Solución, en el proyecto donde creaste el `client_id`:

1. <https://console.cloud.google.com/> → selecciona el proyecto.
2. *APIs y servicios → Pantalla de consentimiento OAuth* (en la consola nueva,
   *Google Auth Platform → Audience*).
3. En **Usuarios de prueba** → *Add users* → escribe el correo exacto con el que
   autorizas (por ejemplo `danimanx@gmail.com`) → *Save*.
4. Espera un par de minutos y repite `drivesync auth`.

Límites del modo prueba: hasta 100 usuarios de prueba y los **refresh tokens
caducan a los 7 días**, así que toca reautorizar semanalmente. Para evitarlo hay
que publicar la app (*Publish app*), lo que con un scope restringido como
`drive` implica el proceso de verificación de Google; la alternativa práctica
para uso propio es un cliente con el scope `drive.file`, o seguir usando
`drivesync auth --access-token` con un token de corta duración en scripts.

Comprueba qué cuenta tienes configurada y con qué cliente:

```bash
drivesync status
```

**`invalid_grant`**

El código ya se canjeó, caducó (10 minutos) o el `refresh_token` se revocó.
Vuelve a ejecutar `drivesync auth`.

## Comandos

```
drivesync list   [--folder CARPETA] [--max N] [--no-sizes]
drivesync upload RUTA... [--to CARPETA] [--name NOMBRE] [--contents] [--exclude PATRÓN]
drivesync download REMOTO [--to RUTA] [--overwrite]
drivesync mkdir  NOMBRE [--folder CARPETA] [--literal]
drivesync sync   DIRECTORIO --remote CARPETA [opciones]
drivesync status
drivesync logout
```

`CARPETA`/`REMOTO` aceptan un **ID** de Drive, una **ruta** desde *Mi unidad*
(`Backup/Fotos/2026`) o un **nombre** (se busca y se avisa si hay duplicados).

En las rutas vale tanto `/` como `\`, así que `Backup\Fotos\2026` y
`Backup/Fotos/2026` son lo mismo. `mkdir` además crea las carpetas
intermedias que falten:

```bash
drivesync mkdir "Recomp/NSW 2"            # crea Recomp si no existe y NSW 2 dentro
drivesync mkdir --folder Recomp "NSW 2"   # equivalente, con el nombre suelto
drivesync mkdir "Recomp\GBA 2"            # estilo Windows, también válido
drivesync mkdir --literal "c\raro"        # nombre literal, sin separar
```

Solo `mkdir` admite `--literal`; el resto de comandos interpretan `\` como
separador, así que una carpeta cuyo nombre contenga una barra invertida debe
referenciarse por su ID.

### Ejemplos

```bash
# Listar una carpeta por ruta (las carpetas muestran su tamaño recursivo)
drivesync list --folder Backup/2026

# Igual pero sin calcular tamaños (más rápido en árboles enormes)
drivesync list --folder Backup --no-sizes

# Subir un archivo (si ya existe, se actualiza)
drivesync upload ./informe.pdf --to Backup

# Subir una carpeta completa de forma recursiva: crea Backup/proyecto/...
drivesync upload ./proyecto --to Backup

# Subir solo el contenido, sin crear la carpeta contenedora
drivesync upload ./proyecto --to Backup/Codigo --contents

# Varias rutas de una vez, excluyendo temporales y cachés
drivesync upload ./proyecto ./informe.pdf --to Backup \
  --exclude '*.tmp' --exclude 'cache/**' --exclude '.git' 

# Descargar una carpeta entera a ./restaurado
drivesync download Backup/Codigo --to ./restaurado --overwrite

# Sincronizar en ambos sentidos, mostrando el plan
drivesync sync ./documentos --remote Backup/Documentos --dry-run

# Sincronizar de verdad, excluyendo temporales y cachés
drivesync sync ./documentos --remote Backup/Documentos \
  --exclude '*.tmp' --exclude 'cache/**'

# Solo subir cambios y propagar borrados
drivesync sync ./documentos --remote Backup/Documentos --direction push --delete

# Solo bajar cambios (el remoto manda)
drivesync sync ./documentos --remote Backup/Documentos --direction pull
```

## Subida recursiva

`upload` acepta uno o varios archivos o carpetas y recorre las carpetas en
profundidad, creando la estructura de subcarpetas que falte en Drive.

```bash
drivesync upload ./proyecto --to Backup            # Backup/proyecto/{...}
drivesync upload ./proyecto --to Backup --contents # Backup/{...} sin la carpeta
drivesync upload a.txt b/ c/ --to Backup           # varias rutas a la vez
drivesync upload ./proyecto --to Backup --dry-run  # solo describe
```

- **Idempotente**: compara el MD5 de cada archivo con el que reporta Drive y
  omite los que ya están iguales. Volver a subir el mismo árbol no genera
  escrituras.
- **Carpetas vacías**: también se crean.
- **Exclusiones**: `--exclude` con comodines `*` y `?`, repetible. Aplica a
  rutas, nombres base y subárboles; `cache/**` excluye además la propia carpeta.
- **Enlaces simbólicos** y archivos especiales se omiten con aviso.
- **`--name`** renombra una única ruta; **`--replace ID`** sobrescribe un
  archivo concreto de Drive.
- **Resiliencia**: si un elemento falla (por ejemplo ya existe un archivo donde
  debería ir una carpeta), se informa, se continúa con el resto y el comando
  termina con código distinto de cero.
- **Resumen** al final: subidos, actualizados, sin cambios, carpetas creadas y
  omitidos.

## Cómo funciona la sincronización

El estado se guarda en `DIRECTORIO/.drivesync/state.json`: por cada ruta
relativa anota el MD5 local, el ID remoto, el MD5 remoto y la fecha. En cada
pasada se calcula el MD5 de los archivos locales, se listan recursivamente los
remotos y se comparan tres versiones (base, local, remoto).

Con `--direction both` (por defecto):

- Solo local y sin base → **subir**.
- Solo remoto y sin base → **bajar**.
- Ambos lados iguales → nada.
- Cambió el local → **actualizar remoto**.
- Cambió el remoto → **actualizar local**.
- Ambos cambiaron → **conflicto**.
- Local borrado (y remoto intacto) → con `--delete`, mover el remoto a la
  papelera; sin `--delete`, solo avisar.
- Remoto borrado (y local intacto) → con `--delete`, borrar el local; sin
  `--delete`, solo avisar.
- Lado borrado pero el otro modificado → se restaura la versión modificada.

Los borrados **nunca** se propagan sin `--delete`. Los archivos remotos se
mueven a la papelera de Drive (recuperables).

### Conflictos

`--conflict` elige la política:

- `keep-both` (por defecto): descarga la versión remota como
  `nombre.conflicto-remoto-AAAAMMDD-HHMMSS.ext`, sube la local al archivo
  remoto original y sube la copia remota como archivo nuevo. Ambos lados quedan
  con las dos versiones.
- `keep-local`: la versión local sobrescribe la remota.
- `keep-remote`: la versión remota sobrescribe la local.

### Exclusiones

`--exclude` acepta comodines `*` y `?` y se aplica a la ruta relativa, al nombre
base y a todo el subárbol (`cache/**`), tanto en local como en remoto (así
`--delete` no borra en Drive algo que estás excluyendo). `.drivesync` se excluye
siempre. Los enlaces simbólicos se omiten con un aviso.

## Limitaciones conocidas

- Los **documentos nativos de Google** (Docs, Sheets, Slides) no tienen MD5 ni
  contenido binario descargable: se omiten con un aviso. Para sincronizarlos
  habría que exportarlos a un formato concreto.
- Los **accesos directos** de Drive y los tipos no binarios se omiten.
- No se borran carpetas vacías: se propagan archivos, no la estructura vacía.
- Se calcula el MD5 de todos los archivos locales en cada pasada (correcto,
  pero más lento en árboles muy grandes).
- La sincronización es secuencial (una operación a la vez) y no reanuda una
  subida entre ejecuciones distintas: los bloques se reintentan dentro de la
  misma ejecución.
- Nombres remotos con `/` se sanean a `_`; nombres duplicados en la misma
  carpeta remota se resuelven usando el primero y avisando.
- No ejecutes dos `drivesync sync` sobre el mismo directorio a la vez.

## Seguridad

- El archivo de configuración se crea con permisos `0600` y el directorio con
  `0700` en sistemas Unix. Aun así, el token se guarda en texto plano: protege
  tu directorio personal.
- El alcance `drive` da acceso completo a Drive. Si solo vas a manejar archivos
  creados por la app, puedes usar `--scope https://www.googleapis.com/auth/drive.file`,
  pero entonces DriveSync no verá archivos preexistentes.
- Para CI, prefiere `GOOGLE_ACCESS_TOKEN` de corta duración antes que guardar un
  refresh token en disco.
- `logout` elimina los tokens; revoca el acceso desde
  <https://myaccount.google.com/permissions> si quieres invalidar el refresh
  token por completo.

## Desarrollo y pruebas

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

El flujo de GitHub Actions (`.github/workflows/ci.yml`) ejecuta formato, clippy,
pruebas y compilación release para `x86_64` y `aarch64` en cada push a `main`,
en cada pull request y en cada tag `v*`.

Los binarios compilados se publican de dos formas:

- **Artefactos del run** (`drivesync-<target>`): disponibles para inspección,
  requieren sesión en GitHub y caducan a los 90 días.
- **Release de GitHub**: al empujar un tag `v*` se crea la release con el
  binario crudo, su `.tar.gz` y `SHA256SUMS-<target>.txt` de cada arquitectura.
  Es la vía normal para descargar e instalar.

Para publicar una release de un tag que ya existe sin volver a etiquetar, lanza
el workflow a mano indicando el tag:

```bash
gh workflow run ci.yml -f release_tag=v0.0.1
```

La suite incluye:

- Pruebas unitarias del plan de sincronización (subidas, bajadas, conflictos,
  borrados, `push`/`pull`, exclusiones, utilidades).
- Pruebas de extremo a extremo en `tests/mock_drive.rs` que ejecutan el binario
  real contra un servidor HTTP local que implementa el subconjunto usado de la
  API de Drive: subida multipart y reanudable, descarga, papelera, refresh
  token, conflictos y `--dry-run`.

## Estructura

```
src/main.rs    CLI (clap) y comandos
src/auth.rs    tokens: refresh, OAuth de bucle local, token estático
src/api.rs     cliente HTTP de Drive v3 (listar, subir, bajar, papelera)
src/sync.rs    escaneo, estado de tres vías, plan y ejecución
src/config.rs  configuración persistente
src/util.rs    MD5, comodines, MIME, fechas y tamaños
tests/         Drive simulado de extremo a extremo
```
