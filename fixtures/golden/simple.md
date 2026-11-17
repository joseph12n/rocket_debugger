# simple  (snapshot 1, 1 versiones)
> Fixture minimo del motor
db: test\robot.db | nodos 3 | vars 1 | prof 1

## Higiene (1: 0 alta / 1 media / 0 baja)
- [media] L- SIN_TRY — el robot no tiene ningun trycatch: un error corta el flujo sin registro ni recuperacion → envuelve el bloque principal en un trycatch y registra el error en el else

## Variables (1)
- saludo = (sin valor inicial)

## Flujo
1 log_message  INICIO simple
2 setVar → saludo  "hola"
3 log_message  {saludo}
