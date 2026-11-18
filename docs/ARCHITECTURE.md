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
| MCP | `rocketdebug-mcp` | fachada para el agente por stdio y HTTP (8 tools) |
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

Un defecto con linea concreta (`LINEA_COLISION`, `RUTA_ABSOLUTA`) se puede atender hoy; una carencia
del robot entero (`SIN_TRY`, `IF_SIN_ELSE`, `ANIDAMIENTO`, `VAR_NO_USADA`) es cierta pero no urgente.
Mezcladas competian por el mismo tope y la segunda diluia a la primera: medido, las genericas eran el
36% de lo mostrado. Ahora el render las separa (`## Hallazgos` / `## Higiene`) y cada cubo tiene su
tope. En el barrido real: 70 accionables, 66 de higiene, 0 ocultos.

Regla de clasificacion: **accionable = senala una linea con un defecto concreto**. Se declara por
regla en `rules/builtin.json`, no se infiere de `Finding::line` (`IF_SIN_ELSE` trae linea y es higiene).

### El motor no lee logs por su cuenta

`build_from_conn` es puro: no toca el sistema de ficheros y deja la seccion de errores vacia. El CLI
usa `build_from_conn_with_logs` y decide el directorio. Sin esa separacion, un test cuyo robot se
llamara igual que uno real cambiaria de resultado segun la maquina que lo ejecute, y el golden del
render dejaria de ser un golden.

## Restricciones del entorno real

- `max_executions: 1` en la licencia y un unico ChromeDriver. La app debe ser **broker de ejecucion**
  (cola + mutex), no un espectador: si el agente ejecuta mientras Studio esta abierto, el resultado es
  `SessionNotCreatedException: Chrome failed to start: crashed`.
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
