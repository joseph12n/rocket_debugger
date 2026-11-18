# Primer ciclo completo cerrado

Fecha: 2026-11-17 · Robot: `DEMO_ciclo` (sandbox) · Entorno: Rocketbot 2026.04.07

Esto cierra el bucle que la fase 0 dejó abierto: **detectar → diagnosticar → arreglar → verificar**.
Hasta ahora el motor encontraba defectos pero no se había arreglado ninguno con él.

## El robot

Tres comandos, un defecto real y reproducible:

```
1 log_message  INICIO DEMO_ciclo
2 setVar → contador   contador + 1     ← se asigna a sí misma sin existir
3 log_message  FIN DEMO_ciclo
```

## Los cinco pasos, con su evidencia

### 1. Ejecutar y capturar el fallo

```
mode=studio → ok:false
steps: [L1 True, L2 False "NameError: name 'contador' is not defined", L3 True]
errors: [{line: 2, father: setVar, handled: false}]
```

### 2. Diagnosticar con el pack, sin leer el flujo crudo

`flow_pack` vía MCP → **193 tokens**:

```
## Errores (vivos 1 · payload cambiado 0 · historicos 0 · modulo 0 · ruido 0)
- x1 L2 setVar: EXCEPTION IN (<string> system, L_253 ""): NameError: name 'contador' is not defined

## Flujo
2 setVar → contador  contador + 1
```

El pack pone el error y la línea culpable en la misma pantalla. El diagnóstico no requiere
leer el JSON del robot: la línea 2 se asigna a sí misma una variable que aún no existe.

### 3. Arreglar

Una sola edición: `update_command target=2 changes={command: "1"}` → snapshot 60.

### 4. Verificar

```
mode=studio → ok:true
steps: [L1 True, L2 True, L3 True]   errors: []
```

### 5. Confirmar que el pack ya no miente

`flow_pack` otra vez → **176 tokens**:

```
# DEMO_ciclo  (snapshot 60, 5 versiones)
## Errores (vivos 0 · payload cambiado 0 · historicos 1 · modulo 0 · ruido 0)
- historicos de versiones anteriores: setVar(x1)
```

**Esto es el diferenciador.** El error sigue en el log, pero el pack lo atribuye al snapshot 59
—el que estaba vivo cuando ocurrió— y no lo presenta como un fallo actual. Un panel sin atribución
temporal habría seguido enseñando el error en rojo para siempre.

## Lo que este ciclo demuestra y lo que no

Demuestra:

- Que el pack basta para diagnosticar: no hizo falta abrir el robot ni el JSON.
- Que la atribución temporal clasifica bien en un caso real de arreglo: `1 vivo` → `1 histórico`.
- Que el bucle completo funciona con las piezas actuales (MCP + motor + editor + runner).

No demuestra:

- **Que sea más rápido que leer el flujo en crudo.** Este robot son 3 nodos: el pack costó 193
  tokens y el JSON crudo no habría costado mucho más. La ventaja aparece a partir de ~20 nodos,
  donde el pack está medido en 1,1-1,8k tokens y el JSON en 9-18k. Falta medirlo con un robot de
  cliente mediano-grande, que hoy no hay en el entorno.
- Que el agente haya acertado por su cuenta: el diagnóstico era trivial a propósito. La prueba
  interesante es un defecto donde la causa no esté a la vista (por ejemplo el `savexlsx` de
  `reporteMercadolibre`, cuya causa es una forma de parámetro, no una línea sospechosa).

## Hallazgo operativo: Studio abierto rompe la ejecución por CLI

La primera ejecución devolvió `exit_code: 0` **sin haber ejecutado nada**:

```
aviso: "Studio estaba abierto: rocketbot.exe puede haber delegado en la instancia existente
        sin ejecutar el robot. Para ejecucion por CLI cierra Studio, o usa el modo API."
```

`eventos_del_robot: 0`. Es exactamente el problema que la fase 0 anticipó al decir que la app debe
ser **broker de ejecución**: con `max_executions: 1` y Studio abierto, el runner CLI delega y no
ejecuta, sin devolver error. Un agente que se fíe del `exit_code` concluirá que el robot funciona.

Consecuencia de diseño, ya registrada: la app tiene que tener el mutex de ejecución y no fiarse nunca
de un `exit_code` sin comprobar que hubo eventos del robot.
