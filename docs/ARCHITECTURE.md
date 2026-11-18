# Arquitectura

## Que es y que no es

Es el **motor de deteccion** y el **suministro de contexto** para depurar flujos de Rocketbot.
No es un IDE ni un editor visual: nunca escribe `robot.db`. Todo cambio en un robot pasa por el
agente; esta herramienta detecta, propone y muestra el diff.

## Capas

```
robot.db (N dbs) ──┐
logs/*.log ────────┤→ motor ──┬→ render markdown  ← lo que lee el agente
catalogo comandos ─┘          ├→ daemon local (HTTP/WS)  ← lo que pinta la UI
                              └→ fachada MCP  ← intenciones, no datos
```

| Capa | Crate | Responsabilidad |
|---|---|---|
| Motor | `rocketdebug-core` | parseo, arbol, indice de versiones, grafo, reglas, atribucion, render |
| CLI | `rocketdebug-cli` | inspeccion manual y debug del motor |
| MCP | `rocketdebug-mcp` | fachada para el agente por stdio y HTTP (10 tools) |
| Registro | `registry` (core) | memoria de firmas y fixes; **el unico sitio donde se escribe** |
| Vigilante | `rocketdebug-cli watch` | barrido periodico que alimenta el registro sin intervencion |
| UI | `rocketdebug-mcp --http` | bandeja de entrada en el navegador (`/`, `/api/review`, `/api/pack`); Tauri solo la envolveria |

## Decisiones y su motivo

### `serde_json::Value` en vez de structs

El formato de `bots.data` **no esta documentado** y puede cambiar entre versiones de Rocketbot.
Un campo nuevo se ignora, uno que desaparece no rompe nada. El coste es perder seguridad de tipos,
que se compensa con tests sobre fixtures y el test de integracion contra datos reales.

### `id` como direccion, `line` como etiqueta

El `id` de cada nodo coincide con el `cmd_id` que el runner escribe en el log: es la unica union
fiable entre ejecucion y codigo.

`line` **no es unico**. Medido: 34 lineas duplicadas en 13 de 28 robots. En `ML_06_Legacy` el `try`
y el `catch` comparten `2.1`, `2.2`, `2.3`. Cualquier herramienta que direccione por linea — incluido
un `edit` por linea — puede tocar el nodo equivocado. El motor lo detecta y el render lo avisa.

### La atribucion es temporal

Rocketbot no escribe en el log que version se ejecuto. Pero `bots` es un historial: una fila por
guardado, con `created_at`. El intervalo de vida de cada version es reconstruible:

```
snapshot_vivo(t) = argmax { created_at <= t }
```

Caso real: `ML_06_Legacy` fallaba con `NameError: contador` entre 12:49:22 y 12:50:28, cuando estaba
vivo v25/v26. El robot actual es v29 y el error ya no existe. Comparar el nodo contra el snapshot
actual **no** lo detecta: el fix fue anadir un nodo *antes*, sin tocar el nodo que fallaba.

### `preserve_order` en serde_json

Los parametros conservan el orden en que los escribio Studio. Con orden alfabetico, un reordenamiento
de claves al re-guardar aparece como ruido en el diff entre versiones.

### Deduplicacion por hash de contenido

El mismo robot aparece copiado en varias dbs de entrega (`robot_run.db` y
`entrega_corregida/robot_run.db`). Sin dedupe, el workspace de 28 robots se cuenta como 50 y las
metricas mienten.

### Dos cubos de hallazgos

Un defecto concreto (`LINEA_COLISION`, `RUTA_ABSOLUTA`, `RUTA_MIGRADA`) se puede atender hoy; una
carencia del robot entero (`SIN_TRY`, `IF_SIN_ELSE`, `ANIDAMIENTO`, `VAR_NO_USADA`) es cierta pero no
urgente. Mezcladas competian por el mismo tope y la segunda diluia a la primera: medido, las genericas
eran el 36% de lo mostrado. Ahora el render las separa (`## Hallazgos` / `## Higiene`) y cada cubo
tiene su tope. En el barrido real: 70 accionables, 66 de higiene, 0 ocultos.

Regla de clasificacion: **accionable = senala un defecto concreto y su sitio**. El sitio es una linea
cuando el defecto vive en un nodo (`LINEA_COLISION`, `RUTA_ABSOLUTA`); `RUTA_MIGRADA` puede apuntar a
una variable y usar como linea la llamada al subrobot que la consume. Se declara por regla en
`rules/builtin.json`, no se infiere de `Finding::line` (`IF_SIN_ELSE` trae linea y es higiene).

### El motor no lee logs por su cuenta

`build_from_conn` es puro: no toca el sistema de ficheros y deja la seccion de errores vacia. El CLI
usa `build_from_conn_with_logs` y decide el directorio. Sin esa separacion, un test cuyo robot se
llamara igual que uno real cambiaria de resultado segun la maquina que lo ejecute, y el golden del
render dejaria de ser un golden.

### El broker no se fia del `exit_code`

