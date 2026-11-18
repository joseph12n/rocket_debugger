# Auditoria de arquitectura

Fecha: 2026-11-18 · Base: punta de la cadena de PRs (#29 -> #30 -> #31 -> este) ·
Suite local: 352 tests verdes (3 `#[ignore]`), `fmt --check` y `clippy -D warnings` limpios.

Metodo: conteo de lineas y comentarios por modulo, inventario de dependencias, busqueda de
duplicados por nombre de funcion y de superficie de API. Mediciones con el binario de depuracion
sobre el workspace real (`C:\rocketbot_bots`: 16 robots en 4 dbs).

## 1. Foto

41 ficheros `.rs` en `crates/`: **22.291 lineas, 3.829 de comentarios (17,2%)**. Los diez modulos
mas cargados de comentario:

| modulo | lineas | comentarios | % |
|---|---:|---:|---:|
| `core/analysis.rs` | 1387 | 402 | 29 |
| `core/logs.rs` | 1023 | 256 | 25 |
| `core/redact.rs` | 1050 | 242 | 23 |
| `core/exec.rs` | 1343 | 209 | 16 |
| `core/graph.rs` | 930 | 202 | 22 |
| `cli/watch.rs` | 943 | 200 | 21 |
| `core/attribution.rs` | 598 | 192 | 32 |
| `core/registry.rs` | 845 | 186 | 22 |
| `mcp/http.rs` | 999 | 166 | 17 |
| `core/render.rs` | 883 | 144 | 16 |

(`mcp/lib.rs` tiene el porcentaje mas alto, 40%, pero son 276 lineas de contrato de `Settings`.)

Dependencias (`──` = "usa"). Sin ciclos; `robot.db` siempre en solo lectura y la unica escritura
persistente es `problems.db` (registro):

```
db ── tree, versions
logs, signature ── (logs: tipos en signature)     attribution ── (puro)
graph ── tree            rules ── tree             paths ── tree, rules (especificacion)
analysis ── db, tree, graph, rules, paths, logs, signature, attribution
pack ── db, tree, versions, redact, analysis
render ── redact, rules, diff, analysis, pack
review ── analysis, db
exec ── db, logs, signature, attribution, analysis (solo snapshot_views)
registry ── (independiente)
mcp ── core    ·    cli ── core
```

## 2. Fortalezas a conservar

1. **El motor es hermetico.** Sin probe de rutas (`NoPaths`) y sin logs (`None`), el motor no toca
   disco: 352 tests pasan sin depender de `C:\Rocketbot`. Los cambios de comportamiento por
   entorno viven en el borde (CLI, MCP), no dentro.
2. **Una politica, un punto.** Enmascarado en `render::field` (+ `redact::mask_emails`), etiquetas
   de db en `db::label`/`db::labels_for`, atribucion en `attribution::attribute`, firmas en
   `signature::normalize`. Un bug de politica se arregla una vez.
3. **Determinismo con golden.** El render es estable y tiene golden; la bandeja y el CLI comparten
   la misma agregacion (`review`), y hay test de paridad entre el analisis compartido y el
   individual.
4. **El bloqueo real convertido en garantia.** El falso exito del runner CLI (Studio abierto, sin
   licencia de produccion) queda como `NoEvents` y nunca como exito: la trampa del entorno es hoy
   un test.
5. **El comentario documenta el por que.** Convencion del README que ya se cumple; la sanitizacion
   (seccion 4) la respeta, no la invierte.

## 3. Hallazgos

Ordenados por prioridad. Cada uno con evidencia (archivo:linea de esta punta), riesgo y accion.

### H1 · `analysis.rs` concentra cuatro responsabilidades — ALTA

1387 lineas, el modulo mas grande. Mezcla: carga/dedupe de hechos (`load_workspace_of`,
`load_db_facts`, `Target`), orquestacion por robot (`Workspace::analyse`, `analyse_loaded`),
informe de errores (`errors_for`, `snapshot_views`, `bucket_of`) y pistas de linea para rutas
(`variable_lines`).

**Accion:** partir en `facts.rs` (carga + dedupe) y `errors.rs` (informe de errores), dejando
`analysis.rs` como orquestador. Los tests de paridad actuales hacen el movimiento barato: si el
resultado cambia, fallan.

### H2 · El tokenizador de rutas absolutas esta triplicado — ALTA

El mismo trío de guardas (`continues_scheme`, `inside_url`, `path_snippet`) vive en:

- `rules.rs:434/442/458` — para `RUTA_ABSOLUTA`: solo rutas de unidad, corta el snippet en comas.
- `paths.rs:466/474/489` — para `RUTA_MIGRADA`: tambien POSIX, snippet hasta 260 y sin comas.
- `redact.rs:218` — copia de `continues_scheme` para el enmascarado.

Una misma ruta puede ser detectada por una regla y no por la otra (p. ej. una ruta con coma en el
nombre), y tres sitios que deciden "esto es una ruta" acabaran discrepando.

**Accion:** un extractor unico en `paths.rs` que devuelva rutas con posicion y tipo; `rules`,
`paths` y `redact` lo consumen, cada uno con su semantica. Test: fixture donde las tres reglas
ven el mismo conjunto donde aplican.

### H3 · Lectura del formato `bots.data` duplicada — ALTA (trivial)

- `fn commands_of` identica en `analysis.rs:987` y `pack.rs:214`.
- Dos `fn vars_of` con el mismo nombre y semantica distinta: `pack.rs:223` redacta,
  `paths.rs:63` devuelve crudo.

**Accion:** subir `commands_of` a `db.rs` junto a `project_root` (es lectura del formato, no
politica de ningun consumidor); que `pack::vars_of` consuma `paths::vars_of` y solo anada la
redaccion.

### H4 · Siete variantes de `analyse_*` + `Workspace::analyse` — MEDIA

`analyse`, `analyse_with_logs`, `analyse_with_logs_at`, `analyse_with_logs_at_paths`,
`analyse_with_workspace`, `analyse_with_workspace_at`, `analyse_with_workspace_at_and_paths`.
Despues de `Workspace`, la mayoria son envoltorios de 1-3 lineas. Call sites reales fuera de
tests: `pack.rs:158`, `review.rs:174/183` (Workspace), `mcp/tools.rs:737`, `cli/main.rs:222`,
`cli/watch.rs:232/234`, `cli/main.rs:369` (`load_workspace`).

**Accion:** dejar `Workspace::load/analyse` y una sola conveniencia de db unica con
`(logs, paths, snapshot)`; migrar 4 call sites y borrar el resto. Menos superficie publica =
menos formas de desincronizarse.

### H5 · `mcp/http.rs` mezcla transporte y aplicacion — MEDIA

999 lineas: parse HTTP, limits, ruteo, `review_json`, `pack_for` y assets embebidos en el mismo
fichero.

**Accion:** extraer `api.rs` (lo que la UI consume) y dejar `http.rs` como transporte. No
urgente: urge solo si la bandeja crece.

### H6 · CI no pasa los tests en Windows — MEDIA

`ci.yml` corre fmt/clippy/test en ubuntu; el job de Windows solo hace `cargo build --release`.
El codigo tiene rutas `cfg(windows)` (`parse_tasklist`, `GetLocalTime`, `kill_tree` con
`taskkill`) cubiertas por tests que nunca corren en CI; hoy las verifica solo este equipo.

**Accion:** anadir `cargo test --all` al job de Windows. (En esta punta va un fix previo:
`#[link(name = "kernel32")]` explicito en `exec.rs`, porque el CI de Windows enlaza con MSVC y no
conviene fiarse de que `kernel32` venga por defecto.)

### H7 · Fixtures de test duplicados — BAJA/MEDIA

`make_db` en 9 ficheros de test con firmas ligeramente distintas; `TempDir` casero en 2
(`cli/tests/review.rs:32`, `mcp/tests/common/mod.rs:24`) mientras core usa `tempfile`.

**Accion:** crate dev-only `rocketdebug-testkit` (miembro del workspace, `publish = false`) con
`make_db`, `TempDir` y `demo_project`. No toca produccion.

### H8 · Modulos grandes pero cohesivos — BAJA (vigilar)

`watch.rs` (943), `exec.rs` (1343, nuevo), `registry.rs` (845): partir solo si crecen. El split
natural de `exec` si llega el modo API es `runner.rs` / `broker.rs`.

## 4. Sanitizacion de comentarios

Criterio (respeta la convencion del README: el comentario documenta el POR QUE, no el QUE):

**Queda**
- el por que no obvio, invariantes y trampas de plataforma (`Path` no trata `\` como separador en
  Linux; candados envenenados),
- datos medidos y referencias a issues,
- un ejemplo donde el contrato no es obvio (el doctest de `attribution::attribute`).

**Se quita**
- prosa que repite el codigo linea a linea,
- la historia del bug cuando el test ya la fija (el test es la historia),
- cabeceras de seccion que duplican el nombre del modulo,
- anecdotas de 15+ lineas: se resumen a 2-4 y el detalle vive en docs/ o en el issue.

**Reglas de forma**
- doc comment: tope orientativo 10 lineas; espanol sin acentos; identificadores en ingles,
- cada nota repetida se documenta UNA vez y los demas sitios apuntan ahi,
- diff solo de comentarios: `cargo test --all` identico, fmt y clippy verdes. Lo que ya cumple el
  criterio no se toca (restriccion = parte del estandar).

**Inventario** (orden propuesto, densidad primero): `analysis.rs` (29%) -> `logs.rs` (25%) ->
`redact.rs` (23%) -> `watch.rs` (21%) -> `graph.rs` (22%) -> `registry.rs` (22%) -> `attribution.rs`
(32%, pero casi todo es por que legitimo) -> resto por debajo del 20%.

**Piloto (esta ronda):** `logs.rs` (modulo preexistente mas comentado y no tocado por los PRs
abiertos) y la cabecera de `exec.rs` (la mas verbosa del codigo nuevo, mia). Medido: `logs.rs`
256 -> 199 comentarios, `exec.rs` 209 -> 188. Comportamiento intacto: 352 tests verdes (0 fallos),
fmt y clippy limpios.

## 5. Roadmap sugerido

1. Merge de la cadena (#29 -> #31), del tope hacia la base.
2. Sanitizacion por modulo en el orden del inventario (logs y exec ya en el piloto).
3. H3 (trivial) y H2 (extractor unico) con sus tests.
4. H1 partir `analysis.rs`; luego H7 testkit, H6 job de tests en Windows, H4 variantes.
5. H5 solo si la bandeja crece.

## 6. Limitaciones aceptadas (no son deuda)

- `protocolVersion` fijo sin negociacion; clave de dedupe `(nombre, hash)`; `flow_pack` lee logs
  si el directorio existe; `Pack.flow[].params` crudo (el enmascarado vive en `render::field`, con
  test que lo fija).
- Aceptacion real de #25 y ciclo #27: a la espera de licencia de produccion (la instalada es
  Studio Develop). Documentado en `docs/ARCHITECTURE.md`; los tests reales estan listos con
  `-- --ignored`.
