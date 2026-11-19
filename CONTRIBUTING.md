# Contribuir

## Antes de empezar

Rust stable. En Windows no hay MSVC: se compila con el toolchain GNU y un GCC portatil
(WinLibs) para las dependencias nativas.

```powershell
rustup toolchain install stable-x86_64-pc-windows-gnu
cargo +stable-x86_64-pc-windows-gnu build
```

- `gh` en el PATH: `pr.ps1` lo usa para el alta del PR y para esperar las comprobaciones.
- Build, uso y estado del proyecto: [README](README.md). Auditoria de arquitectura y plan de
  sanitizacion: [docs/AUDIT.md](docs/AUDIT.md).
- Los tests no dependen de `C:\Rocketbot`: los fixtures son sinteticos. Los de integracion
  contra datos reales van con `#[ignore]` y se corren a mano cuando el entorno lo permite.

## Ramas

`<tipo>/<slug-en-ingles-kebab-case>`: el tipo de la tabla, barra, y el slug en ingles, en
minusculas y con guiones. Sin espacios, sin guiones bajos y sin espanol en el slug.

| tipo | cuando | ejemplo |
|---|---|---|
| `feat` | funcionalidad nueva | `feat/exec-broker` |
| `fix` | error corregido | `fix/redact-apostrophe` |
| `refactor` | interno, sin cambio de comportamiento | `refactor/path-tokenizer` |
| `perf` | rendimiento | `perf/render-token-cache` |
| `docs` | documentacion | `docs/architecture-audit` |
| `test` | tests | `test/redaction-golden` |
| `build` | build, toolchain, dependencias | `build/gnu-toolchain` |
| `ci` | workflows y automatizacion | `ci/windows-release` |
| `chore` | mantenimiento sin codigo de producto | `chore/bump-deps` |

`pr.ps1` valida la rama y se para si no encaja. Mal: `exec-broker` (sin tipo),
`sanitizacion-comentarios` (slug en espanol), `mi_rama` (sin tipo y con guion bajo).

## Commits

Conventional commits, en espanol sin acentos y en imperativo. Un commit, un cambio con sentido.

```text
feat(exec): broker de ejecucion con cola
fix(redact): el apostrofo forma parte del nombre
```

El asunto dice que cambia; nada de `wip` ni de "varios arreglos". Sin atribuciones a
herramientas de IA, ni en el mensaje ni en el codigo.

## Tamano de PR

Tope: **400 lineas de codigo cambiadas**. No cuentan `docs/`, `fixtures/`, los `*.md`,
`Cargo.lock` ni el **movimiento puro**: una linea que se borra y reaparece identica en otro fichero
es un traslado, no una revision. Se mide con:

```bash
git diff --numstat origin/main...HEAD
```

`pr.ps1` calcula el numero que vale (descuenta las lineas movidas) y no sube si se pasa del tope.
Si el cambio no cabe, se parte en PRs apilados por tema, no en uno gigante.

## Firma de commits

Todos los commits van firmados: el `commit.gpgsign` global esta activo y la firma es SSH
(`gpg.format=ssh`), asi que un commit sin firma es un error de configuracion, no una eleccion.

```powershell
ssh-keygen -t ed25519 -f $HOME\.ssh\id_github_signing -N '""'
git config --global gpg.format ssh
git config --global user.signingkey $HOME\.ssh\id_github_signing.pub
git config --global commit.gpgsign true
```

Para que GitHub lo muestre como **Verified**, la clave publica tiene que estar registrada como
clave de firma: Settings -> SSH and GPG keys -> New SSH key -> tipo *Signing Key*, o
`gh ssh-key add <pub> --type signing` (pide el scope `admin:ssh_signing_key`). Comprobar:

```powershell
git log -1 --format='%G? %GS'
```

## Hooks locales

`lefthook` corre en cada commit: `gitleaks protect --staged` (secretos), bloqueo de `.env` en
staging y `cog verify` sobre el mensaje (Conventional Commits). Se instala una vez por clon:

```powershell
lefthook install
```

`--no-verify` no se usa. Si un hook falla, se arregla la causa.

## Versiones

Al etiquetar `v*` y subir el tag, `release.yml` compila, pasa la suite y publica un Release de
GitHub con los dos ejecutables de Windows. El tag va firmado como los commits. Ademas, cada
build de `main` deja un artifact descargable en la pestana Actions.

```powershell
git tag -a v0.1.0 -m "v0.1.0"
git push origin v0.1.0
```

## Ciclo de trabajo

Orden: rama -> cambios -> gate -> `pr.ps1` -> CI y Sonar -> merge (tope->base si hay cadena).

1. Rama desde `main`: `git switch -c feat/exec-broker`.
2. Cambios con su test. Arbol limpio antes de subir.
3. Gate local, que replica CI:

```powershell
pwsh scripts/gate.ps1
pwsh scripts/gate.ps1 -Quick
pwsh scripts/gate.ps1 -Release
```

4. Push y PR en una orden:

```powershell
pwsh scripts/pr.ps1 -BodyFile C:\tmp\pr.md
pwsh scripts/pr.ps1 -Merge
```

5. El cuerpo del PR dice que cambia y como se verifico: gate local en verde, CI y SonarCloud. Si
   cierra un issue, se referencia (`cierra #26`).
6. Cadena apilada: GitHub no cierra el issue solo, se cierra a mano al aterrizar.
7. El merge se hace con `pr.ps1 -Merge` (fast-forward y push), no con el boton de GitHub:
   rebase/squash reescriben los commits y **pierden la firma SSH**; por fast-forward llegan a la
   base tal como se firmaron.

## Comentarios

El codigo no lleva: ni doc de modulo, ni doc de API, ni inline, ni en tests, scripts, workflows o
configuracion. El codigo, el nombre, el tipo y el test son la explicacion; lo que antes contaba un
comentario vive en `docs/ARCHITECTURE.md` o en el historial de commits. Los ejemplos de doctest se
movieron a tests normales y la ayuda del CLI se declara con atributos de clap (`help`, `about`).
`docs/` y `fixtures/` son documentacion y datos: no son comentarios y no se tocan. `Cargo.lock` es
autogenerado por Cargo y conserva su cabecera (cargo la repone en cada escritura).

## Que no se hace

- Reescribir fuentes con `Set-Content` de PowerShell: mete BOM y rompe el UTF-8 (acentos y
  simbolos del render). Se edita con un editor en UTF-8 sin BOM.
- Escribir en `robot.db`: va siempre en solo lectura. El unico fichero que se escribe es el
  registro `problems.db`.
- Colar secretos: credenciales, tokens o rutas absolutas de cliente, ni en codigo, ni en
  fixtures, ni en un PR. La redaccion del motor no es excusa.
- Atribuciones a herramientas de IA en commits, PRs o codigo.
- Commitear en `main` o saltarse el gate "porque es un cambio pequeno".