`rocketbot.exe -start=... -db=...` con Studio abierto **delega en la instancia que ya corre** y
puede devolver `exit_code 0` sin ejecutar el robot: es el hallazgo operativo de `docs/CICLO.md`
(`eventos_del_robot: 0`). Por eso la fachada no expone "lanza y devuelve el exit code": expone
`run_robot`, que pasa por un broker.

- **Una ejecucion a la vez** en el workspace: cola FIFO con mutex y condvar. `run_status` publica
  quien esta corriendo, quien espera turno y como termino la ultima ejecucion.
- **Deteccion de Studio abierto antes de lanzar** (procesos `rocketbot.exe` del sistema). No se
  lanza nada y el mensaje da las dos alternativas: cerrar Studio o usar el modo API del servidor
  de Rocketbot.
- **Verificacion de eventos:** despues de lanzar, el broker exige al menos una entrada de log con
  `request: <robot>`. Sin eventos el resultado es fallo aunque el `exit_code` sea 0. El log de
  fichero es la prueba; el codigo de salida no lo es.
- **Atribucion temporal:** los errores de esa ejecucion se cruzan con el historial de `bots` y se
  atribuyen al snapshot vivo de su instante, no al ultimo guardado.

El runner recibe un directorio de logs nuevo por ejecucion (`--log=<dir>`). Si la version instalada
ignora la opcion, el broker prueba el `Log path:` que el CLI anuncia en stdout y, como ultimo
recurso, el directorio de logs configurado; los tres caminos se filtran por la hora de arranque,
asi que un log viejo no cuenta como ejecucion de ahora. Ese directorio es el mismo que lee el pack,
de modo que despues de ejecutar, `flow_pack` ya ve los errores nuevos.

`run_robot` es sincrono y puede tardar: el timeout (5-900 s, 180 por defecto) es de **ejecucion**,
no de espera en cola. `run_status` se puede pedir en paralelo porque el servidor HTTP atiende una
conexion por hilo. La ejecucion real contra BBRE2 vive en un test `#[ignore]`: requiere licencia de
produccion, Studio cerrado, `ROCKETBOT_EXE` y `C:\rocketbot_bots\BBRE2\robot_run.db`.

> **Estado del entorno (2026-11-18):** la licencia instalada es **Studio Develop**. Con Studio
> cerrado, el CLI sigue respondiendo `Does not have a production license`, sale con `exit_code 0` y
> no deja ni un evento; el broker lo reporta como `NoEvents`, que es exactamente el falso exito que
> existe para evitar. La prueba real de #25 queda pendiente de activar una licencia de produccion
> (ya estaba registrada como pendiente en `C:\rocketbot_bots\MEMORIA.md`). El `/execute` del
> servidor de Studio ejecuta comandos sueltos y la orquestacion del flujo vive en la UI, asi que
> "modo API" no es hoy un runner completo: se documenta como alternativa, no se implementa aqui.

## Restricciones del entorno real

- `max_executions: 1` en la licencia y un unico ChromeDriver. Por eso `run_robot` pasa por el
  **broker de ejecucion** (cola + mutex) y se niega con Studio abierto: si el agente ejecuta mientras
  Studio esta abierto, el resultado es `SessionNotCreatedException: Chrome failed to start: crashed`
  o, peor, un `exit_code 0` sin eventos. El broker convierte ese falso exito en fallo.
- Cada guardado crea un snapshot nuevo: **el undo del agente es gratis**. El diff y el rollback se
  construyen sobre datos que ya existen.
- Ruido de motor: buena parte de los `ERROR` del log son `zdev`/`getbot` (fallos del propio Rocketbot,
  no del robot). Sin clasificarlos, el panel esta siempre en rojo y se deja de mirar.

## Presupuesto de contexto

Medido sobre 28 robots reales:

| | tokens |
|---|---|
| JSON crudo de las 3 dbs | 102.052 (fase 0) |
| render de las mismas | 16.326 (fase 0, 4 secciones) |
| `ML_06_Legacy` (36 nodos, todas las secciones) | 1.128 (8,2x) |
| `reporteMercadolibre` (17 nodos, 86 vars) | 1.812 (5,6x) |

Objetivo de diseno: **un robot en <= 2,5k tokens**, para que un agente pueda tener el workspace
entero en contexto y diagnosticar sin pedir el flujo crudo.

### `RUTA_MIGRADA` comprueba el dispositivo, no solo el formato

`RUTA_ABSOLUTA` avisa de una ruta absoluta; `RUTA_MIGRADA` responde una pregunta
distinta y verificable: **¿esa ruta existe en este equipo?**. Es el caso de BBRE2:
`reporteMercadolibre` trae `C:/Users/josep/Downloads/ENTREGA_BBRE2/...` en sus
variables y esa carpeta no existe aqui.

- Se evalua **solo cuando el pack recibe un `PathProbe`**. El CLI y la fachada MCP
  pasan `LocalPaths`; el motor hermetico (tests, `review`, registro) usa `NoPaths`
  y el resultado es identico al historico. Asi el golden no depende del disco de
  la maquina que corre la suite.
- Mira **variables y parametros**. En los robots reales las rutas viven en
  variables; en un parametro literal la linea es la del nodo. Si la variable solo
  la usa un subrobot, la linea es la de la llamada `execRocketBotDB` en el padre:
  es donde se decide ejecutar ese contrato.
