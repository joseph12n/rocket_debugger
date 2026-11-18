# rocket_debugger

Depurador de flujos de Rocketbot RPA. Lee `robot.db` en solo-lectura, atribuye errores al
snapshot que estaba vivo cuando ocurrieron, y emite un **context pack** de ~2k tokens que un
agente puede leer de una vez en lugar del JSON crudo (10x menos contexto).

No es un IDE ni un editor visual. Es el motor de deteccion y el suministro de contexto.
**Todo cambio en un robot pasa por el agente**, nunca por esta herramienta.

## Estado

Fase 0 cerrada: prototipo validado sobre 28 robots reales en 3 dbs.

| Medido | Valor |
|---|---|
| Contexto crudo (JSON pretty) | 102.052 tokens |
| Contexto renderizado | 16.326 tokens |
| Ratio | 6,3x (1,8x en robots de 3 nodos, 10x en el de 32) |
| Defecto critico encontrado | 34 lineas duplicadas en 13 robots |

Ver [`docs/FINDINGS.md`](docs/FINDINGS.md) para el informe completo de la fase 0.

## Por que existe

Un robot de 300 comandos son ~250k tokens de JSON: no entra en ninguna ventana de contexto,
ni en la del agente ni en la de una persona. El 70% de ese JSON es metadata de ejecucion
(`img`, `screenshot`, `time`, `execute_debugg`) que no aporta nada para entender el flujo.

Y un error de hace tres versiones no es un error de hoy. Sin atribucion temporal, cualquier
panel miente sobre que esta roto.

## Uso

```bash
cargo run -p rocketdebug-cli -- list
cargo run -p rocketdebug-cli -- pack ML_06_Legacy --db "C:\rocketbot_bots\certificaciones\BBRE2\robot_run.db"
cargo run -p rocketdebug-cli -- pack reporteMercadolibre --stats
```

La ejecucion la pide el agente por MCP con `run_robot`: pasa por un broker (una ejecucion a la vez),
se niega si Studio esta abierto —el runner CLI delegaria sin ejecutar— y exige eventos del robot
antes de dar la ejecucion por buena. `run_status` muestra quien corre y quien espera. Requiere
`ROCKETBOT_EXE` apuntando a `rocketbot.exe` y **licencia de produccion**: con la licencia Studio
Develop el CLI sale con 0 sin ejecutar y el broker lo reporta como fallo, no como exito. El timeout
es 180 s por defecto (maximo 900).

## Arquitectura

```
robot.db (N dbs) â”€â”€â”
logs/*.log â”€â”€â”€â”€â”€â”€â”€â”€â”¤â†’ motor â”€â”€â”¬â†’ render markdown (el context pack)
catalogo comandos â”€â”˜          â”œâ†’ daemon local (HTTP/WS)
                              â””â†’ fachada MCP (intenciones, no datos)
```

Detalle en [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).
Esquema del context pack en [`docs/CONTEXT_PACK.md`](docs/CONTEXT_PACK.md).

## Convenciones

- Documentacion e issues en espanol. Codigo e identificadores en ingles.
- Decisiones de diseno que no son obvias se documentan en el codigo con el *por que*,
  no el *que*.
- Nada de dependencia de `C:\Rocketbot` en tests: los fixtures son sinteticos.
  Los tests de integracion contra datos reales van marcados `#[ignore]`.

## Build en Windows sin Visual Studio

Este equipo no tiene MSVC. El proyecto compila con el toolchain GNU y un GCC portatil
(WinLibs) para las dependencias nativas:

```powershell
rustup toolchain install stable-x86_64-pc-windows-gnu
# aÃ±adir el bin de mingw64 al PATH
cargo +stable-x86_64-pc-windows-gnu build
```

En CI (Linux) se usa el toolchain por defecto.

## Roadmap

- [x] #1 parser de `bots.data` e indice de versiones
- [x] #2 seccion de errores con atribucion temporal al snapshot vivo
- [x] #3 grafo de llamadas y contrato implicito de variables
- [x] #4 motor de reglas con hallazgos accionables e higiene separados
- [x] #5 render del context pack con contabilidad de tokens
- [x] #8 redaccion de credenciales cifradas y valores sensibles
- [x] #11 redaccion de rutas absolutas en el panel de variables
- [x] #6 fachada MCP de lectura para el agente
- [x] #12 workspace multi-db (el grafo ve todas las dbs)
- [x] #13 aviso de hallazgos omitidos por el tope
- [x] #16 enmascarado de rutas en los parametros de los comandos
- [x] #19 transporte HTTP, ui.render, registro de problemas y bandeja de entrada

## Plan 2 (2026-11)

Orden acordado: el broker primero; las rutas migradas habilitan el ciclo real sobre BBRE2.

- [ ] #25 exec: broker de ejecucion con cola, mutex y verificacion de eventos
- [ ] #26 analysis: regla RUTA_MIGRADA para rutas de otro dispositivo
- [ ] #27 ciclo: cierre end-to-end con un robot real no trivial (BBRE2)
- [ ] #18 deuda conocida del motor y la fachada, cada punto cerrado con su test

## Estado medido

`rocketdebug pack` sobre datos reales: 70 hallazgos accionables en 39 instancias de robot (2,7 por
robot afectado, todos verificables estructuralmente), 66 de higiene en seccion aparte, y packs de
1,1-1,8k tokens para robots de 17-36 nodos.
