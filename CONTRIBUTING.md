# Contribuir

## Antes de empezar

Rust stable. En Windows no hay MSVC: se compila con el toolchain GNU y un GCC portatil
(WinLibs) para las dependencias nativas.

```powershell
rustup toolchain install stable-x86_64-pc-windows-gnu
# el bin de mingw64 (WinLibs) tiene que estar en el PATH
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

Tope: **400 lineas de codigo cambiadas**, sumando anadidas y borradas. No cuentan `docs/`,
`fixtures/`, los `*.md` ni `Cargo.lock`. Se mide con:

```bash
git diff --numstat origin/main...HEAD
```

`pr.ps1` hace la misma cuenta (tope 400) y no sube si se pasa. Si el cambio no cabe, se parte
en PRs apilados por tema, no en uno gigante.

## Ciclo de trabajo

Orden: rama -> cambios -> gate -> `pr.ps1` -> CI y Sonar -> merge (tope->base si hay cadena).

1. Rama desde `main`: `git switch -c feat/exec-broker`.
2. Cambios con su test. Arbol limpio antes de subir.
3. Gate local, que replica CI:

```powershell
pwsh scripts/gate.ps1           # fmt + clippy -D warnings + tests
pwsh scripts/gate.ps1 -Quick    # omite tests, al iterar
pwsh scripts/gate.ps1 -Release  # anade el build release del job de Windows
```

4. Push y PR en una orden:

```powershell
pwsh scripts/pr.ps1 -BodyFile C:\tmp\pr.md
pwsh scripts/pr.ps1 -Merge     # y lo aterriza con merge rebase
```

5. El cuerpo del PR dice que cambia y como se verifico: gate local en verde, CI y SonarCloud. Si
   cierra un issue, se referencia (`cierra #26`).
6. Cadena apilada: GitHub no cierra el issue solo, se cierra a mano al aterrizar.

## Que no se hace

- Reescribir fuentes con `Set-Content` de PowerShell: mete BOM y rompe el UTF-8 (acentos y
  simbolos del render). Se edita con un editor en UTF-8 sin BOM.
- Escribir en `robot.db`: va siempre en solo lectura. El unico fichero que se escribe es el
  registro `problems.db`.
- Colar secretos: credenciales, tokens o rutas absolutas de cliente, ni en codigo, ni en
  fixtures, ni en un PR. La redaccion del motor no es excusa.
- Atribuciones a herramientas de IA en commits, PRs o codigo.
- Commitear en `main` o saltarse el gate "porque es un cambio pequeno".