- No marca rutas que existen, rutas de sistema (`C:/Windows`, `C:/Program Files`,
  `C:/Rocketbot`, `/usr`...), URLs ni plantillas (`{base_path}`, `%USERPROFILE%`).
- Agrupa por **la primera carpeta que falta**: las diez variables de BBRE2 bajo
  `ENTREGA_BBRE2` producen un hallazgo, no diez. El remapeo busca una carpeta
  local con la misma hoja y puntua los componentes padre que coinciden; "sin
  equivalente local" tambien es un resultado valido.
- El `fix` del hallazgo propone la carpeta y recuerda que el cambio lo aplica el
  agente con `rb_plan_patch`/`rb_apply_patch`: la herramienta **nunca** escribe
  `robot.db`.
- La bandeja muestra la propuesta en el pack del robot seleccionado. Los
  contadores del barrido siguen siendo de reglas de codigo: `review` no comprueba
  el disco una vez por fila.

### El registro es el unico sitio donde se escribe

`robot.db` se abre siempre en solo lectura. Lo unico que la herramienta persiste es su propio
`problems.db`: firmas de problema normalizadas, avistamientos (con idempotencia: repetir un barrido no
infla los contadores) y fixes con el diff que los cerro. Es el activo que convierte sesiones aisladas
en aprendizaje acumulado; sin el, cada diagnostico empieza de cero.

Ruta: `ROCKETDEBUG_REGISTRY`, o `ROCKETDEBUG_HOME`, o el directorio de datos del usuario.

Los mensajes que el robot escribe con `log_message` **no** se registran: son del robot, no defectos, y
nadie los va a arreglar. El ruido del motor (`zdev`, `getbot`) tampoco.
### La hora de una observacion es la del snapshot, no la del barrido

El registro identifica una observacion por `(firma, robot, version, linea, instante)`. Si el instante
fuera el reloj del escaneo, cada pasada del vigilante contaria la misma version como un avistamiento
nuevo y `occurrences` dejaria de significar nada. Medido antes del arreglo: dos pasadas seguidas sobre
la misma db dejaban 164 observaciones donde la primera habia dejado 82. Ahora `db::latest_created_at`
da la hora del snapshot observado y dos pasadas son idempotentes (82 y 0 nuevas).
### La UI se sirve desde el mismo servidor, no desde una app aparte

`rocketdebug-mcp --http <puerto>` sirve tres cosas en `127.0.0.1`:

| ruta | para quien | que es |
|---|---|---|
| `POST /mcp` | el agente | JSON-RPC (MCP) |
| `GET /` | el humano | la pagina de bandeja de entrada, embebida en el binario |
| `GET /api/review` | la pagina | la misma agregacion que `rocketdebug review`, en JSON |
| `GET /api/pack?robot=…&db=…` | la pagina | el markdown de un robot |

Decision: **una sola pagina HTML sin build**, embebida con `include_str!`. Ni framework, ni bundler, ni
CDN: el binario es el producto y no puede depender de que alguien instale Node para ver la bandeja.
Tauri queda como paso posterior y solo tendria que apuntar a esa misma pagina.

La agregacion vive en `rocketdebug_core::review` y la usan **el CLI y la API**, asi que no pueden dar
numeros distintos. Medido: `review --workspace` y `/api/review` coinciden en 32 robots, 67 accionables,
76 higiene y 6 vivos.
### El agente ordena, el humano mira

`ui_render` devuelve un enlace como
`http://127.0.0.1:PUERTO/?robot=ML_06_Legacy&db=BBRE2%5Crobot_run.db`, y esa URL abre la bandeja con
el robot seleccionado y su pack cargado. Es la alternativa a pegar 2k tokens de contexto en el chat:
el agente apunta, la persona mira.

El servidor HTTP necesita URL conocida (`ROCKETDEBUG_UI_URL`) porque no tiene puerto por defecto. La
pagina toma `robot`/`db` de su propia URL, escribe la URL al pulsar una fila (enlaces compartibles) y
respeta atras/adelante.

Detalle de rendimiento: con el cache de logs parseados (`logs::parse_file_cached`, invalidado por
`mtime` y tamano) el barrido de `review` sobre 32 robots bajo de ~2 s a ~0,8 s sin cambiar una coma de
la salida (sha256 identico). Sin cache, la bandeja releia y reparaba los mismos ~20 ficheros una vez
por robot.

El otro O(N^2) del barrido era recargar los hechos de la db por robot. `analysis::Workspace` carga y
deduplica los hechos una vez y cada fila analiza su robot sobre esa misma foto. Medido con el binario
de depuracion sobre 16 robots de 4 dbs sin logs: **0,7 s -> 0,1 s**, con un test que compara campo a
campo el analisis compartido con el analisis individual de cada robot.

Ademas, los hechos leen solo la fila del snapshot que necesitan (`load_latest_snapshot` o
`load_snapshot`), no el historial entero: una version vieja corrupta ya no bloquea el analisis de la
ultima, y el coste deja de crecer con las versiones que no se van a mirar.
